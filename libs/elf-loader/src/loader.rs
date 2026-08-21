//! Поиск зависимостей, отображение сегментов и динамическая линковка.

use core::{ptr, str};

use rustos_abi::{memory::MEMORY_ABI_VERSION, syscall, Handle, VmFlags, PAGE_SIZE};
use rustos_runtime::{
    handle_close, shared_memory_create, shared_memory_map, shared_memory_seal, vm_map, vm_protect,
    vm_unmap, SharedMemoryCreate, SharedMemoryMap, VmMapRequest,
};

use crate::elf::{align_down, align_up, ElfError, ElfView, ProgramFlags, Symbol};

const MAX_MODULES: usize = 8;
const MAX_REGIONS_PER_MODULE: usize = 8;
const PATH_BYTES: usize = 256;
const MODULE_ARENA_BASE: u64 = 0x0000_5800_0000_0000;
const MODULE_STRIDE: u64 = 32 * 1024 * 1024;

#[cfg(target_arch = "x86_64")]
mod relocation {
    pub const NONE: u32 = 0;
    pub const ABSOLUTE_64: u32 = 1;
    pub const PC32: u32 = 2;
    pub const GLOB_DAT: u32 = 6;
    pub const JUMP_SLOT: u32 = 7;
    pub const RELATIVE: u32 = 8;
    pub const ABSOLUTE_32: u32 = 10;
    pub const ABSOLUTE_32S: u32 = 11;
    pub const DTPMOD64: u32 = 16;
    pub const DTPOFF64: u32 = 17;
    pub const TPOFF64: u32 = 18;
}

#[cfg(target_arch = "aarch64")]
mod relocation {
    pub const NONE: u32 = 0;
    pub const ABSOLUTE_64: u32 = 257;
    pub const GLOB_DAT: u32 = 1025;
    pub const JUMP_SLOT: u32 = 1026;
    pub const RELATIVE: u32 = 1027;
    pub const DTPMOD64: u32 = 1028;
    pub const DTPOFF64: u32 = 1029;
    pub const TPOFF64: u32 = 1030;
    pub const PC32: u32 = u32::MAX;
    pub const ABSOLUTE_32: u32 = u32::MAX - 1;
    pub const ABSOLUTE_32S: u32 = u32::MAX - 2;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadError {
    Elf(ElfError),
    RootNotFound,
    DependencyNotFound,
    TooManyModules,
    DuplicateModule,
    ModuleTooLarge,
    TooManyMappings,
    Memory(i64),
    MissingSymbol,
    UnsupportedRelocation(u32),
    TextRelocation,
    IntegerOverflow,
    InvalidTls,
    InvalidRelro,
    InvalidPath,
}

impl From<ElfError> for LoadError {
    fn from(error: ElfError) -> Self {
        Self::Elf(error)
    }
}

/// Источник неизменяемых ELF images. Реальная реализация читает файлы через
/// `vfs-1.dll`; тестовая может отдать заранее загруженные byte slices.
pub trait ModuleSource {
    fn open<'a>(&'a self, path: &str) -> Option<&'a [u8]>;
}

/// Детерминированный порядок поиска `DT_NEEDED`.
#[derive(Clone, Copy)]
pub struct SearchPolicy<'a> {
    /// Каталог основного приложения.
    pub application_dir: &'a str,
    /// Опциональный приватный каталог bundle (`/apps/id/lib`).
    pub private_library_dir: Option<&'a str>,
    /// Системный ABI, обычно `/system/lib`.
    pub system_library_dir: &'a str,
}

/// Абстракция VM оставляет parser тестируемым на host и содержит весь unsafe
/// код реальной записи в user mappings в одном маленьком месте.
pub trait Memory {
    fn map_private(&mut self, address: u64, length: u64, flags: VmFlags) -> Result<(), i64>;
    fn create_shared_rw(&mut self, length: u64) -> Result<Handle, i64>;
    fn map_shared(
        &mut self,
        handle: Handle,
        address: u64,
        length: u64,
        flags: VmFlags,
    ) -> Result<u64, i64>;
    fn seal_shared(&mut self, handle: Handle, flags: VmFlags) -> Result<(), i64>;
    fn protect(&mut self, address: u64, length: u64, flags: VmFlags) -> Result<(), i64>;
    fn unmap(&mut self, address: u64, length: u64) -> Result<(), i64>;
    fn close(&mut self, handle: Handle);
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), i64>;
}

/// Реализация поверх memory ABI RustOS.
pub struct RuntimeMemory;

impl Memory for RuntimeMemory {
    fn map_private(&mut self, address: u64, length: u64, flags: VmFlags) -> Result<(), i64> {
        let request = VmMapRequest {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            address,
            length,
            flags,
        };
        let result = vm_map(&request);
        if result == address as i64 {
            Ok(())
        } else {
            Err(result)
        }
    }

    fn create_shared_rw(&mut self, length: u64) -> Result<Handle, i64> {
        let request = SharedMemoryCreate {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            length,
            flags: VmFlags::READ.union(VmFlags::WRITE),
        };
        let result = shared_memory_create(&request);
        if result > 0 {
            Ok(Handle(result as u32))
        } else {
            Err(result)
        }
    }

    fn map_shared(
        &mut self,
        handle: Handle,
        address: u64,
        length: u64,
        flags: VmFlags,
    ) -> Result<u64, i64> {
        let request = SharedMemoryMap {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            address,
            offset: 0,
            length,
            flags,
        };
        let result = shared_memory_map(handle, &request);
        if result > 0 {
            Ok(result as u64)
        } else {
            Err(result)
        }
    }

    fn seal_shared(&mut self, handle: Handle, flags: VmFlags) -> Result<(), i64> {
        status_result(shared_memory_seal(handle, flags))
    }

    fn protect(&mut self, address: u64, length: u64, flags: VmFlags) -> Result<(), i64> {
        status_result(vm_protect(address, length, flags))
    }

    fn unmap(&mut self, address: u64, length: u64) -> Result<(), i64> {
        status_result(vm_unmap(address, length))
    }

    fn close(&mut self, handle: Handle) {
        let _ = handle_close(handle);
    }

    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), i64> {
        // SAFETY: адрес был возвращён/проверен memory ABI, а caller loader'а
        // разрешает запись только в текущее RW mapping.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len()) };
        Ok(())
    }
}

fn status_result(result: i64) -> Result<(), i64> {
    if result == syscall::status::OK {
        Ok(())
    } else {
        Err(result)
    }
}

#[derive(Clone, Copy, Debug)]
struct Region {
    address: u64,
    length: u64,
    flags: VmFlags,
    shared: Handle,
}

const EMPTY_REGION: Region = Region {
    address: 0,
    length: 0,
    flags: VmFlags(0),
    shared: Handle::INVALID,
};

#[derive(Clone, Copy)]
struct Module<'a> {
    view: ElfView<'a>,
    load_bias: u64,
    tls_id: u64,
    tls_end_offset: u64,
    regions: [Region; MAX_REGIONS_PER_MODULE],
    region_count: usize,
}

/// Информация о физически разделяемом сегменте DLL. Capability можно
/// attenuate до `READ | EXECUTE | MAP | TRANSFER` и передать другому процессу.
#[derive(Clone, Copy, Debug)]
pub struct SharedRegion {
    pub handle: Handle,
    pub address: u64,
    pub length: u64,
    pub flags: VmFlags,
}

/// Variant-II static TLS: thread pointer расположен сразу после блока.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlsLayout {
    /// Суммарный объём TLS templates/BSS до TCB.
    pub size: u64,
    pub alignment: u64,
    /// Требуемый backing buffer с минимальным x86-64 TCB.
    pub storage_size: u64,
}

/// Результат загрузки основного PIE.
#[derive(Clone, Copy, Debug)]
pub struct LoadedProgram {
    pub entry: u64,
    pub modules: usize,
    pub tls: TlsLayout,
    pub relro_pages: usize,
    pub shared_pages: usize,
}

/// Bounded user-space linker. Объект живёт, пока нужны mappings и shared
/// capabilities; `unload` освобождает их в обратном порядке.
pub struct DynamicLoader<'a, S: ModuleSource> {
    source: &'a S,
    search: SearchPolicy<'a>,
    modules: [Option<Module<'a>>; MAX_MODULES],
    module_count: usize,
    tls: TlsLayout,
    relro_pages: usize,
    shared_pages: usize,
}

impl<'a, S: ModuleSource> DynamicLoader<'a, S> {
    pub const fn new(source: &'a S, search: SearchPolicy<'a>) -> Self {
        Self {
            source,
            search,
            modules: [None; MAX_MODULES],
            module_count: 0,
            tls: TlsLayout {
                size: 0,
                alignment: 1,
                storage_size: 16,
            },
            relro_pages: 0,
            shared_pages: 0,
        }
    }

    /// Загружает root и транзитивный closure `DT_NEEDED`, применяет eager
    /// relocations и только затем включает RELRO.
    pub fn load<M: Memory>(
        &mut self,
        root_path: &str,
        memory: &mut M,
    ) -> Result<LoadedProgram, LoadError> {
        if self.module_count != 0 {
            return Err(LoadError::DuplicateModule);
        }
        let root = self.source.open(root_path).ok_or(LoadError::RootNotFound)?;
        self.add_module(root)?;
        if let Err(error) = self.discover_dependencies().and_then(|_| self.assign_tls()) {
            self.unload(memory);
            return Err(error);
        }

        let result = self
            .map_modules(memory)
            .and_then(|_| self.relocate_modules(memory))
            .and_then(|_| self.apply_relro(memory));
        if let Err(error) = result {
            self.unload(memory);
            return Err(error);
        }
        let Some(root) = self.modules[0] else {
            self.unload(memory);
            return Err(LoadError::RootNotFound);
        };
        // У PIE executable есть e_entry, у DLL он закономерно равен нулю.
        let entry = if root.view.entry() == 0 {
            0
        } else {
            let Some(entry) = root.load_bias.checked_add(root.view.entry()) else {
                self.unload(memory);
                return Err(LoadError::IntegerOverflow);
            };
            if !root.view.address_is_executable(root.view.entry()) {
                self.unload(memory);
                return Err(LoadError::Elf(ElfError::InvalidProgramHeader));
            }
            entry
        };
        Ok(LoadedProgram {
            entry,
            modules: self.module_count,
            tls: self.tls,
            relro_pages: self.relro_pages,
            shared_pages: self.shared_pages,
        })
    }

    /// Ищет публичный symbol в стандартном global scope: root, затем BFS
    /// dependencies. Weak symbol используется только если strong не найден.
    pub fn symbol(&self, name: &str) -> Result<u64, LoadError> {
        self.find_global_symbol(name)
            .map(|definition| definition.address)
            .ok_or(LoadError::MissingSymbol)
    }

    pub fn shared_executable_region(&self, soname: &str) -> Option<SharedRegion> {
        for module in self.modules.iter().take(self.module_count).flatten() {
            if module.view.soname().ok().flatten() != Some(soname) {
                continue;
            }
            for region in module.regions.iter().take(module.region_count) {
                if region.shared.is_valid() && region.flags.contains(VmFlags::EXECUTE) {
                    return Some(SharedRegion {
                        handle: region.shared,
                        address: region.address,
                        length: region.length,
                        flags: region.flags,
                    });
                }
            }
        }
        None
    }

    /// Копирует TLS templates и возвращает thread pointer. `storage` должен
    /// оставаться отображённым всё время жизни потока.
    pub fn initialize_tls(&self, storage: &mut [u8]) -> Result<u64, LoadError> {
        let storage_size =
            usize::try_from(self.tls.storage_size).map_err(|_| LoadError::InvalidTls)?;
        if storage.len() < storage_size {
            return Err(LoadError::InvalidTls);
        }
        storage[..storage_size].fill(0);
        let base = storage.as_ptr() as u64;
        let thread_pointer = align_up(
            base.checked_add(self.tls.size)
                .ok_or(LoadError::InvalidTls)?,
            self.tls.alignment,
        )
        .ok_or(LoadError::InvalidTls)?;
        let data_start = thread_pointer
            .checked_sub(self.tls.size)
            .and_then(|address| address.checked_sub(base))
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(LoadError::InvalidTls)?;
        for module in self.modules.iter().take(self.module_count).flatten() {
            let Some(tls) = module.view.tls() else {
                continue;
            };
            let start = data_start
                + self
                    .tls
                    .size
                    .checked_sub(module.tls_end_offset)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(LoadError::InvalidTls)?;
            let file_start = usize::try_from(tls.offset).map_err(|_| LoadError::InvalidTls)?;
            let file_size = usize::try_from(tls.file_size).map_err(|_| LoadError::InvalidTls)?;
            let source = module
                .view
                .image()
                .get(file_start..file_start + file_size)
                .ok_or(LoadError::InvalidTls)?;
            storage[start..start + file_size].copy_from_slice(source);
        }
        // SysV x86-64 variant-II code читает `%fs:0` как self pointer TCB.
        let tcb = usize::try_from(thread_pointer - base).map_err(|_| LoadError::InvalidTls)?;
        storage[tcb..tcb + 8].copy_from_slice(&thread_pointer.to_le_bytes());
        Ok(thread_pointer)
    }

    pub fn unload<M: Memory>(&mut self, memory: &mut M) {
        for module_index in (0..self.module_count).rev() {
            if let Some(module) = self.modules[module_index].as_mut() {
                for region_index in (0..module.region_count).rev() {
                    let region = module.regions[region_index];
                    let _ = memory.unmap(region.address, region.length);
                    if region.shared.is_valid() {
                        memory.close(region.shared);
                    }
                    module.regions[region_index] = EMPTY_REGION;
                }
                module.region_count = 0;
            }
            self.modules[module_index] = None;
        }
        self.module_count = 0;
        self.tls = TlsLayout {
            size: 0,
            alignment: 1,
            storage_size: 16,
        };
        self.relro_pages = 0;
        self.shared_pages = 0;
    }

    fn add_module(&mut self, image: &'a [u8]) -> Result<usize, LoadError> {
        if self.module_count == MAX_MODULES {
            return Err(LoadError::TooManyModules);
        }
        let view = ElfView::parse(image)?;
        if let Some(soname) = view.soname()? {
            if self
                .modules
                .iter()
                .take(self.module_count)
                .flatten()
                .any(|module| module.view.soname().ok().flatten() == Some(soname))
            {
                return Err(LoadError::DuplicateModule);
            }
        }
        let minimum = view.minimum_page(PAGE_SIZE);
        let maximum = view.maximum_page(PAGE_SIZE)?;
        if maximum.saturating_sub(minimum) > MODULE_STRIDE {
            return Err(LoadError::ModuleTooLarge);
        }
        let slot_base = MODULE_ARENA_BASE
            .checked_add(self.module_count as u64 * MODULE_STRIDE)
            .ok_or(LoadError::IntegerOverflow)?;
        let load_bias = slot_base
            .checked_sub(minimum)
            .ok_or(LoadError::IntegerOverflow)?;
        let index = self.module_count;
        self.modules[index] = Some(Module {
            view,
            load_bias,
            tls_id: index as u64 + 1,
            tls_end_offset: 0,
            regions: [EMPTY_REGION; MAX_REGIONS_PER_MODULE],
            region_count: 0,
        });
        self.module_count += 1;
        Ok(index)
    }

    fn discover_dependencies(&mut self) -> Result<(), LoadError> {
        let mut cursor = 0usize;
        while cursor < self.module_count {
            let view = self.modules[cursor].ok_or(LoadError::RootNotFound)?.view;
            for needed_index in 0..view.needed_count() {
                let needed = view.needed(needed_index)?;
                if self.module_named(needed) {
                    continue;
                }
                let image = self
                    .find_dependency(needed)
                    .ok_or(LoadError::DependencyNotFound)?;
                self.add_module(image)?;
            }
            cursor += 1;
        }
        Ok(())
    }

    fn module_named(&self, name: &str) -> bool {
        self.modules
            .iter()
            .take(self.module_count)
            .flatten()
            .any(|module| module.view.soname().ok().flatten() == Some(name))
    }

    fn find_dependency(&self, name: &str) -> Option<&'a [u8]> {
        if name.is_empty() || name.as_bytes().contains(&b'/') {
            return None;
        }
        let directories = [
            Some(self.search.application_dir),
            self.search.private_library_dir,
            Some(self.search.system_library_dir),
        ];
        for directory in directories.into_iter().flatten() {
            let mut path = PathBuffer::new();
            if path.join(directory, name).is_ok() {
                if let Some(image) = self.source.open(path.as_str()) {
                    return Some(image);
                }
            }
        }
        None
    }

    fn assign_tls(&mut self) -> Result<(), LoadError> {
        let mut used = 0u64;
        let mut maximum_alignment = 1u64;
        for module in self.modules.iter_mut().take(self.module_count).flatten() {
            let Some(tls) = module.view.tls() else {
                continue;
            };
            if !tls.alignment.is_power_of_two() || tls.file_size > tls.memory_size {
                return Err(LoadError::InvalidTls);
            }
            used = used
                .checked_add(tls.memory_size)
                .and_then(|value| align_up(value, tls.alignment))
                .ok_or(LoadError::IntegerOverflow)?;
            module.tls_end_offset = used;
            maximum_alignment = maximum_alignment.max(tls.alignment);
        }
        self.tls = TlsLayout {
            size: align_up(used, maximum_alignment).ok_or(LoadError::IntegerOverflow)?,
            alignment: maximum_alignment,
            storage_size: align_up(used, maximum_alignment)
                .and_then(|size| size.checked_add(maximum_alignment - 1))
                .and_then(|size| size.checked_add(16))
                .ok_or(LoadError::IntegerOverflow)?,
        };
        Ok(())
    }

    fn map_modules<M: Memory>(&mut self, memory: &mut M) -> Result<(), LoadError> {
        for module_index in 0..self.module_count {
            let module = self.modules[module_index].ok_or(LoadError::RootNotFound)?;
            for segment in module.view.segments() {
                if segment.memory_size == 0 {
                    continue;
                }
                let page_start = align_down(segment.virtual_address, PAGE_SIZE);
                let page_end = align_up(
                    segment
                        .virtual_address
                        .checked_add(segment.memory_size)
                        .ok_or(LoadError::IntegerOverflow)?,
                    PAGE_SIZE,
                )
                .ok_or(LoadError::IntegerOverflow)?;
                let address = module
                    .load_bias
                    .checked_add(page_start)
                    .ok_or(LoadError::IntegerOverflow)?;
                let length = page_end - page_start;
                let flags = vm_flags(segment.flags);
                if segment.flags.contains(ProgramFlags::WRITE) {
                    memory
                        .map_private(address, length, VmFlags::READ.union(VmFlags::WRITE))
                        .map_err(LoadError::Memory)?;
                    self.record_region(
                        module_index,
                        Region {
                            address,
                            length,
                            flags: VmFlags::READ.union(VmFlags::WRITE),
                            shared: Handle::INVALID,
                        },
                    )?;
                    self.copy_segment(memory, module, *segment, address, page_start)?;
                } else {
                    self.map_shared_segment(
                        memory,
                        module_index,
                        module,
                        *segment,
                        address,
                        page_start,
                        length,
                        flags,
                    )?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn map_shared_segment<M: Memory>(
        &mut self,
        memory: &mut M,
        module_index: usize,
        module: Module<'a>,
        segment: crate::elf::Segment,
        final_address: u64,
        page_start: u64,
        length: u64,
        final_flags: VmFlags,
    ) -> Result<(), LoadError> {
        let handle = memory.create_shared_rw(length).map_err(LoadError::Memory)?;
        let temporary =
            match memory.map_shared(handle, 0, length, VmFlags::READ.union(VmFlags::WRITE)) {
                Ok(address) => address,
                Err(error) => {
                    memory.close(handle);
                    return Err(LoadError::Memory(error));
                }
            };
        if let Err(error) = self.copy_segment(memory, module, segment, temporary, page_start) {
            let _ = memory.unmap(temporary, length);
            memory.close(handle);
            return Err(error);
        }
        if let Err(error) = memory.unmap(temporary, length) {
            memory.close(handle);
            return Err(LoadError::Memory(error));
        }
        if let Err(error) = memory.seal_shared(handle, final_flags) {
            memory.close(handle);
            return Err(LoadError::Memory(error));
        }
        match memory.map_shared(handle, final_address, length, final_flags) {
            Ok(address) if address == final_address => {}
            Ok(address) => {
                let _ = memory.unmap(address, length);
                memory.close(handle);
                return Err(LoadError::Memory(syscall::status::INVALID_ARGUMENT));
            }
            Err(error) => {
                memory.close(handle);
                return Err(LoadError::Memory(error));
            }
        }
        self.record_region(
            module_index,
            Region {
                address: final_address,
                length,
                flags: final_flags,
                shared: handle,
            },
        )?;
        self.shared_pages += (length / PAGE_SIZE) as usize;
        Ok(())
    }

    fn copy_segment<M: Memory>(
        &self,
        memory: &mut M,
        module: Module<'a>,
        segment: crate::elf::Segment,
        mapping_address: u64,
        page_start: u64,
    ) -> Result<(), LoadError> {
        let file_start = usize::try_from(segment.offset).map_err(|_| LoadError::IntegerOverflow)?;
        let file_size =
            usize::try_from(segment.file_size).map_err(|_| LoadError::IntegerOverflow)?;
        let source = module
            .view
            .image()
            .get(file_start..file_start + file_size)
            .ok_or(LoadError::Elf(ElfError::InvalidProgramHeader))?;
        let target = mapping_address
            .checked_add(segment.virtual_address - page_start)
            .ok_or(LoadError::IntegerOverflow)?;
        memory.write(target, source).map_err(LoadError::Memory)
    }

    fn record_region(&mut self, module_index: usize, region: Region) -> Result<(), LoadError> {
        let module = self.modules[module_index]
            .as_mut()
            .ok_or(LoadError::RootNotFound)?;
        if module.region_count == MAX_REGIONS_PER_MODULE {
            return Err(LoadError::TooManyMappings);
        }
        module.regions[module.region_count] = region;
        module.region_count += 1;
        Ok(())
    }

    fn relocate_modules<M: Memory>(&self, memory: &mut M) -> Result<(), LoadError> {
        for module_index in 0..self.module_count {
            let module = self.modules[module_index].ok_or(LoadError::RootNotFound)?;
            for relocation_index in 0..module.view.relocation_count()? {
                let relocation = module.view.relocation(relocation_index)?;
                if relocation.kind == relocation::NONE {
                    continue;
                }
                let target_rva = relocation.offset;
                if !module.view.address_is_writable(target_rva, 8) {
                    return Err(LoadError::TextRelocation);
                }
                let target = module
                    .load_bias
                    .checked_add(target_rva)
                    .ok_or(LoadError::IntegerOverflow)?;
                let place = target;
                match relocation.kind {
                    relocation::RELATIVE => {
                        let value = add_signed(module.load_bias, relocation.addend)?;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    relocation::ABSOLUTE_64 | relocation::GLOB_DAT | relocation::JUMP_SLOT => {
                        let definition =
                            self.resolve_relocation_symbol(module_index, relocation.symbol)?;
                        let value = add_signed(definition.address, relocation.addend)?;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    relocation::PC32 => {
                        let definition =
                            self.resolve_relocation_symbol(module_index, relocation.symbol)?;
                        let value = i128::from(definition.address) + i128::from(relocation.addend)
                            - i128::from(place);
                        let value = i32::try_from(value).map_err(|_| LoadError::IntegerOverflow)?;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    relocation::ABSOLUTE_32 => {
                        let definition =
                            self.resolve_relocation_symbol(module_index, relocation.symbol)?;
                        let value = add_signed(definition.address, relocation.addend)?;
                        let value = u32::try_from(value).map_err(|_| LoadError::IntegerOverflow)?;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    relocation::ABSOLUTE_32S => {
                        let definition =
                            self.resolve_relocation_symbol(module_index, relocation.symbol)?;
                        let value = i128::from(definition.address) + i128::from(relocation.addend);
                        let value = i32::try_from(value).map_err(|_| LoadError::IntegerOverflow)?;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    relocation::DTPMOD64 => {
                        let tls_module = if relocation.symbol == 0 {
                            module_index
                        } else {
                            self.resolve_relocation_symbol(module_index, relocation.symbol)?
                                .module
                        };
                        let value = self.modules[tls_module]
                            .ok_or(LoadError::InvalidTls)?
                            .tls_id;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    relocation::DTPOFF64 | relocation::TPOFF64 => {
                        let (definition_module, symbol_offset) = if relocation.symbol == 0 {
                            // Local-exec codegen encodes the TLS-template
                            // offset directly in RELA addend.
                            (module_index, 0)
                        } else {
                            let definition =
                                self.resolve_relocation_symbol(module_index, relocation.symbol)?;
                            let tls_module =
                                self.modules[definition.module].ok_or(LoadError::InvalidTls)?;
                            let symbol = tls_module.view.symbol(definition.symbol)?;
                            let tls = tls_module.view.tls().ok_or(LoadError::InvalidTls)?;
                            (
                                definition.module,
                                symbol
                                    .value
                                    .checked_sub(tls.virtual_address)
                                    .ok_or(LoadError::InvalidTls)?,
                            )
                        };
                        let tls_module =
                            self.modules[definition_module].ok_or(LoadError::InvalidTls)?;
                        tls_module.view.tls().ok_or(LoadError::InvalidTls)?;
                        let value = if relocation.kind == relocation::DTPOFF64 {
                            i128::from(symbol_offset) + i128::from(relocation.addend)
                        } else {
                            i128::from(symbol_offset) - i128::from(tls_module.tls_end_offset)
                                + i128::from(relocation.addend)
                        };
                        let value = i64::try_from(value).map_err(|_| LoadError::IntegerOverflow)?;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    kind => return Err(LoadError::UnsupportedRelocation(kind)),
                }
            }
        }
        Ok(())
    }

    fn apply_relro<M: Memory>(&mut self, memory: &mut M) -> Result<(), LoadError> {
        for module in self.modules.iter().take(self.module_count).flatten() {
            let Some(relro) = module.view.relro() else {
                continue;
            };
            let start = align_down(
                module
                    .load_bias
                    .checked_add(relro.virtual_address)
                    .ok_or(LoadError::IntegerOverflow)?,
                PAGE_SIZE,
            );
            let end = align_up(
                module
                    .load_bias
                    .checked_add(relro.virtual_address)
                    .and_then(|value| value.checked_add(relro.memory_size))
                    .ok_or(LoadError::IntegerOverflow)?,
                PAGE_SIZE,
            )
            .ok_or(LoadError::IntegerOverflow)?;
            if end <= start {
                return Err(LoadError::InvalidRelro);
            }
            memory
                .protect(start, end - start, VmFlags::READ)
                .map_err(LoadError::Memory)?;
            self.relro_pages += ((end - start) / PAGE_SIZE) as usize;
        }
        Ok(())
    }

    fn resolve_relocation_symbol(
        &self,
        requester: usize,
        symbol_index: u32,
    ) -> Result<SymbolDefinition, LoadError> {
        let module = self.modules[requester].ok_or(LoadError::MissingSymbol)?;
        let symbol = module.view.symbol(symbol_index)?;
        let binding = symbol.binding();
        let visibility = symbol.visibility & 3;
        if symbol.is_defined() && (binding == 0 || visibility == 3) {
            return self.local_definition(requester, symbol_index, symbol);
        }
        let name = module.view.symbol_name(symbol)?;
        if let Some(definition) = self.find_global_symbol(name) {
            return Ok(definition);
        }
        if binding == 2 {
            // Undefined weak symbol имеет нулевое значение по ELF ABI.
            return Ok(SymbolDefinition {
                module: requester,
                symbol: symbol_index,
                address: 0,
            });
        }
        Err(LoadError::MissingSymbol)
    }

    fn find_global_symbol(&self, name: &str) -> Option<SymbolDefinition> {
        let mut weak = None;
        for (module_index, module) in self.modules.iter().take(self.module_count).enumerate() {
            let module = module.as_ref()?;
            for symbol_index in 1..module.view.symbol_count() {
                let symbol = module.view.symbol(symbol_index).ok()?;
                if !symbol.is_defined()
                    || !matches!(symbol.binding(), 1 | 2)
                    || module.view.symbol_name(symbol).ok()? != name
                {
                    continue;
                }
                let definition = self
                    .local_definition(module_index, symbol_index, symbol)
                    .ok()?;
                if symbol.binding() == 1 {
                    return Some(definition);
                }
                weak = weak.or(Some(definition));
            }
        }
        weak
    }

    fn local_definition(
        &self,
        module_index: usize,
        symbol_index: u32,
        symbol: Symbol,
    ) -> Result<SymbolDefinition, LoadError> {
        let module = self.modules[module_index].ok_or(LoadError::MissingSymbol)?;
        let address = if symbol.kind() == 6 {
            // TLS symbol не имеет обычного process virtual address; числовое
            // поле здесь не используется TLS relocation handlers.
            0
        } else {
            module
                .load_bias
                .checked_add(symbol.value)
                .ok_or(LoadError::IntegerOverflow)?
        };
        Ok(SymbolDefinition {
            module: module_index,
            symbol: symbol_index,
            address,
        })
    }
}

#[derive(Clone, Copy)]
struct SymbolDefinition {
    module: usize,
    symbol: u32,
    address: u64,
}

struct PathBuffer {
    bytes: [u8; PATH_BYTES],
    length: usize,
}

impl PathBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; PATH_BYTES],
            length: 0,
        }
    }

    fn join(&mut self, directory: &str, name: &str) -> Result<(), LoadError> {
        let directory = directory.trim_end_matches('/');
        let length = directory
            .len()
            .checked_add(1)
            .and_then(|value| value.checked_add(name.len()))
            .ok_or(LoadError::InvalidPath)?;
        if directory.is_empty() || length > PATH_BYTES {
            return Err(LoadError::InvalidPath);
        }
        self.bytes[..directory.len()].copy_from_slice(directory.as_bytes());
        self.bytes[directory.len()] = b'/';
        self.bytes[directory.len() + 1..length].copy_from_slice(name.as_bytes());
        self.length = length;
        Ok(())
    }

    fn as_str(&self) -> &str {
        // Оба компонента уже были `str`, между ними добавлен ASCII slash.
        unsafe { str::from_utf8_unchecked(&self.bytes[..self.length]) }
    }
}

fn vm_flags(flags: ProgramFlags) -> VmFlags {
    let mut result = VmFlags::READ;
    if flags.contains(ProgramFlags::WRITE) {
        result = result.union(VmFlags::WRITE);
    }
    if flags.contains(ProgramFlags::EXECUTE) {
        result = result.union(VmFlags::EXECUTE);
    }
    result
}

fn add_signed(base: u64, addend: i64) -> Result<u64, LoadError> {
    let value = i128::from(base) + i128::from(addend);
    u64::try_from(value).map_err(|_| LoadError::IntegerOverflow)
}

// Гарантируем, что публичный caller может передавать таблицу shared regions
// через ABI без зависимости от Rust layout внутренних структур.
const _: () = assert!(core::mem::size_of::<SharedRegion>() == 32);

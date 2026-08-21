//! Пользовательский загрузчик программ и DLL в нативном формате RUNE.
//!
//! Микроядро проверяет и запускает маленький статический loader, но граф
//! библиотек, версии ABI и символы разрешаются в ring 3. Ошибка parser'а или
//! несовместимая DLL поэтому завершают только создаваемый процесс.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ptr;

use rustos_abi::{memory::MEMORY_ABI_VERSION, syscall, Handle, VmFlags, PAGE_SIZE};
use rustos_rune_format::{
    architecture, export_flags, parse_dependency, parse_export, parse_import, parse_relocation,
    record_kind, region_flags, relocation_kind, Container, Dependency, Export, FormatError, Import,
    InterfaceId, SymbolId, TocEntry, DEPENDENCY_SIZE, EXPORT_SIZE, IMPORT_SIZE, RELOCATION_SIZE,
};
use rustos_runtime::{
    handle_close, shared_memory_create, shared_memory_map, shared_memory_seal, vm_map, vm_protect,
    vm_unmap, SharedMemoryCreate, SharedMemoryMap, VmMapRequest,
};

#[cfg(target_arch = "x86_64")]
const CURRENT_ARCHITECTURE: u16 = architecture::X86_64;
#[cfg(target_arch = "aarch64")]
const CURRENT_ARCHITECTURE: u16 = architecture::AARCH64;

const MAX_MODULES: usize = 8;
const MAX_REGIONS_PER_MODULE: usize = 12;
const MODULE_ARENA_BASE: u64 = 0x0000_5800_0000_0000;
const MODULE_STRIDE: u64 = 32 * 1024 * 1024;
const PATH_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadError {
    Format(FormatError),
    RootNotFound,
    DependencyNotFound,
    IncompatibleDependency,
    MissingImport,
    DuplicateModule,
    TooManyModules,
    TooManyMappings,
    ModuleTooLarge,
    InvalidRecord,
    UnsupportedRelocation(u16),
    TextRelocation,
    InvalidTls,
    InvalidRelro,
    InvalidPath,
    IntegerOverflow,
    Memory(i64),
}

impl From<FormatError> for LoadError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

/// Источник уже прочитанных неизменяемых RUNE containers. Реальный process
/// manager наполняет его через `vfs.dll`; loader не знает формат диска.
pub trait ModuleSource {
    fn open<'a>(&'a self, path: &str) -> Option<&'a [u8]>;
}

#[derive(Clone, Copy)]
pub struct SearchPolicy<'a> {
    pub application_dir: &'a str,
    pub private_library_dir: Option<&'a str>,
    pub system_library_dir: &'a str,
}

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
        (result == address as i64).then_some(()).ok_or(result)
    }

    fn create_shared_rw(&mut self, length: u64) -> Result<Handle, i64> {
        let result = shared_memory_create(&SharedMemoryCreate {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            length,
            flags: VmFlags::READ.union(VmFlags::WRITE),
        });
        (result > 0).then_some(Handle(result as u32)).ok_or(result)
    }

    fn map_shared(
        &mut self,
        handle: Handle,
        address: u64,
        length: u64,
        flags: VmFlags,
    ) -> Result<u64, i64> {
        let result = shared_memory_map(
            handle,
            &SharedMemoryMap {
                version: MEMORY_ABI_VERSION,
                reserved: 0,
                address,
                offset: 0,
                length,
                flags,
            },
        );
        (result > 0).then_some(result as u64).ok_or(result)
    }

    fn seal_shared(&mut self, handle: Handle, flags: VmFlags) -> Result<(), i64> {
        status(shared_memory_seal(handle, flags))
    }

    fn protect(&mut self, address: u64, length: u64, flags: VmFlags) -> Result<(), i64> {
        status(vm_protect(address, length, flags))
    }

    fn unmap(&mut self, address: u64, length: u64) -> Result<(), i64> {
        status(vm_unmap(address, length))
    }

    fn close(&mut self, handle: Handle) {
        let _ = handle_close(handle);
    }

    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), i64> {
        // SAFETY: address принадлежит RW mapping текущего процесса; границы
        // предварительно проверены parser'ом и memory ABI.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len()) };
        Ok(())
    }
}

fn status(result: i64) -> Result<(), i64> {
    (result == syscall::status::OK).then_some(()).ok_or(result)
}

#[derive(Clone, Copy, Debug)]
struct Region {
    address: u64,
    length: u64,
    final_flags: VmFlags,
    shared: Handle,
}

const EMPTY_REGION: Region = Region {
    address: 0,
    length: 0,
    final_flags: VmFlags(0),
    shared: Handle::INVALID,
};

#[derive(Clone, Copy)]
struct Module<'a> {
    container: Container<'a>,
    base: u64,
    tls_end_offset: u64,
    regions: [Region; MAX_REGIONS_PER_MODULE],
    region_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlsLayout {
    pub size: u64,
    pub alignment: u64,
    pub storage_size: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct SharedRegion {
    pub handle: Handle,
    pub address: u64,
    pub length: u64,
    pub flags: VmFlags,
}

#[derive(Clone, Copy, Debug)]
pub struct LoadedProgram {
    pub entry: u64,
    pub modules: usize,
    pub tls: TlsLayout,
    pub relro_pages: usize,
    pub shared_pages: usize,
}

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
        let result = self
            .discover_dependencies()
            .and_then(|_| self.assign_tls())
            .and_then(|_| self.map_modules(memory))
            .and_then(|_| self.relocate_modules(memory))
            .and_then(|_| self.apply_relro(memory));
        if let Err(error) = result {
            self.unload(memory);
            return Err(error);
        }
        let root = self.modules[0].ok_or(LoadError::RootNotFound)?;
        let slice = root.container.slice(CURRENT_ARCHITECTURE)?;
        let entry = root
            .base
            .checked_add(slice.virtual_address)
            .ok_or(LoadError::IntegerOverflow)?;
        if !self.address_is_executable(root, entry) {
            self.unload(memory);
            return Err(LoadError::InvalidRecord);
        }
        Ok(LoadedProgram {
            entry,
            modules: self.module_count,
            tls: self.tls,
            relro_pages: self.relro_pages,
            shared_pages: self.shared_pages,
        })
    }

    pub fn symbol(
        &self,
        interface: InterfaceId,
        symbol: SymbolId,
        minimum_abi: u16,
        maximum_abi: u16,
    ) -> Result<u64, LoadError> {
        self.resolve_export(interface, symbol, minimum_abi, maximum_abi, 0)
            .map(|definition| definition.address)
            .ok_or(LoadError::MissingImport)
    }

    pub fn shared_executable_region(&self, interface: InterfaceId) -> Option<SharedRegion> {
        for module in self.modules.iter().take(self.module_count).flatten() {
            if !module_provides(*module, interface, 1, u16::MAX) {
                continue;
            }
            for region in module.regions.iter().take(module.region_count) {
                if region.shared.is_valid() && region.final_flags.contains(VmFlags::EXECUTE) {
                    return Some(SharedRegion {
                        handle: region.shared,
                        address: region.address,
                        length: region.length,
                        flags: region.final_flags,
                    });
                }
            }
        }
        None
    }

    pub fn initialize_tls(&self, storage: &mut [u8]) -> Result<u64, LoadError> {
        let required = usize::try_from(self.tls.storage_size).map_err(|_| LoadError::InvalidTls)?;
        if storage.len() < required {
            return Err(LoadError::InvalidTls);
        }
        storage[..required].fill(0);
        let base = storage.as_ptr() as u64;
        let thread_pointer = align_up(
            base.checked_add(self.tls.size)
                .ok_or(LoadError::InvalidTls)?,
            self.tls.alignment,
        )?;
        let tls_start = thread_pointer
            .checked_sub(self.tls.size)
            .and_then(|address| address.checked_sub(base))
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(LoadError::InvalidTls)?;
        for module in self.modules.iter().take(self.module_count).flatten() {
            let Some(template) = tls_record(module.container)? else {
                continue;
            };
            let destination = tls_start
                .checked_add(
                    usize::try_from(self.tls.size - module.tls_end_offset)
                        .map_err(|_| LoadError::InvalidTls)?,
                )
                .ok_or(LoadError::InvalidTls)?;
            let payload = module
                .container
                .payload(template)
                .ok_or(LoadError::InvalidTls)?;
            storage[destination..destination + payload.len()].copy_from_slice(payload);
        }
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
        let container = Container::parse(image)?;
        container.slice(CURRENT_ARCHITECTURE)?;
        let maximum = module_end(container)?;
        if maximum > MODULE_STRIDE {
            return Err(LoadError::ModuleTooLarge);
        }
        if self
            .modules
            .iter()
            .take(self.module_count)
            .flatten()
            .any(|module| module.container.header().package_id == container.header().package_id)
        {
            return Err(LoadError::DuplicateModule);
        }
        let index = self.module_count;
        self.modules[index] = Some(Module {
            container,
            base: MODULE_ARENA_BASE + index as u64 * MODULE_STRIDE,
            tls_end_offset: 0,
            regions: [EMPTY_REGION; MAX_REGIONS_PER_MODULE],
            region_count: 0,
        });
        self.module_count += 1;
        Ok(index)
    }

    fn discover_dependencies(&mut self) -> Result<(), LoadError> {
        let mut cursor = 0;
        while cursor < self.module_count {
            let module = self.modules[cursor].ok_or(LoadError::InvalidRecord)?;
            for dependency in dependencies(module.container)? {
                if self
                    .modules
                    .iter()
                    .take(self.module_count)
                    .flatten()
                    .any(|provider| {
                        module_provides(
                            *provider,
                            dependency.interface,
                            dependency.minimum_abi,
                            dependency.maximum_abi,
                        )
                    })
                {
                    continue;
                }
                let name = module
                    .container
                    .string(dependency.name_offset, dependency.name_length)
                    .ok_or(LoadError::InvalidRecord)?;
                let image = self
                    .find_dependency(name)
                    .ok_or(LoadError::DependencyNotFound)?;
                let added = self.add_module(image)?;
                if !module_provides(
                    self.modules[added].ok_or(LoadError::InvalidRecord)?,
                    dependency.interface,
                    dependency.minimum_abi,
                    dependency.maximum_abi,
                ) {
                    return Err(LoadError::IncompatibleDependency);
                }
            }
            cursor += 1;
        }
        Ok(())
    }

    fn find_dependency(&self, name: &str) -> Option<&'a [u8]> {
        if name.is_empty() || name.as_bytes().contains(&b'/') {
            return None;
        }
        for directory in [
            Some(self.search.application_dir),
            self.search.private_library_dir,
            Some(self.search.system_library_dir),
        ]
        .into_iter()
        .flatten()
        {
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
        let mut alignment = 1u64;
        for module in self.modules.iter_mut().take(self.module_count).flatten() {
            let Some(template) = tls_record(module.container)? else {
                continue;
            };
            if template.alignment == 0
                || !template.alignment.is_power_of_two()
                || template.file_size > template.memory_size
            {
                return Err(LoadError::InvalidTls);
            }
            used = align_up(
                used.checked_add(template.memory_size)
                    .ok_or(LoadError::InvalidTls)?,
                template.alignment,
            )?;
            module.tls_end_offset = used;
            alignment = alignment.max(template.alignment);
        }
        let size = align_up(used, alignment)?;
        self.tls = TlsLayout {
            size,
            alignment,
            storage_size: size
                .checked_add(alignment - 1)
                .and_then(|size| size.checked_add(16))
                .ok_or(LoadError::InvalidTls)?,
        };
        Ok(())
    }

    fn map_modules<M: Memory>(&mut self, memory: &mut M) -> Result<(), LoadError> {
        for module_index in 0..self.module_count {
            let module = self.modules[module_index].ok_or(LoadError::InvalidRecord)?;
            for region in regions(module.container) {
                self.map_region(memory, module_index, module, region)?;
            }
        }
        Ok(())
    }

    fn map_region<M: Memory>(
        &mut self,
        memory: &mut M,
        module_index: usize,
        module: Module<'a>,
        region: TocEntry,
    ) -> Result<(), LoadError> {
        let page_start = align_down(region.virtual_address, PAGE_SIZE);
        let page_end = align_up(
            region
                .virtual_address
                .checked_add(region.memory_size)
                .ok_or(LoadError::IntegerOverflow)?,
            PAGE_SIZE,
        )?;
        let address = module
            .base
            .checked_add(page_start)
            .ok_or(LoadError::IntegerOverflow)?;
        let length = page_end - page_start;
        let final_flags = region_vm_flags(region.flags);
        let payload = module
            .container
            .payload(region)
            .ok_or(LoadError::InvalidRecord)?;
        let payload_offset = region.virtual_address - page_start;
        if region.flags & region_flags::WRITE != 0 {
            memory
                .map_private(address, length, VmFlags::READ.union(VmFlags::WRITE))
                .map_err(LoadError::Memory)?;
            if let Err(error) = memory.write(address + payload_offset, payload) {
                let _ = memory.unmap(address, length);
                return Err(LoadError::Memory(error));
            }
            if let Err(error) = self.record_region(
                module_index,
                Region {
                    address,
                    length,
                    final_flags,
                    shared: Handle::INVALID,
                },
            ) {
                let _ = memory.unmap(address, length);
                return Err(error);
            }
            Ok(())
        } else {
            let handle = match memory.create_shared_rw(length) {
                Ok(handle) => handle,
                // Ранний kernel ограничивает один shared object 256 КиБ.
                // Большой rustc/rust-lld text всё равно должен загружаться:
                // он получает private W staging, после копирования строго
                // переводится в конечный R/RX. Семантика и W^X сохраняются,
                // теряется только межпроцессное разделение этих страниц.
                Err(error) if error == syscall::status::LIMIT_REACHED => {
                    memory
                        .map_private(address, length, VmFlags::READ.union(VmFlags::WRITE))
                        .map_err(LoadError::Memory)?;
                    if let Err(error) = memory.write(address + payload_offset, payload) {
                        let _ = memory.unmap(address, length);
                        return Err(LoadError::Memory(error));
                    }
                    if let Err(error) = memory.protect(address, length, final_flags) {
                        let _ = memory.unmap(address, length);
                        return Err(LoadError::Memory(error));
                    }
                    if let Err(error) = self.record_region(
                        module_index,
                        Region {
                            address,
                            length,
                            final_flags,
                            shared: Handle::INVALID,
                        },
                    ) {
                        let _ = memory.unmap(address, length);
                        return Err(error);
                    }
                    return Ok(());
                }
                Err(error) => return Err(LoadError::Memory(error)),
            };
            let temporary =
                match memory.map_shared(handle, 0, length, VmFlags::READ.union(VmFlags::WRITE)) {
                    Ok(address) => address,
                    Err(error) => {
                        memory.close(handle);
                        return Err(LoadError::Memory(error));
                    }
                };
            if let Err(error) = memory.write(temporary + payload_offset, payload) {
                let _ = memory.unmap(temporary, length);
                memory.close(handle);
                return Err(LoadError::Memory(error));
            }
            if let Err(error) = memory.unmap(temporary, length) {
                memory.close(handle);
                return Err(LoadError::Memory(error));
            }
            if let Err(error) = memory.seal_shared(handle, final_flags) {
                memory.close(handle);
                return Err(LoadError::Memory(error));
            }
            let mapped = match memory.map_shared(handle, address, length, final_flags) {
                Ok(mapped) => mapped,
                Err(error) => {
                    memory.close(handle);
                    return Err(LoadError::Memory(error));
                }
            };
            if mapped != address {
                let _ = memory.unmap(mapped, length);
                memory.close(handle);
                return Err(LoadError::Memory(syscall::status::INVALID_ARGUMENT));
            }
            self.shared_pages += (length / PAGE_SIZE) as usize;
            if let Err(error) = self.record_region(
                module_index,
                Region {
                    address,
                    length,
                    final_flags,
                    shared: handle,
                },
            ) {
                let _ = memory.unmap(address, length);
                memory.close(handle);
                return Err(error);
            }
            Ok(())
        }
    }

    fn record_region(&mut self, module_index: usize, region: Region) -> Result<(), LoadError> {
        let module = self.modules[module_index]
            .as_mut()
            .ok_or(LoadError::InvalidRecord)?;
        if module.region_count == MAX_REGIONS_PER_MODULE {
            return Err(LoadError::TooManyMappings);
        }
        module.regions[module.region_count] = region;
        module.region_count += 1;
        Ok(())
    }

    fn relocate_modules<M: Memory>(&self, memory: &mut M) -> Result<(), LoadError> {
        for module_index in 0..self.module_count {
            let module = self.modules[module_index].ok_or(LoadError::InvalidRecord)?;
            for relocation in relocations(module.container)? {
                if !writable_target(module.container, relocation.offset, 8) {
                    return Err(LoadError::TextRelocation);
                }
                let target = module
                    .base
                    .checked_add(relocation.offset)
                    .ok_or(LoadError::IntegerOverflow)?;
                match relocation.kind {
                    relocation_kind::RELATIVE64 => {
                        let value = add_signed(module.base, relocation.addend)?;
                        memory
                            .write(target, &value.to_le_bytes())
                            .map_err(LoadError::Memory)?;
                    }
                    relocation_kind::IMPORT64 | relocation_kind::IMPORT_PC32 => {
                        let import = import_at(module.container, relocation.symbol as usize)?;
                        let definition = self
                            .resolve_export(
                                import.interface,
                                import.symbol,
                                import.minimum_abi,
                                import.maximum_abi,
                                import.flags,
                            )
                            .or_else(|| {
                                (import.flags & rustos_rune_format::import_flags::WEAK != 0)
                                    .then_some(Definition { address: 0 })
                            })
                            .ok_or(LoadError::MissingImport)?;
                        if relocation.kind == relocation_kind::IMPORT64 {
                            let value = add_signed(definition.address, relocation.addend)?;
                            memory
                                .write(target, &value.to_le_bytes())
                                .map_err(LoadError::Memory)?;
                        } else {
                            let value = i128::from(definition.address)
                                + i128::from(relocation.addend)
                                - i128::from(target);
                            let value =
                                i32::try_from(value).map_err(|_| LoadError::IntegerOverflow)?;
                            memory
                                .write(target, &value.to_le_bytes())
                                .map_err(LoadError::Memory)?;
                        }
                    }
                    relocation_kind::TLS_TPOFF64 if relocation.symbol == 0 => {
                        let value =
                            i128::from(relocation.addend) - i128::from(module.tls_end_offset);
                        let value = i64::try_from(value).map_err(|_| LoadError::InvalidTls)?;
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

    fn resolve_export(
        &self,
        interface: InterfaceId,
        symbol: SymbolId,
        minimum_abi: u16,
        maximum_abi: u16,
        import_flags: u32,
    ) -> Option<Definition> {
        for module in self.modules.iter().take(self.module_count) {
            let module = module.as_ref()?;
            for export in exports(module.container).ok()? {
                if export.interface == interface
                    && export.symbol == symbol
                    && export.abi_version >= minimum_abi
                    && export.abi_version <= maximum_abi
                    && compatible_symbol_flags(import_flags, export.flags)
                {
                    return Some(Definition {
                        address: module.base.checked_add(export.virtual_address)?,
                    });
                }
            }
        }
        None
    }

    fn apply_relro<M: Memory>(&mut self, memory: &mut M) -> Result<(), LoadError> {
        for module in self.modules.iter().take(self.module_count).flatten() {
            for relro in module.container.entries().filter(|entry| {
                entry.kind == record_kind::RELRO && entry.architecture == CURRENT_ARCHITECTURE
            }) {
                if relro.memory_size == 0 {
                    continue;
                }
                let start = module
                    .base
                    .checked_add(align_down(relro.virtual_address, PAGE_SIZE))
                    .ok_or(LoadError::InvalidRelro)?;
                let end = module
                    .base
                    .checked_add(align_up(
                        relro
                            .virtual_address
                            .checked_add(relro.memory_size)
                            .ok_or(LoadError::InvalidRelro)?,
                        PAGE_SIZE,
                    )?)
                    .ok_or(LoadError::InvalidRelro)?;
                memory
                    .protect(start, end - start, VmFlags::READ)
                    .map_err(LoadError::Memory)?;
                self.relro_pages += ((end - start) / PAGE_SIZE) as usize;
            }
        }
        Ok(())
    }

    fn address_is_executable(&self, module: Module<'a>, address: u64) -> bool {
        module
            .regions
            .iter()
            .take(module.region_count)
            .any(|region| {
                region.final_flags.contains(VmFlags::EXECUTE)
                    && address >= region.address
                    && address < region.address.saturating_add(region.length)
            })
    }
}

#[derive(Clone, Copy)]
struct Definition {
    address: u64,
}

fn regions<'a>(container: Container<'a>) -> impl Iterator<Item = TocEntry> + 'a {
    let count = container.header().toc_count as usize;
    (0..count).filter_map(move |index| {
        container.entry(index).filter(|entry| {
            entry.kind == record_kind::REGION && entry.architecture == CURRENT_ARCHITECTURE
        })
    })
}

fn module_end(container: Container<'_>) -> Result<u64, LoadError> {
    regions(container).try_fold(0, |maximum, region| {
        region
            .virtual_address
            .checked_add(region.memory_size)
            .map(|end| maximum.max(end))
            .ok_or(LoadError::IntegerOverflow)
    })
}

fn tls_record(container: Container<'_>) -> Result<Option<TocEntry>, LoadError> {
    let mut records = container.entries().filter(|entry| {
        entry.kind == record_kind::TLS && entry.architecture == CURRENT_ARCHITECTURE
    });
    let first = records.next();
    if records.next().is_some() {
        return Err(LoadError::InvalidTls);
    }
    Ok(first)
}

fn dependencies(container: Container<'_>) -> Result<RecordIterator<'_, Dependency>, LoadError> {
    RecordIterator::new(
        container,
        record_kind::DEPENDENCIES,
        DEPENDENCY_SIZE,
        parse_dependency,
    )
}

fn exports(container: Container<'_>) -> Result<RecordIterator<'_, Export>, LoadError> {
    RecordIterator::new(container, record_kind::EXPORTS, EXPORT_SIZE, parse_export)
}

fn relocations(
    container: Container<'_>,
) -> Result<RecordIterator<'_, rustos_rune_format::Relocation>, LoadError> {
    RecordIterator::new(
        container,
        record_kind::RELOCATIONS,
        RELOCATION_SIZE,
        parse_relocation,
    )
}

fn import_at(container: Container<'_>, requested: usize) -> Result<Import, LoadError> {
    let mut index = 0;
    for table in container.entries().filter(|entry| {
        entry.kind == record_kind::IMPORTS && entry.architecture == CURRENT_ARCHITECTURE
    }) {
        let bytes = container.payload(table).ok_or(LoadError::InvalidRecord)?;
        if !bytes.len().is_multiple_of(IMPORT_SIZE) {
            return Err(LoadError::InvalidRecord);
        }
        for raw in bytes.as_chunks::<IMPORT_SIZE>().0 {
            if index == requested {
                return parse_import(raw).ok_or(LoadError::InvalidRecord);
            }
            index += 1;
        }
    }
    Err(LoadError::MissingImport)
}

struct RecordIterator<'a, T> {
    container: Container<'a>,
    table_index: usize,
    bytes: &'a [u8],
    cursor: usize,
    size: usize,
    kind: u16,
    parser: fn(&[u8]) -> Option<T>,
}

impl<'a, T> RecordIterator<'a, T> {
    fn new(
        container: Container<'a>,
        kind: u16,
        size: usize,
        parser: fn(&[u8]) -> Option<T>,
    ) -> Result<Self, LoadError> {
        for table in container
            .entries()
            .filter(|entry| entry.kind == kind && entry.architecture == CURRENT_ARCHITECTURE)
        {
            let bytes = container.payload(table).ok_or(LoadError::InvalidRecord)?;
            if !bytes.len().is_multiple_of(size) {
                return Err(LoadError::InvalidRecord);
            }
        }
        Ok(Self {
            container,
            table_index: 0,
            bytes: &[],
            cursor: 0,
            size,
            kind,
            parser,
        })
    }

    fn next_table(&mut self) -> Option<Result<(), LoadError>> {
        while let Some(table) = self.container.entry(self.table_index) {
            self.table_index += 1;
            if table.kind != self.kind || table.architecture != CURRENT_ARCHITECTURE {
                continue;
            }
            let bytes = match self.container.payload(table) {
                Some(bytes) if bytes.len().is_multiple_of(self.size) => bytes,
                _ => return Some(Err(LoadError::InvalidRecord)),
            };
            self.bytes = bytes;
            self.cursor = 0;
            return Some(Ok(()));
        }
        None
    }
}

impl<T> Iterator for RecordIterator<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(raw) = self.bytes.get(self.cursor..self.cursor + self.size) {
                self.cursor += self.size;
                return (self.parser)(raw);
            }
            self.next_table()?.ok()?;
        }
    }
}

fn module_provides(
    module: Module<'_>,
    interface: InterfaceId,
    minimum_abi: u16,
    maximum_abi: u16,
) -> bool {
    exports(module.container).is_ok_and(|mut exports| {
        exports.any(|export| {
            export.interface == interface
                && export.abi_version >= minimum_abi
                && export.abi_version <= maximum_abi
        })
    })
}

fn compatible_symbol_flags(import: u32, export: u16) -> bool {
    let kind = import
        & (rustos_rune_format::import_flags::FUNCTION
            | rustos_rune_format::import_flags::DATA
            | rustos_rune_format::import_flags::TLS);
    match kind {
        rustos_rune_format::import_flags::FUNCTION => export & export_flags::FUNCTION != 0,
        rustos_rune_format::import_flags::DATA => export & export_flags::DATA != 0,
        rustos_rune_format::import_flags::TLS => export & export_flags::TLS != 0,
        0 => true,
        _ => false,
    }
}

fn writable_target(container: Container<'_>, offset: u64, length: u64) -> bool {
    regions(container).any(|region| {
        region.flags & region_flags::WRITE != 0
            && offset >= region.virtual_address
            && offset
                .checked_add(length)
                .is_some_and(|end| end <= region.virtual_address.saturating_add(region.memory_size))
    })
}

fn region_vm_flags(flags: u32) -> VmFlags {
    let mut result = VmFlags(0);
    if flags & region_flags::READ != 0 {
        result = result.union(VmFlags::READ);
    }
    if flags & region_flags::WRITE != 0 {
        result = result.union(VmFlags::WRITE);
    }
    if flags & region_flags::EXECUTE != 0 {
        result = result.union(VmFlags::EXECUTE);
    }
    result
}

fn add_signed(base: u64, addend: i64) -> Result<u64, LoadError> {
    let value = i128::from(base) + i128::from(addend);
    u64::try_from(value).map_err(|_| LoadError::IntegerOverflow)
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Result<u64, LoadError> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(LoadError::InvalidRecord);
    }
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(LoadError::IntegerOverflow)
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

    fn join(&mut self, directory: &str, file: &str) -> Result<(), LoadError> {
        if directory.is_empty() || file.is_empty() || file.as_bytes().contains(&b'/') {
            return Err(LoadError::InvalidPath);
        }
        let separator = usize::from(!directory.ends_with('/'));
        let length = directory
            .len()
            .checked_add(separator)
            .and_then(|length| length.checked_add(file.len()))
            .filter(|length| *length <= PATH_BYTES)
            .ok_or(LoadError::InvalidPath)?;
        self.bytes[..directory.len()].copy_from_slice(directory.as_bytes());
        let mut cursor = directory.len();
        if separator != 0 {
            self.bytes[cursor] = b'/';
            cursor += 1;
        }
        self.bytes[cursor..length].copy_from_slice(file.as_bytes());
        self.length = length;
        Ok(())
    }

    fn as_str(&self) -> &str {
        // join принимает только уже проверенные UTF-8 str.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.length]) }
    }
}

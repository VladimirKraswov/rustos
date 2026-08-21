//! Переносимое владение user address space с ISA-specific PTE encoding.
//!
//! Kernel identity mappings копируются на верхнем уровне как supervisor-only;
//! пользовательская половина создаётся в отдельном root-table slot. Каждая
//! выделенная data/page-table page записывается в разреженные
//! списки владения, поэтому Drop возвращает все кадры даже при
//! частичной ошибке loader'а. Сам `AddressSpace` остаётся маленьким:
//! его можно безопасно передавать между process manager и loader'ом, не
//! резервируя десятки килобайт на kernel stack.

use core::ptr;

use rustos_abi::PAGE_SIZE;

use super::{allocate, free, FrameBlock};

const ENTRY_COUNT: usize = 512;
#[cfg(target_arch = "x86_64")]
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
#[cfg(target_arch = "aarch64")]
const ADDRESS_MASK: u64 = 0x0000_ffff_ffff_f000;

const VALID: u64 = 1 << 0;
const REGISTRY_ENTRIES: usize = PAGE_SIZE as usize / core::mem::size_of::<u64>();
const PAGE_RECORDS_PER_CHUNK: usize = PAGE_SIZE as usize / core::mem::size_of::<UserPage>();
const OWNED_FRAMES_PER_CHUNK: usize =
    (PAGE_SIZE as usize - 2 * core::mem::size_of::<u64>()) / core::mem::size_of::<FrameBlock>();
/// Отдельная arena для `vm_map`: она не пересекается с PIE image, bootstrap
/// block и растущими вниз thread stacks.
const USER_DYNAMIC_BASE: u64 = 0x0000_5000_0000_0000;
const USER_DYNAMIC_END: u64 = 0x0000_7000_0000_0000;

const EMPTY_BLOCK: FrameBlock = FrameBlock { phys: 0, frames: 0 };

/// Один физический кадр метаданных описывает page-table frames,
/// которыми владеет address space. Сам chunk в список не входит:
/// иначе для его учёта потребовался бы ещё один chunk.
#[repr(C)]
struct OwnedFrameChunk {
    next: u64,
    len: u64,
    blocks: [FrameBlock; OWNED_FRAMES_PER_CHUNK],
}

const _: () = assert!(core::mem::size_of::<OwnedFrameChunk>() == PAGE_SIZE as usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserPageFlags {
    pub writable: bool,
    pub executable: bool,
}

impl UserPageFlags {
    pub const READ_WRITE: Self = Self {
        writable: true,
        executable: false,
    };

    const fn union(self, other: Self) -> Self {
        Self {
            writable: self.writable || other.writable,
            executable: self.executable || other.executable,
        }
    }
}

#[derive(Clone, Copy)]
struct UserPage {
    used: bool,
    virtual_address: u64,
    physical_address: u64,
    flags: UserPageFlags,
    allowed_flags: UserPageFlags,
    backing: UserPageBacking,
}

/// Private frame освобождается самим address space. Shared frame принадлежит
/// kernel object и живёт, пока существуют capability либо mapping references.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserPageBacking {
    Private,
    Shared(u16),
}

const EMPTY_PAGE: UserPage = UserPage {
    used: false,
    virtual_address: 0,
    physical_address: 0,
    flags: UserPageFlags {
        writable: false,
        executable: false,
    },
    allowed_flags: UserPageFlags {
        writable: false,
        executable: false,
    },
    backing: UserPageBacking::Private,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressSpaceError {
    OutOfMemory,
    TooManyMappings,
    InvalidAddress,
    KernelMappingConflict,
    HugePageConflict,
    AlreadyMapped,
    AccessDenied,
}

pub struct AddressSpace {
    root: u64,
    owned_head: FrameBlock,
    owned_tail: FrameBlock,
    /// Трёхуровневый sparse registry user pages. Каждый уровень — обычный
    /// identity-mapped physical frame с 512 адресами следующего уровня.
    page_registry_root: FrameBlock,
    page_len: usize,
}

// AddressSpace часто перемещается по значению. Эта проверка не даст
// случайно вернуть большой inline-array и снова переполнить kernel stack.
const _: () = assert!(core::mem::size_of::<AddressSpace>() <= 128);

impl AddressSpace {
    /// Создаёт новую корневую таблицу и разделяет supervisor-only mappings.
    pub fn new(kernel_root: u64) -> Result<Self, AddressSpaceError> {
        let page_registry_root = allocate(1, 1).map_err(|_| AddressSpaceError::OutOfMemory)?;
        unsafe { (page_registry_root.phys as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
        let mut space = Self {
            root: 0,
            owned_head: EMPTY_BLOCK,
            owned_tail: EMPTY_BLOCK,
            page_registry_root,
            page_len: 0,
        };
        let root = space.allocate_zeroed_frame()?;
        // SAFETY: оба адреса — identity-mapped 4-KiB page-table frames.
        unsafe {
            ptr::copy_nonoverlapping(kernel_root as *const u64, root as *mut u64, ENTRY_COUNT);
        }
        space.root = root;
        Ok(space)
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    /// Отображает одну user page. Повторное отображение объединяет права;
    /// ELF-сегменты часто делят граничную страницу.
    pub fn map_page(
        &mut self,
        virtual_address: u64,
        flags: UserPageFlags,
    ) -> Result<u64, AddressSpaceError> {
        if !virtual_address.is_multiple_of(PAGE_SIZE) || !is_user_canonical(virtual_address) {
            return Err(AddressSpaceError::InvalidAddress);
        }
        if let Some(index) = self.find_page(virtual_address) {
            let page = self.page(index);
            let merged = page.flags.union(flags);
            self.page_mut(index).flags = merged;
            self.update_leaf_flags(virtual_address, merged)?;
            return Ok(page.physical_address);
        }
        let block = allocate(1, 1).map_err(|_| AddressSpaceError::OutOfMemory)?;
        unsafe { (block.phys as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
        let physical_address = block.phys;
        let allowed_flags = UserPageFlags {
            writable: true,
            executable: true,
        };
        if let Err(error) = self.map_physical_page(
            virtual_address,
            physical_address,
            flags,
            allowed_flags,
            UserPageBacking::Private,
        ) {
            let _ = free(block);
            return Err(error);
        }
        Ok(physical_address)
    }

    /// Отображает frame shared-memory object без передачи владения frame'ом
    /// address space. Повторное отображение на тот же адрес запрещено.
    pub fn map_shared_page(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
        flags: UserPageFlags,
        allowed_flags: UserPageFlags,
        object: u16,
    ) -> Result<(), AddressSpaceError> {
        if self.find_page(virtual_address).is_some() {
            return Err(AddressSpaceError::AlreadyMapped);
        }
        self.map_physical_page(
            virtual_address,
            physical_address,
            flags,
            allowed_flags,
            UserPageBacking::Shared(object),
        )
    }

    fn map_physical_page(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
        flags: UserPageFlags,
        allowed_flags: UserPageFlags,
        backing: UserPageBacking,
    ) -> Result<(), AddressSpaceError> {
        if !virtual_address.is_multiple_of(PAGE_SIZE)
            || !physical_address.is_multiple_of(PAGE_SIZE)
            || !is_user_canonical(virtual_address)
        {
            return Err(AddressSpaceError::InvalidAddress);
        }
        let record_index = self.reserve_page_record(virtual_address)?;
        let pml4_index = ((virtual_address >> 39) & 0x1ff) as usize;
        let pdpt_index = ((virtual_address >> 30) & 0x1ff) as usize;
        let pd_index = ((virtual_address >> 21) & 0x1ff) as usize;
        let pt_index = ((virtual_address >> 12) & 0x1ff) as usize;

        let root = self.root;
        let pdpt = self.ensure_user_table(root, pml4_index)?;
        let pd = self.ensure_user_table(pdpt, pdpt_index)?;
        let pt = self.ensure_user_table(pd, pd_index)?;
        // SAFETY: `pt` — выделенная таблица, индекс < 512.
        let slot = unsafe { (pt as *mut u64).add(pt_index) };
        if entry_is_valid(unsafe { slot.read() }) {
            return Err(AddressSpaceError::AlreadyMapped);
        }
        unsafe { slot.write(physical_address | leaf_flags(flags)) };
        *self.page_mut(record_index) = UserPage {
            used: true,
            virtual_address,
            physical_address,
            flags,
            allowed_flags,
            backing,
        };
        Ok(())
    }

    /// Находит свободный непрерывный диапазон в user VM arena.
    pub fn find_free_range(&self, length: u64) -> Result<u64, AddressSpaceError> {
        if length == 0 || !length.is_multiple_of(PAGE_SIZE) {
            return Err(AddressSpaceError::InvalidAddress);
        }
        let mut candidate = USER_DYNAMIC_BASE;
        while candidate
            .checked_add(length)
            .is_some_and(|end| end <= USER_DYNAMIC_END)
        {
            if self.range_is_free(candidate, length) {
                return Ok(candidate);
            }
            candidate = candidate
                .checked_add(PAGE_SIZE)
                .ok_or(AddressSpaceError::InvalidAddress)?;
        }
        Err(AddressSpaceError::TooManyMappings)
    }

    /// Проверяет, что диапазон целиком свободен и находится в user half.
    pub fn range_is_free(&self, address: u64, length: u64) -> bool {
        if length == 0 || !address.is_multiple_of(PAGE_SIZE) || !length.is_multiple_of(PAGE_SIZE) {
            return false;
        }
        let Some(end) = address.checked_add(length) else {
            return false;
        };
        if end == 0 || !is_user_canonical(address) || !is_user_canonical(end - 1) {
            return false;
        }
        let mut page = address;
        while page < end {
            if self.find_page(page).is_some() {
                return false;
            }
            page += PAGE_SIZE;
        }
        true
    }

    /// Удаляет одну страницу и возвращает тип её backing store. Private frame
    /// освобождается здесь; shared frame освобождает владеющий объект.
    pub fn unmap_page(
        &mut self,
        virtual_address: u64,
    ) -> Result<UserPageBacking, AddressSpaceError> {
        if !virtual_address.is_multiple_of(PAGE_SIZE) {
            return Err(AddressSpaceError::InvalidAddress);
        }
        let index = self
            .find_page(virtual_address)
            .ok_or(AddressSpaceError::InvalidAddress)?;
        let page = self.page(index);
        self.clear_leaf(virtual_address)?;
        self.page_mut(index).used = false;
        if page.backing == UserPageBacking::Private {
            let _ = free(FrameBlock {
                phys: page.physical_address,
                frames: 1,
            });
        }
        Ok(page.backing)
    }

    /// Меняет права уже существующей страницы. Политика W^X проверяется
    /// уровнем syscall, а address space отвечает только за PTE encoding.
    pub fn protect_page(
        &mut self,
        virtual_address: u64,
        flags: UserPageFlags,
    ) -> Result<(), AddressSpaceError> {
        let index = self
            .find_page(virtual_address)
            .ok_or(AddressSpaceError::InvalidAddress)?;
        let allowed = self.page(index).allowed_flags;
        if (flags.writable && !allowed.writable) || (flags.executable && !allowed.executable) {
            return Err(AddressSpaceError::AccessDenied);
        }
        self.page_mut(index).flags = flags;
        self.update_leaf_flags(virtual_address, flags)
    }

    /// Число страниц, ссылающихся на shared-memory object. Используется при
    /// завершении процесса для корректного refcount без обхода page tables.
    pub fn shared_mapping_pages(&self, object: u16) -> usize {
        (0..self.page_len)
            .filter(|index| {
                let page = self.page(*index);
                page.used && page.backing == UserPageBacking::Shared(object)
            })
            .count()
    }

    /// Копирует kernel-данные в уже отображённый user range, обходя
    /// физически несмежные страницы. Запись разрешена loader'у даже для RX:
    /// процесс ещё не запущен, а CR0.WP защищает mapping только после старта.
    pub fn copy_into_user(&self, address: u64, data: &[u8]) -> Result<(), AddressSpaceError> {
        let mut copied = 0usize;
        while copied < data.len() {
            let virtual_address = address
                .checked_add(copied as u64)
                .ok_or(AddressSpaceError::InvalidAddress)?;
            let page = self
                .page_for_address(virtual_address)
                .ok_or(AddressSpaceError::InvalidAddress)?;
            let offset = (virtual_address - page.virtual_address) as usize;
            let count = (PAGE_SIZE as usize - offset).min(data.len() - copied);
            // SAFETY: physical page принадлежит space, offset/count в её границах.
            unsafe {
                ptr::copy_nonoverlapping(
                    data[copied..copied + count].as_ptr(),
                    (page.physical_address as *mut u8).add(offset),
                    count,
                );
            }
            // На AMD64 I/D cache coherent. На AArch64 без явного clean D-cache
            // + invalidate I-cache только что загруженная RX-страница иногда
            // исполняет старые нули (особенно заметно под HVF при повторном
            // использовании кадра). Backend сам выбирает нужную операцию.
            crate::arch::synchronize_executable_memory(
                page.physical_address + offset as u64,
                count,
            );
            copied += count;
        }
        Ok(())
    }

    /// Безопасно копирует user memory в kernel buffer для syscall parser'а.
    pub fn copy_from_user(&self, address: u64, output: &mut [u8]) -> Result<(), AddressSpaceError> {
        let mut copied = 0usize;
        while copied < output.len() {
            let virtual_address = address
                .checked_add(copied as u64)
                .ok_or(AddressSpaceError::InvalidAddress)?;
            let page = self
                .page_for_address(virtual_address)
                .ok_or(AddressSpaceError::InvalidAddress)?;
            let offset = (virtual_address - page.virtual_address) as usize;
            let count = (PAGE_SIZE as usize - offset).min(output.len() - copied);
            // SAFETY: page описывает owned physical frame, диапазон проверен.
            unsafe {
                ptr::copy_nonoverlapping(
                    (page.physical_address as *const u8).add(offset),
                    output[copied..copied + count].as_mut_ptr(),
                    count,
                );
            }
            copied += count;
        }
        Ok(())
    }

    /// Копирует syscall/IPC result в user buffer только если весь диапазон
    /// действительно отображён writable. В отличие от loader-only
    /// `copy_into_user`, этот метод не позволяет kernel API писать в RX code.
    pub fn copy_to_user(&self, address: u64, data: &[u8]) -> Result<(), AddressSpaceError> {
        if !self.contains_user_range(address, data.len(), true) {
            return Err(AddressSpaceError::InvalidAddress);
        }
        self.copy_into_user(address, data)
    }

    pub fn contains_user_range(&self, address: u64, length: usize, write: bool) -> bool {
        if length == 0 {
            return true;
        }
        let Some(end) = address.checked_add(length as u64 - 1) else {
            return false;
        };
        let mut page_address = align_down(address, PAGE_SIZE);
        let last = align_down(end, PAGE_SIZE);
        loop {
            let Some(index) = self.find_page(page_address) else {
                return false;
            };
            let page = self.page(index);
            if write && !page.flags.writable {
                return false;
            }
            if page_address == last {
                return true;
            }
            let Some(next) = page_address.checked_add(PAGE_SIZE) else {
                return false;
            };
            page_address = next;
        }
    }

    pub fn is_executable(&self, address: u64) -> bool {
        self.page_for_address(address)
            .is_some_and(|page| page.flags.executable)
    }

    pub fn is_writable(&self, address: u64) -> bool {
        self.page_for_address(address)
            .is_some_and(|page| page.flags.writable)
    }

    fn find_page(&self, virtual_address: u64) -> Option<usize> {
        self.find_record(virtual_address)
            .filter(|index| self.page(*index).used)
    }

    fn page_for_address(&self, address: u64) -> Option<UserPage> {
        let page = align_down(address, PAGE_SIZE);
        self.find_page(page).map(|index| self.page(index))
    }

    fn ensure_page_slot(&mut self, page_index: usize) -> Result<(), AddressSpaceError> {
        let chunk_index = page_index / PAGE_RECORDS_PER_CHUNK;
        let top = chunk_index / (REGISTRY_ENTRIES * REGISTRY_ENTRIES);
        let middle = (chunk_index / REGISTRY_ENTRIES) % REGISTRY_ENTRIES;
        let leaf = chunk_index % REGISTRY_ENTRIES;
        if top >= REGISTRY_ENTRIES {
            return Err(AddressSpaceError::TooManyMappings);
        }
        let middle_table = ensure_registry_child(self.page_registry_root.phys, top)?;
        let leaf_table = ensure_registry_child(middle_table, middle)?;
        let _chunk = ensure_registry_child(leaf_table, leaf)?;
        Ok(())
    }

    fn find_record(&self, virtual_address: u64) -> Option<usize> {
        let mut left = 0usize;
        let mut right = self.page_len;
        while left < right {
            let middle = left + (right - left) / 2;
            match self.page(middle).virtual_address.cmp(&virtual_address) {
                core::cmp::Ordering::Less => left = middle + 1,
                core::cmp::Ordering::Greater => right = middle,
                core::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    fn reserve_page_record(&mut self, virtual_address: u64) -> Result<usize, AddressSpaceError> {
        if let Some(index) = self.find_record(virtual_address) {
            if self.page(index).used {
                return Err(AddressSpaceError::AlreadyMapped);
            }
            return Ok(index);
        }
        let mut left = 0usize;
        let mut right = self.page_len;
        while left < right {
            let middle = left + (right - left) / 2;
            if self.page(middle).virtual_address < virtual_address {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        let insertion = left;
        self.ensure_page_slot(self.page_len)?;
        for index in (insertion..self.page_len).rev() {
            let page = self.page(index);
            *self.page_mut(index + 1) = page;
        }
        *self.page_mut(insertion) = UserPage {
            virtual_address,
            ..EMPTY_PAGE
        };
        self.page_len += 1;
        Ok(insertion)
    }

    fn registry_chunk(&self, page_index: usize) -> Option<u64> {
        let chunk_index = page_index / PAGE_RECORDS_PER_CHUNK;
        let top = chunk_index / (REGISTRY_ENTRIES * REGISTRY_ENTRIES);
        let middle = (chunk_index / REGISTRY_ENTRIES) % REGISTRY_ENTRIES;
        let leaf = chunk_index % REGISTRY_ENTRIES;
        if top >= REGISTRY_ENTRIES {
            return None;
        }
        let middle_table = registry_child(self.page_registry_root.phys, top)?;
        let leaf_table = registry_child(middle_table, middle)?;
        registry_child(leaf_table, leaf)
    }

    fn page(&self, index: usize) -> UserPage {
        let chunk = self
            .registry_chunk(index)
            .expect("page registry slot must exist");
        let offset = index % PAGE_RECORDS_PER_CHUNK;
        unsafe { (chunk as *const UserPage).add(offset).read() }
    }

    fn page_mut(&mut self, index: usize) -> &mut UserPage {
        let chunk = self
            .registry_chunk(index)
            .expect("page registry slot must exist");
        let offset = index % PAGE_RECORDS_PER_CHUNK;
        unsafe { &mut *(chunk as *mut UserPage).add(offset) }
    }

    fn allocate_zeroed_frame(&mut self) -> Result<u64, AddressSpaceError> {
        let block = allocate(1, 1).map_err(|_| AddressSpaceError::OutOfMemory)?;
        // SAFETY: новый frame identity-mapped и эксклюзивно принадлежит space.
        unsafe { (block.phys as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
        if let Err(error) = self.record_owned_frame(block) {
            let _ = free(block);
            return Err(error);
        }
        Ok(block.phys)
    }

    fn record_owned_frame(&mut self, block: FrameBlock) -> Result<(), AddressSpaceError> {
        let needs_chunk = self.owned_tail.phys == 0
            || unsafe {
                (*(self.owned_tail.phys as *const OwnedFrameChunk)).len as usize
                    == OWNED_FRAMES_PER_CHUNK
            };
        if needs_chunk {
            let chunk = allocate(1, 1).map_err(|_| AddressSpaceError::OutOfMemory)?;
            unsafe { (chunk.phys as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
            if self.owned_tail.phys == 0 {
                self.owned_head = chunk;
            } else {
                // SAFETY: tail — живой эксклюзивный metadata frame.
                unsafe {
                    (*(self.owned_tail.phys as *mut OwnedFrameChunk)).next = chunk.phys;
                }
            }
            self.owned_tail = chunk;
        }

        // SAFETY: tail существует и в нём есть место, что проверено выше.
        let tail = unsafe { &mut *(self.owned_tail.phys as *mut OwnedFrameChunk) };
        tail.blocks[tail.len as usize] = block;
        tail.len += 1;
        Ok(())
    }

    fn ensure_user_table(&mut self, table: u64, index: usize) -> Result<u64, AddressSpaceError> {
        // SAFETY: table — frame одного из верхних уровней, index <512.
        let slot = unsafe { (table as *mut u64).add(index) };
        let value = unsafe { slot.read() };
        if entry_is_valid(value) {
            if !entry_is_table(value) {
                return Err(AddressSpaceError::HugePageConflict);
            }
            let child = value & ADDRESS_MASK;
            if !self.owns_frame(child) {
                return Err(AddressSpaceError::KernelMappingConflict);
            }
            return Ok(child);
        }
        let child = self.allocate_zeroed_frame()?;
        unsafe { slot.write(child | table_flags()) };
        Ok(child)
    }

    fn update_leaf_flags(
        &mut self,
        virtual_address: u64,
        flags: UserPageFlags,
    ) -> Result<(), AddressSpaceError> {
        let pml4 = unsafe { table_entry(self.root, virtual_address, 39)? };
        let pdpt = unsafe { table_entry(pml4, virtual_address, 30)? };
        let pd = unsafe { table_entry(pdpt, virtual_address, 21)? };
        let index = ((virtual_address >> 12) & 0x1ff) as usize;
        let slot = unsafe { (pd as *mut u64).add(index) };
        let value = unsafe { slot.read() };
        if !entry_is_valid(value) {
            return Err(AddressSpaceError::InvalidAddress);
        }
        unsafe { slot.write((value & ADDRESS_MASK) | leaf_flags(flags)) };
        Ok(())
    }

    fn clear_leaf(&mut self, virtual_address: u64) -> Result<(), AddressSpaceError> {
        let pml4 = unsafe { table_entry(self.root, virtual_address, 39)? };
        let pdpt = unsafe { table_entry(pml4, virtual_address, 30)? };
        let pd = unsafe { table_entry(pdpt, virtual_address, 21)? };
        let index = ((virtual_address >> 12) & 0x1ff) as usize;
        let slot = unsafe { (pd as *mut u64).add(index) };
        if !entry_is_valid(unsafe { slot.read() }) {
            return Err(AddressSpaceError::InvalidAddress);
        }
        unsafe { slot.write(0) };
        Ok(())
    }

    fn owns_frame(&self, physical_address: u64) -> bool {
        let mut chunk_phys = self.owned_head.phys;
        while chunk_phys != 0 {
            // SAFETY: цепочка создана record_owned_frame и жива до Drop.
            let chunk = unsafe { &*(chunk_phys as *const OwnedFrameChunk) };
            if chunk.blocks[..chunk.len as usize]
                .iter()
                .any(|block| block.phys == physical_address && block.frames == 1)
            {
                return true;
            }
            chunk_phys = chunk.next;
        }
        false
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Private leaf frames больше не лежат в bounded `owned`: sparse
        // registry позволяет освободить сколь угодно большой address space.
        for index in 0..self.page_len {
            let page = self.page(index);
            if page.used && page.backing == UserPageBacking::Private {
                let _ = free(FrameBlock {
                    phys: page.physical_address,
                    frames: 1,
                });
            }
        }
        self.page_len = 0;

        // Address space уже не активен, поэтому page-table frames можно
        // освобождать в порядке цепочки. `next` считываем до возврата
        // metadata frame аллокатору.
        let mut chunk_phys = self.owned_head.phys;
        while chunk_phys != 0 {
            let chunk = unsafe { &*(chunk_phys as *const OwnedFrameChunk) };
            let next = chunk.next;
            for block in &chunk.blocks[..chunk.len as usize] {
                let _ = free(*block);
            }
            let _ = free(FrameBlock {
                phys: chunk_phys,
                frames: 1,
            });
            chunk_phys = next;
        }
        self.owned_head = EMPTY_BLOCK;
        self.owned_tail = EMPTY_BLOCK;

        free_registry_children(self.page_registry_root.phys, 3);
        let _ = free(self.page_registry_root);
        self.page_registry_root = EMPTY_BLOCK;
    }
}

fn ensure_registry_child(table: u64, index: usize) -> Result<u64, AddressSpaceError> {
    let slot = unsafe { (table as *mut u64).add(index) };
    let current = unsafe { slot.read() };
    if current != 0 {
        return Ok(current);
    }
    let block = allocate(1, 1).map_err(|_| AddressSpaceError::OutOfMemory)?;
    unsafe { (block.phys as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
    unsafe { slot.write(block.phys) };
    Ok(block.phys)
}

fn registry_child(table: u64, index: usize) -> Option<u64> {
    let child = unsafe { (table as *const u64).add(index).read() };
    (child != 0).then_some(child)
}

fn free_registry_children(table: u64, levels: usize) {
    for index in 0..REGISTRY_ENTRIES {
        let Some(child) = registry_child(table, index) else {
            continue;
        };
        if levels > 1 {
            free_registry_children(child, levels - 1);
        }
        let _ = free(FrameBlock {
            phys: child,
            frames: 1,
        });
    }
}

unsafe fn table_entry(table: u64, address: u64, shift: u32) -> Result<u64, AddressSpaceError> {
    let index = ((address >> shift) & 0x1ff) as usize;
    let value = unsafe { (table as *const u64).add(index).read() };
    if !entry_is_valid(value) || !entry_is_table(value) {
        return Err(AddressSpaceError::InvalidAddress);
    }
    Ok(value & ADDRESS_MASK)
}

#[cfg(target_arch = "x86_64")]
const fn table_flags() -> u64 {
    VALID | (1 << 1) | (1 << 2) // present, writable, user
}

#[cfg(target_arch = "aarch64")]
const fn table_flags() -> u64 {
    VALID | (1 << 1) // valid table descriptor
}

#[cfg(target_arch = "x86_64")]
const fn leaf_flags(flags: UserPageFlags) -> u64 {
    VALID
        | (1 << 2) // user
        | if flags.writable { 1 << 1 } else { 0 }
        | if flags.executable { 0 } else { 1 << 63 }
}

#[cfg(target_arch = "aarch64")]
const fn leaf_flags(flags: UserPageFlags) -> u64 {
    // L3 page descriptor обязан иметь type=0b11. AP[7:6]=01
    // разрешает EL0 read/write, AP=11 — EL0 read-only. AP=00, который
    // был здесь раньше, является supervisor-only и неизбежно даёт
    // permission fault при первой же инструкции процесса.
    VALID
        | (1 << 1) // type: L3 page
        | (1 << 6) // AP[0]: EL0 access enabled
        | (3 << 8) // SH: inner shareable
        | (1 << 10) // AF: access flag
        | if flags.writable { 0 } else { 1 << 7 } // AP[1]: read-only
        | if flags.executable {
            1 << 53 // PXN: EL1 never executes user pages
        } else {
            (1 << 53) | (1 << 54) // PXN + UXN
        }
}

const fn entry_is_valid(value: u64) -> bool {
    value & VALID != 0
}

#[cfg(target_arch = "x86_64")]
const fn entry_is_table(value: u64) -> bool {
    value & (1 << 7) == 0 // PS=0
}

#[cfg(target_arch = "aarch64")]
const fn entry_is_table(value: u64) -> bool {
    value & (1 << 1) != 0 // table descriptor on levels 0..2
}

const fn is_user_canonical(address: u64) -> bool {
    // Lower 48-bit user VA only. Kernel identity mappings остаются ниже
    // нескольких TiB, а loader использует отдельный root-table slot.
    address < 0x0000_8000_0000_0000
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

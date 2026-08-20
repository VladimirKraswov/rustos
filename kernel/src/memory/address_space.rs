//! Отдельное x86-64 address space процесса.
//!
//! Kernel identity mappings копируются на верхнем уровне как supervisor-only;
//! пользовательская половина создаётся в отдельном PML4 slot. Каждая
//! выделенная data/page-table page записывается в `owned`, поэтому Drop
//! возвращает все кадры даже при частичной ошибке ELF loader'а.

use core::ptr;

use rustos_abi::PAGE_SIZE;

use super::{allocate, free, FrameBlock};

const ENTRY_COUNT: usize = 512;
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const MAX_OWNED_FRAMES: usize = 512;
const MAX_USER_PAGES: usize = 384;

const EMPTY_BLOCK: FrameBlock = FrameBlock { phys: 0, frames: 0 };

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
    virtual_address: u64,
    physical_address: u64,
    flags: UserPageFlags,
}

const EMPTY_PAGE: UserPage = UserPage {
    virtual_address: 0,
    physical_address: 0,
    flags: UserPageFlags {
        writable: false,
        executable: false,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressSpaceError {
    OutOfMemory,
    TooManyMappings,
    InvalidAddress,
    KernelMappingConflict,
    HugePageConflict,
    AlreadyMapped,
}

pub struct AddressSpace {
    root: u64,
    owned: [FrameBlock; MAX_OWNED_FRAMES],
    owned_len: usize,
    pages: [UserPage; MAX_USER_PAGES],
    page_len: usize,
}

impl AddressSpace {
    /// Создаёт новый PML4 и разделяет supervisor-only kernel mappings.
    pub fn new(kernel_root: u64) -> Result<Self, AddressSpaceError> {
        let mut space = Self {
            root: 0,
            owned: [EMPTY_BLOCK; MAX_OWNED_FRAMES],
            owned_len: 0,
            pages: [EMPTY_PAGE; MAX_USER_PAGES],
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
            let merged = self.pages[index].flags.union(flags);
            self.pages[index].flags = merged;
            self.update_leaf_flags(virtual_address, merged)?;
            return Ok(self.pages[index].physical_address);
        }
        if self.page_len == MAX_USER_PAGES {
            return Err(AddressSpaceError::TooManyMappings);
        }

        let physical_address = self.allocate_zeroed_frame()?;
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
        if unsafe { slot.read() } & PRESENT != 0 {
            return Err(AddressSpaceError::AlreadyMapped);
        }
        unsafe { slot.write(physical_address | leaf_flags(flags)) };
        self.pages[self.page_len] = UserPage {
            virtual_address,
            physical_address,
            flags,
        };
        self.page_len += 1;
        Ok(physical_address)
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
            let Some(page) = self.pages[..self.page_len]
                .iter()
                .find(|page| page.virtual_address == page_address)
            else {
                return false;
            };
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

    fn find_page(&self, virtual_address: u64) -> Option<usize> {
        self.pages[..self.page_len]
            .iter()
            .position(|page| page.virtual_address == virtual_address)
    }

    fn page_for_address(&self, address: u64) -> Option<&UserPage> {
        let page = align_down(address, PAGE_SIZE);
        self.pages[..self.page_len]
            .iter()
            .find(|entry| entry.virtual_address == page)
    }

    fn allocate_zeroed_frame(&mut self) -> Result<u64, AddressSpaceError> {
        if self.owned_len == MAX_OWNED_FRAMES {
            return Err(AddressSpaceError::TooManyMappings);
        }
        let block = allocate(1, 1).map_err(|_| AddressSpaceError::OutOfMemory)?;
        // SAFETY: новый frame identity-mapped и эксклюзивно принадлежит space.
        unsafe { (block.phys as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
        self.owned[self.owned_len] = block;
        self.owned_len += 1;
        Ok(block.phys)
    }

    fn ensure_user_table(&mut self, table: u64, index: usize) -> Result<u64, AddressSpaceError> {
        // SAFETY: table — PML4/PDPT/PD frame, index <512.
        let slot = unsafe { (table as *mut u64).add(index) };
        let value = unsafe { slot.read() };
        if value & PRESENT != 0 {
            if value & HUGE != 0 {
                return Err(AddressSpaceError::HugePageConflict);
            }
            if value & USER == 0 {
                return Err(AddressSpaceError::KernelMappingConflict);
            }
            return Ok(value & ADDRESS_MASK);
        }
        let child = self.allocate_zeroed_frame()?;
        unsafe { slot.write(child | PRESENT | WRITABLE | USER) };
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
        if value & PRESENT == 0 {
            return Err(AddressSpaceError::InvalidAddress);
        }
        unsafe { slot.write((value & ADDRESS_MASK) | leaf_flags(flags)) };
        Ok(())
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Владелец обязан сначала вернуть kernel CR3. Reverse order удобен
        // для диагностики: data frames уходят до корневой таблицы.
        for index in (0..self.owned_len).rev() {
            let _ = free(self.owned[index]);
            self.owned[index] = EMPTY_BLOCK;
        }
        self.owned_len = 0;
    }
}

unsafe fn table_entry(table: u64, address: u64, shift: u32) -> Result<u64, AddressSpaceError> {
    let index = ((address >> shift) & 0x1ff) as usize;
    let value = unsafe { (table as *const u64).add(index).read() };
    if value & PRESENT == 0 || value & HUGE != 0 {
        return Err(AddressSpaceError::InvalidAddress);
    }
    Ok(value & ADDRESS_MASK)
}

const fn leaf_flags(flags: UserPageFlags) -> u64 {
    PRESENT
        | USER
        | if flags.writable { WRITABLE } else { 0 }
        | if flags.executable { 0 } else { NO_EXECUTE }
}

const fn is_user_canonical(address: u64) -> bool {
    // Lower canonical half only. Kernel identity mappings остаются ниже
    // нескольких TiB, а loader использует отдельный PML4 slot 128.
    address < 0x0000_8000_0000_0000
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

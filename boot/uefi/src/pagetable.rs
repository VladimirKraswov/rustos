//! Ранние identity page tables, передаваемые UEFI-загрузчиком ядру.
//!
//! Это bootstrap-карта, а не окончательное адресное пространство ядра.
//! Она отображает реальные RAM-дескрипторы UEFI, резерв ядра, RSDP, local
//! APIC MMIO и GOP framebuffer. Остальные физические дыры остаются unmapped.
//!
//! Иерархия x86-64 (LA57=0): PML4 -> PDPT -> PD -> PT. Полностью
//! выровненные куски отображаются страницами 2 MiB, края — 4 KiB.

use rustos_abi::bootinfo::BootFramebuffer;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};

// Биты PTE (page table entry, 64 бита): см. раздел 4.10 Intel SDM.
const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
/// Бит 7 PTE «Page Size»: 1 — крупная 2 MiB страница (leaf), 0 — указатель
/// на таблицу нижнего уровня. Не путать с размером страницы (4 KiB).
const PAGE_SIZE: u64 = 1 << 7;
/// Маска физического адреса в PTE (биты 51..12).
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;
/// Архитектурный physical base legacy xAPIC. Нужен как fallback на QEMU TCG
/// и CPU без x2APIC; x2APIC backend эту страницу не трогает.
const LOCAL_APIC_MMIO: u64 = 0xfee0_0000;
/// Флаги для записей-указателей на таблицы.
const TABLE_FLAGS: u64 = PRESENT | WRITABLE;
/// Флаги для записей-листьев (самых страниц).
const LEAF_FLAGS: u64 = PRESENT | WRITABLE;

/// Ошибка построения bootstrap-карты.
#[derive(Debug)]
pub enum PtError {
    /// Бюджет page tables (16 MiB) исчерпан.
    OutOfBudget,
    /// Диапазон физический адресов выходит за пределы u64.
    AddressOverflow,
    /// Один и тот же диапазон пытаются отображать с конфликтом
    /// (например, 2 MiB-листь поверх существующей 4 KiB-таблицы).
    ConflictingMapping,
}

impl core::fmt::Display for PtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfBudget => write!(f, "page table budget exhausted"),
            Self::AddressOverflow => write!(f, "physical mapping range overflows u64"),
            Self::ConflictingMapping => write!(f, "conflicting bootstrap page mapping"),
        }
    }
}

/// Индексы четырёх уровней page table для канонического адреса.
#[derive(Clone, Copy)]
struct Indices {
    pml4: usize,
    pdpt: usize,
    pd: usize,
    pt: usize,
}

#[inline]
fn indices(address: u64) -> Indices {
    Indices {
        pml4: ((address >> 39) & 0x1ff) as usize,
        pdpt: ((address >> 30) & 0x1ff) as usize,
        pd: ((address >> 21) & 0x1ff) as usize,
        pt: ((address >> 12) & 0x1ff) as usize,
    }
}

/// Раздаёт 4 KiB page-table frames из фиксированного бюджета загрузчика;
/// каждая таблица обнуляется при выделении.
struct TableAllocator {
    next: u64,
    end: u64,
}

impl TableAllocator {
    fn new(base: u64, budget: u64) -> Result<Self, PtError> {
        let end = base.checked_add(budget).ok_or(PtError::AddressOverflow)?;
        Ok(Self { next: base, end })
    }

    fn allocate(&mut self) -> Result<u64, PtError> {
        let next = self
            .next
            .checked_add(PAGE_4K)
            .ok_or(PtError::AddressOverflow)?;
        if next > self.end {
            return Err(PtError::OutOfBudget);
        }
        let frame = self.next;
        self.next = next;
        // SAFETY: весь table budget заранее закреплён загрузчиком как
        // LOADER_DATA и доступен через действующий UEFI identity mapping.
        unsafe { (frame as *mut u8).write_bytes(0, PAGE_4K as usize) };
        Ok(frame)
    }
}

/// Строит разрежённую identity-карту для раннего ядра.
///
/// # Safety
///
/// `[table_base, table_base + table_budget)` принадлежит загрузчику и
/// доступен на запись. Все переданные физические диапазоны существуют.
pub unsafe fn build_identity_map(
    table_base: u64,
    table_budget: u64,
    memory_map: &MemoryMapOwned,
    reservation_base: u64,
    reservation_size: u64,
    rsdp: u64,
    framebuffer: &BootFramebuffer,
) -> Result<u64, PtError> {
    let mut allocator = TableAllocator::new(table_base, table_budget)?;
    let pml4 = allocator.allocate()?;

    // Отображаем только типы, за которыми действительно стоит RAM. MMIO и
    // большие reserved holes из UEFI-карты сюда не входят.
    for descriptor in memory_map.entries() {
        if is_ram(descriptor.ty) {
            let size = descriptor
                .page_count
                .checked_mul(PAGE_4K)
                .ok_or(PtError::AddressOverflow)?;
            map_range(&mut allocator, pml4, descriptor.phys_start, size)?;
        }
    }

    // Явные зависимости ядра. Повторное отображение с тем же адресом
    // разрешено и делает код устойчивым к классификации firmware.
    map_range(&mut allocator, pml4, reservation_base, reservation_size)?;
    if rsdp != 0 {
        map_range(&mut allocator, pml4, align_down(rsdp, PAGE_4K), PAGE_4K)?;
    }
    map_range(&mut allocator, pml4, LOCAL_APIC_MMIO, PAGE_4K)?;
    if framebuffer.phys_addr != 0 {
        let bytes = (framebuffer.stride as u64)
            .checked_mul(framebuffer.height as u64)
            .ok_or(PtError::AddressOverflow)?;
        let start = align_down(framebuffer.phys_addr, PAGE_4K);
        let prefix = framebuffer.phys_addr - start;
        map_range(
            &mut allocator,
            pml4,
            start,
            align_up(
                prefix.checked_add(bytes).ok_or(PtError::AddressOverflow)?,
                PAGE_4K,
            )?,
        )?;
    }

    Ok(pml4)
}

/// Учитываем только UEFI-типы, за которыми действительно стоит RAM (включая
/// loader/boot/runtime service — это RAM, а не MMIO). MMIO и резерв firmware
/// остаются unmapped: раннее ядро их не трогает.
fn is_ram(ty: MemoryType) -> bool {
    matches!(
        ty,
        MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::RUNTIME_SERVICES_CODE
            | MemoryType::RUNTIME_SERVICES_DATA
            | MemoryType::CONVENTIONAL
            | MemoryType::ACPI_RECLAIM
            | MemoryType::ACPI_NON_VOLATILE
            | MemoryType::PERSISTENT_MEMORY
    )
}

fn map_range(
    allocator: &mut TableAllocator,
    pml4: u64,
    start: u64,
    size: u64,
) -> Result<(), PtError> {
    if size == 0 {
        return Ok(());
    }
    let end = start.checked_add(size).ok_or(PtError::AddressOverflow)?;
    let mut address = align_down(start, PAGE_4K);

    // Невыровненное начало до ближайшей границы 2 MiB.
    while address < end && !address.is_multiple_of(PAGE_2M) {
        map_4k(allocator, pml4, address)?;
        address = address
            .checked_add(PAGE_4K)
            .ok_or(PtError::AddressOverflow)?;
    }
    // Только полные 2 MiB внутри диапазона.
    while address.checked_add(PAGE_2M).is_some_and(|next| next <= end) {
        map_2m(allocator, pml4, address)?;
        address += PAGE_2M;
    }
    // Хвост после последней полной крупной страницы.
    while address < end {
        map_4k(allocator, pml4, address)?;
        address = address
            .checked_add(PAGE_4K)
            .ok_or(PtError::AddressOverflow)?;
    }
    Ok(())
}

fn map_2m(allocator: &mut TableAllocator, pml4: u64, address: u64) -> Result<(), PtError> {
    let idx = indices(address);
    // SAFETY: все таблицы выделены из `allocator`, индексы всегда <512.
    unsafe {
        let pdpt = ensure_table(allocator, pml4, idx.pml4)?;
        let pd = ensure_table(allocator, pdpt, idx.pdpt)?;
        let slot = entry_ptr(pd, idx.pd);
        let old = slot.read();
        let value = address | LEAF_FLAGS | PAGE_SIZE;
        if old == 0 || old == value {
            slot.write(value);
            Ok(())
        } else if old & PRESENT != 0 && old & PAGE_SIZE == 0 {
            // Этот 2 MiB диапазон уже частично отображён таблицей 4 KiB
            // (например, соседним UEFI descriptor). Дополняем её, не
            // заменяя более точную таблицу крупным leaf-входом.
            for offset in (0..PAGE_2M).step_by(PAGE_4K as usize) {
                map_4k(allocator, pml4, address + offset)?;
            }
            Ok(())
        } else {
            Err(PtError::ConflictingMapping)
        }
    }
}

fn map_4k(allocator: &mut TableAllocator, pml4: u64, address: u64) -> Result<(), PtError> {
    let idx = indices(address);
    // SAFETY: все таблицы выделены из `allocator`, индексы всегда <512.
    unsafe {
        let pdpt = ensure_table(allocator, pml4, idx.pml4)?;
        let pd = ensure_table(allocator, pdpt, idx.pdpt)?;
        let pd_slot = entry_ptr(pd, idx.pd);
        let pd_value = pd_slot.read();
        if pd_value & PRESENT != 0 && pd_value & PAGE_SIZE != 0 {
            let expected = align_down(address, PAGE_2M) | LEAF_FLAGS | PAGE_SIZE;
            return if pd_value == expected {
                Ok(())
            } else {
                Err(PtError::ConflictingMapping)
            };
        }
        let pt = ensure_table(allocator, pd, idx.pd)?;
        let slot = entry_ptr(pt, idx.pt);
        let old = slot.read();
        let value = address | LEAF_FLAGS;
        if old == 0 || old == value {
            slot.write(value);
            Ok(())
        } else {
            Err(PtError::ConflictingMapping)
        }
    }
}

/// Возвращает таблицу следующего уровня, создавая её при необходимости.
///
/// # Safety
///
/// `table` указывает на выделенный 4 KiB page-table frame, `index < 512`.
unsafe fn ensure_table(
    allocator: &mut TableAllocator,
    table: u64,
    index: usize,
) -> Result<u64, PtError> {
    let slot = entry_ptr(table, index);
    let old = slot.read();
    if old & PRESENT != 0 {
        if old & PAGE_SIZE != 0 {
            return Err(PtError::ConflictingMapping);
        }
        return Ok(old & ADDRESS_MASK);
    }
    let child = allocator.allocate()?;
    slot.write(child | TABLE_FLAGS);
    Ok(child)
}

#[inline]
unsafe fn entry_ptr(table: u64, index: usize) -> *mut u64 {
    debug_assert!(index < 512);
    // SAFETY: контракт вызывающего гарантирует page-table frame.
    unsafe { (table as *mut u64).add(index) }
}

#[inline]
fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

#[inline]
fn align_up(value: u64, alignment: u64) -> Result<u64, PtError> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or(PtError::AddressOverflow)
}

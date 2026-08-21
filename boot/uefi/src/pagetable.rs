//! Ранние identity page tables, передаваемые UEFI-загрузчиком ядру.
//!
//! Это bootstrap-карта, а не окончательное адресное пространство ядра.
//! Она отображает реальные RAM-дескрипторы UEFI, резерв ядра, таблицу
//! описания firmware (ACPI RSDP / Device Tree), окна MMIO и
//! GOP-framebuffer. Остальные физические дыры остаются unmapped.
//!
//! ## x86-64 (LA57=0)
//!
//! PML4 -> PDPT -> PD -> PT. Полностью выровненные куски — страницы 2 MiB,
//! края — 4 KiB.
//!
//! ## AArch64 (4K granule, 4 уровня, 48-bit VA)
//!
//! L0 -> L1 -> L2 -> L3. 2 MiB-блоки на L2, края — 4 KiB. RAM и резерв —
//! normal WB (MAIR AttrIdx 0), GIC и PL011 — device-nGnRE (MAIR AttrIdx 1).

use rustos_abi::bootinfo::BootFramebuffer;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;

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

/// Раздаёт 4 KiB page-table frames из фиксированного бюджета загрузчика;
/// каждая таблица обнуляется при выделении.
struct TableAllocator {
    #[cfg(target_arch = "aarch64")]
    base: u64,
    next: u64,
    end: u64,
}

impl TableAllocator {
    fn new(base: u64, budget: u64) -> Result<Self, PtError> {
        let end = base.checked_add(budget).ok_or(PtError::AddressOverflow)?;
        Ok(Self {
            #[cfg(target_arch = "aarch64")]
            base,
            next: base,
            end,
        })
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

/// Строит разрежённую identity-карту для раннего ядра.
///
/// `firmware_root`/`firmware_size` — таблица описания firmware (x86: ACPI
/// RSDP, одна страница; AArch64: Device Tree целиком — SMP-раздел ядра
/// парсит /cpus).
///
/// # Safety
///
/// `[table_base, table_base + table_budget)` принадлежит загрузчику и
/// доступен на запись. Все переданные физические диапазоны существуют.
#[allow(clippy::too_many_arguments)]
pub unsafe fn build_identity_map(
    table_base: u64,
    table_budget: u64,
    memory_map: &MemoryMapOwned,
    reservation_base: u64,
    reservation_size: u64,
    firmware_root: u64,
    firmware_size: u64,
    framebuffer: &BootFramebuffer,
) -> Result<u64, PtError> {
    let mut allocator = TableAllocator::new(table_base, table_budget)?;
    let root = allocator.allocate()?;

    // Отображаем только типы, за которыми действительно стоит RAM. MMIO и
    // большие reserved holes из UEFI-карты сюда не входят.
    for descriptor in memory_map.entries() {
        if is_ram(descriptor.ty) {
            let size = descriptor
                .page_count
                .checked_mul(PAGE_4K)
                .ok_or(PtError::AddressOverflow)?;
            map_range(&mut allocator, root, descriptor.phys_start, size)?;
        }
    }

    // Явные зависимости ядра. Повторное отображение с тем же адресом
    // разрешено и делает код устойчивым к классификации firmware.
    map_range(&mut allocator, root, reservation_base, reservation_size)?;
    if firmware_root != 0 {
        let size = align_up(firmware_size, PAGE_4K)?;
        map_range(
            &mut allocator,
            root,
            align_down(firmware_root, PAGE_4K),
            size,
        )?;
    }
    map_device_ranges(&mut allocator, root)?;
    if framebuffer.phys_addr != 0 {
        let bytes = (framebuffer.stride as u64)
            .checked_mul(framebuffer.height as u64)
            .ok_or(PtError::AddressOverflow)?;
        let start = align_down(framebuffer.phys_addr, PAGE_4K);
        let prefix = framebuffer.phys_addr - start;
        map_range(
            &mut allocator,
            root,
            start,
            align_up(
                prefix.checked_add(bytes).ok_or(PtError::AddressOverflow)?,
                PAGE_4K,
            )?,
        )?;
    }

    Ok(root)
}

/// Окна MMIO, без которых раннее ядро не работает. На x86 — legacy xAPIC
/// (fallback на QEMU TCG; x2APIC backend эту страницу не трогает).
/// На AArch64 (QEMU virt) — GIC-регион и PL011, отображённые device-nGnRE.
fn map_device_ranges(allocator: &mut TableAllocator, root: u64) -> Result<(), PtError> {
    #[cfg(target_arch = "x86_64")]
    {
        const LOCAL_APIC_MMIO: u64 = 0xfee0_0000;
        map_range(allocator, root, LOCAL_APIC_MMIO, PAGE_4K)
    }
    #[cfg(target_arch = "aarch64")]
    {
        map_range_device(allocator, root, A_PL011_BASE, PAGE_4K)?;
        map_range_device(allocator, root, A_GIC_BASE, A_GIC_SIZE)?;
        map_range_device(allocator, root, A_VIRTIO_MMIO_BASE, A_VIRTIO_MMIO_SIZE)
    }
}

#[cfg(target_arch = "x86_64")]
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

// ===================== x86-64 (PML4 -> PDPT -> PD -> PT) =====================
// Биты PTE: раздел 4.10 Intel SDM.

#[cfg(target_arch = "x86_64")]
const PRESENT: u64 = 1 << 0;
#[cfg(target_arch = "x86_64")]
const WRITABLE: u64 = 1 << 1;
/// Бит 7 PTE «Page Size»: 1 — крупная 2 MiB страница (leaf), 0 — указатель
/// на таблицу нижнего уровня. Не путать с размером страницы (4 KiB).
#[cfg(target_arch = "x86_64")]
const PAGE_SIZE: u64 = 1 << 7;
/// Маска физического адреса в PTE (биты 51..12).
#[cfg(target_arch = "x86_64")]
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
/// Флаги для записей-указателей на таблицы.
#[cfg(target_arch = "x86_64")]
const TABLE_FLAGS: u64 = PRESENT | WRITABLE;
/// Флаги для записей-листьев (самых страниц).
#[cfg(target_arch = "x86_64")]
const LEAF_FLAGS: u64 = PRESENT | WRITABLE;

/// Индексы четырёх уровней page table для канонического адреса.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
struct Indices {
    pml4: usize,
    pdpt: usize,
    pd: usize,
    pt: usize,
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn indices(address: u64) -> Indices {
    Indices {
        pml4: ((address >> 39) & 0x1ff) as usize,
        pdpt: ((address >> 30) & 0x1ff) as usize,
        pd: ((address >> 21) & 0x1ff) as usize,
        pt: ((address >> 12) & 0x1ff) as usize,
    }
}

#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn entry_ptr(table: u64, index: usize) -> *mut u64 {
    debug_assert!(index < 512);
    // SAFETY: контракт вызывающего гарантирует page-table frame.
    unsafe { (table as *mut u64).add(index) }
}

// ===================== AArch64 (L0 -> L1 -> L2 -> L3, 4K) =====================
// PTE-кодировки: ARMv8-A Architecture Reference Manual (DDI 0487),
// 4K granule. Нижние два бита — это не x86-флаги, а тип
// descriptor:
//   0b00 = invalid;
//   0b01 = block на L1/L2;
//   0b11 = table на L0/L1/L2 и page на L3.
// Поэтому 2 MiB block и 4 KiB page нельзя строить одной
// константой leaf flags.

/// QEMU virt: PL011 UART.
#[cfg(target_arch = "aarch64")]
const A_PL011_BASE: u64 = 0x0900_0000;
/// QEMU virt: GIC-регион (GICD + per-CPU GICR-окна), 16 MiB.
#[cfg(target_arch = "aarch64")]
const A_GIC_BASE: u64 = 0x0800_0000;
#[cfg(target_arch = "aarch64")]
const A_GIC_SIZE: u64 = 0x0100_0000;
/// 32 virtio-mmio transport slots QEMU `virt` по 0x200 байт.
#[cfg(target_arch = "aarch64")]
const A_VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;
#[cfg(target_arch = "aarch64")]
const A_VIRTIO_MMIO_SIZE: u64 = 32 * 0x200;
/// Маска физического адреса в PTE (биты 51..12).
#[cfg(target_arch = "aarch64")]
const A_ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
/// Table descriptor: valid=1, type=table (bit1=1).
#[cfg(target_arch = "aarch64")]
const A_TABLE_FLAGS: u64 = 0b11;
/// Общие атрибуты leaf: AF | inner-shareable | AP=00 (EL1 RW,
/// EL0 access denied). User mappings строятся отдельно в kernel.
#[cfg(target_arch = "aarch64")]
const A_LEAF_ATTRIBUTES: u64 = (1 << 10) | (0b11 << 8);
#[cfg(target_arch = "aarch64")]
const A_BLOCK_NORMAL: u64 = A_LEAF_ATTRIBUTES | 0b01;
#[cfg(target_arch = "aarch64")]
const A_PAGE_NORMAL: u64 = A_LEAF_ATTRIBUTES | 0b11;
/// Device-nGnRE использует MAIR AttrIdx 1.
#[cfg(target_arch = "aarch64")]
const A_BLOCK_DEVICE: u64 = A_BLOCK_NORMAL | (1 << 2);
#[cfg(target_arch = "aarch64")]
const A_PAGE_DEVICE: u64 = A_PAGE_NORMAL | (1 << 2);

/// Индексы четырёх уровней AArch64 page table (4K granule, 48-bit VA).
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
struct AIndices {
    l0: usize,
    l1: usize,
    l2: usize,
    l3: usize,
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn a_indices(address: u64) -> AIndices {
    AIndices {
        l0: ((address >> 39) & 0x1ff) as usize,
        l1: ((address >> 30) & 0x1ff) as usize,
        l2: ((address >> 21) & 0x1ff) as usize,
        l3: ((address >> 12) & 0x1ff) as usize,
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn a_entry_ptr(table: u64, index: usize) -> *mut u64 {
    debug_assert!(index < 512);
    // SAFETY: контракт вызывающего гарантирует page-table frame.
    (table as *mut u64).add(index)
}

/// L0/L1: slot всегда указатель на таблицу (крупных страниц нет).
#[cfg(target_arch = "aarch64")]
unsafe fn a_ensure_upper_table(
    allocator: &mut TableAllocator,
    table: u64,
    index: usize,
) -> Result<u64, PtError> {
    let slot = a_entry_ptr(table, index);
    let old = slot.read();
    if old != 0 {
        if old & 0b11 == A_TABLE_FLAGS {
            return Ok(old & A_ADDRESS_MASK);
        }
        return Err(PtError::ConflictingMapping);
    }
    let child = allocator.allocate()?;
    slot.write(child | A_TABLE_FLAGS);
    Ok(child)
}

/// L2: architectural type bits отличают table (0b11) от block
/// (0b01). Проверка PA allocator'а остаётся как дополнительная
/// защита от повреждённого descriptor.
#[cfg(target_arch = "aarch64")]
#[inline]
fn a_is_table_entry(allocator: &TableAllocator, entry: u64) -> bool {
    if entry & 0b11 != A_TABLE_FLAGS {
        return false;
    }
    let pa = entry & A_ADDRESS_MASK;
    allocator.base <= pa && pa < allocator.end
}

/// AArch64: одна 4 KiB страница.
#[cfg(target_arch = "aarch64")]
fn a_map_4k(
    allocator: &mut TableAllocator,
    root: u64,
    address: u64,
    leaf_flags: u64,
) -> Result<(), PtError> {
    let idx = a_indices(address);
    // SAFETY: таблицы из allocator, индексы <512.
    unsafe {
        let l1 = a_ensure_upper_table(allocator, root, idx.l0)?;
        let l2 = a_ensure_upper_table(allocator, l1, idx.l1)?;
        let l2_slot = a_entry_ptr(l2, idx.l2);
        let l2_old = l2_slot.read();
        let l3 = if l2_old == 0 {
            let table = allocator.allocate()?;
            l2_slot.write(table | A_TABLE_FLAGS);
            table
        } else if a_is_table_entry(allocator, l2_old) {
            l2_old & A_ADDRESS_MASK
        } else {
            return Err(PtError::ConflictingMapping);
        };
        let slot = a_entry_ptr(l3, idx.l3);
        let old = slot.read();
        let value = address | leaf_flags;
        if old == 0 || old == value {
            slot.write(value);
            Ok(())
        } else {
            Err(PtError::ConflictingMapping)
        }
    }
}

/// AArch64: 2 MiB-блок.
#[cfg(target_arch = "aarch64")]
fn a_map_2m(
    allocator: &mut TableAllocator,
    root: u64,
    address: u64,
    leaf_flags: u64,
) -> Result<(), PtError> {
    let idx = a_indices(address);
    // SAFETY: таблицы из allocator, индексы <512.
    unsafe {
        let l1 = a_ensure_upper_table(allocator, root, idx.l0)?;
        let l2 = a_ensure_upper_table(allocator, l1, idx.l1)?;
        let slot = a_entry_ptr(l2, idx.l2);
        let old = slot.read();
        let value = address | leaf_flags;
        if old == 0 || old == value {
            slot.write(value);
            Ok(())
        } else if a_is_table_entry(allocator, old) {
            let l3 = old & A_ADDRESS_MASK;
            for i in 0..(PAGE_2M / PAGE_4K) {
                let s = a_entry_ptr(l3, i as usize);
                let page = address + i * PAGE_4K;
                let expected = page | leaf_flags;
                let cur = s.read();
                if cur != 0 && cur != expected {
                    return Err(PtError::ConflictingMapping);
                }
                s.write(expected);
            }
            Ok(())
        } else {
            Err(PtError::ConflictingMapping)
        }
    }
}

/// AArch64: normal-WB диапазон (RAM, резерв, DTB, framebuffer).
#[cfg(target_arch = "aarch64")]
fn map_range(
    allocator: &mut TableAllocator,
    root: u64,
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
        a_map_4k(allocator, root, address, A_PAGE_NORMAL)?;
        address = address
            .checked_add(PAGE_4K)
            .ok_or(PtError::AddressOverflow)?;
    }
    // Только полные 2 MiB внутри диапазона.
    while address.checked_add(PAGE_2M).is_some_and(|next| next <= end) {
        a_map_2m(allocator, root, address, A_BLOCK_NORMAL)?;
        address += PAGE_2M;
    }
    // Хвост после последней полной крупной страницы.
    while address < end {
        a_map_4k(allocator, root, address, A_PAGE_NORMAL)?;
        address = address
            .checked_add(PAGE_4K)
            .ok_or(PtError::AddressOverflow)?;
    }
    Ok(())
}

/// AArch64: device-nGnRE диапазон (MMIO: GIC, PL011).
#[cfg(target_arch = "aarch64")]
fn map_range_device(
    allocator: &mut TableAllocator,
    root: u64,
    start: u64,
    size: u64,
) -> Result<(), PtError> {
    if size == 0 {
        return Ok(());
    }
    let end = start.checked_add(size).ok_or(PtError::AddressOverflow)?;
    let mut address = align_down(start, PAGE_4K);
    while address < end && !address.is_multiple_of(PAGE_2M) {
        a_map_4k(allocator, root, address, A_PAGE_DEVICE)?;
        address = address
            .checked_add(PAGE_4K)
            .ok_or(PtError::AddressOverflow)?;
    }
    while address.checked_add(PAGE_2M).is_some_and(|next| next <= end) {
        a_map_2m(allocator, root, address, A_BLOCK_DEVICE)?;
        address += PAGE_2M;
    }
    while address < end {
        a_map_4k(allocator, root, address, A_PAGE_DEVICE)?;
        address = address
            .checked_add(PAGE_4K)
            .ok_or(PtError::AddressOverflow)?;
    }
    Ok(())
}

/// Parking-таблица векторов AArch64: 16 слотов × 128 B, каждый начинается
/// с `b .` (0x14000000 LE). Ядро немедленно заменит VBAR; таблица лишь
/// страхует окно между ERET и установкой VBAR.
///
/// # Safety
///
/// `base` 1-KiB-выровнен, `[base, base + 2048)` принадлежит загрузчику.
#[cfg(target_arch = "aarch64")]
pub unsafe fn fill_parking_vectors(base: u64) {
    // SAFETY: base выровнен, диапазон закреплён загрузчиком в резерве.
    unsafe {
        (base as *mut u8).write_bytes(0, 2048);
        const BR_SELF: u32 = 0x1400_0000;
        for i in 0..16u64 {
            let ptr = (base + i * 128) as *mut u32;
            ptr.write_volatile(BR_SELF);
        }
    }
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

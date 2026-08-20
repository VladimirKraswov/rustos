//! Версионированная структура `BootInfo`, передаваемая UEFI-загрузчиком ядру.
//!
//! ## Инварианты
//!
//! * Ядро проверяет `magic` и `version` до использования полей. Неизвестная
//!   версия = отказ загрузки (better safe than corrupt state).
//! * Все адреса — *физические*. Ядро работает в identity-маппинге, поэтому
//!   виртуальные адреса загрузчика в BootInfo не передаются.
//! * Структура размещена в памяти, которая переживает `ExitBootServices`
//!   (выделена загрузчиком через `AllocatePages` из свободных регионов UEFI
//!   memory map; детали в docs/ARCHITECTURE.md, раздел «Загрузка»).
//! * Поле `version` монотонно растёт; совместимые расширения добавляются
//!   только в конец структуры, и ядро знает свой размер структуры.

use super::memmap::{MemRegion, MEMMAP_MAX_REGIONS};

/// Число-магия: ядро проверяет его первым. Значение выбрано произвольно, но
/// «читается» в hex: `0x5255_5354_4F53` = "RUSTOS".
pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53;
/// Текущая версия структуры BootInfo.
pub const BOOT_INFO_VERSION: u32 = 2;

/// GOP framebuffer хранит байты пикселя как R, G, B, reserved.
pub const FRAMEBUFFER_FORMAT_RGB: u32 = 0;
/// GOP framebuffer хранит байты пикселя как B, G, R, reserved.
pub const FRAMEBUFFER_FORMAT_BGR: u32 = 1;

/// Размер boot-стека CPU0 (128 KiB — с запасом на ранний вызов прерываний
/// до появления полноценного стека ядра).
pub const KERNEL_STACK_SIZE: u64 = 128 * 1024;

/// Бюджет физической памяти под page tables в резерве загрузчика (16 MiB).
///
/// Identity-маппинг с 1 GiB-страницами потребляет ~8 KiB на 1 GiB RAM
/// (PML4+PDPT) + 2 MiB-хвост и окно MMIO — 16 MiB хватает на сотни ГиБ.
/// Это константа ABI: загрузчик резервирует, а ядро (этап 2) уже не
/// перемещает таблицы.
pub const PAGE_TABLE_BUDGET: u64 = 16 * 1024 * 1024;

/// Информация о GOP-framebuffer'е.
///
/// Если `phys_addr == 0`, графический вывод недоступен (система работает
/// только через serial — ядро обязано деградировать gracefully).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootFramebuffer {
    /// Физический адрес linear framebuffer'а.
    pub phys_addr: u64,
    /// Ширина кадра в пикселях.
    pub width: u32,
    /// Высота кадра в пикселях.
    pub height: u32,
    /// Размер строки кадра в байтах (может быть больше width * bpp/8).
    pub stride: u32,
    /// Бит на пиксель (для первой реализации — только 32).
    pub bpp: u32,
    /// Один из `FRAMEBUFFER_FORMAT_*`; определяет упаковку R/G/B.
    pub format: u32,
    /// Резерв для совместимого расширения ABI.
    pub _reserved: u32,
}

impl BootFramebuffer {
    /// Framebuffer отсутствует (система работает только через serial).
    pub const ZERO: Self = Self {
        phys_addr: 0,
        width: 0,
        height: 0,
        stride: 0,
        bpp: 0,
        format: FRAMEBUFFER_FORMAT_BGR,
        _reserved: 0,
    };
}

/// Параметры загрузочного (boot) стека процессора, запускающего ядро.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootStack {
    /// Адрес верха стека (растёт вниз).
    pub top: u64,
    /// Размер стека в байтах.
    pub size: u64,
}

/// initramfs: плоский образ с системными ELF-программами и манифестом.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInitramfs {
    /// Физический адрес начала образа. 0 — initramfs отсутствует.
    pub phys_addr: u64,
    /// Размер образа в байтах.
    pub size: u64,
}

/// Полная структура BootInfo (текущая версия — [`BOOT_INFO_VERSION`] = 2).
///
/// # Layout
///
/// Размер фиксирован, выравнивание 8 байт; проверено compile-time внизу
/// модуля. Правила расширения см. в начале модуля: совместимые поля
/// добавляются только в конец, вместе с инкрементом версии.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    /// Магия [`BOOT_INFO_MAGIC`].
    pub magic: u64,
    /// Версия структуры [`BOOT_INFO_VERSION`].
    pub version: u32,
    /// Padding для выравнивания memmap под 8 байт.
    pub _pad: u32,

    /// Количество заполненных регионов в `memmap`.
    pub memmap_count: u32,
    /// Padding.
    pub _pad2: u32,
    /// Нормализованная карта физической памяти.
    pub memmap: [MemRegion; MEMMAP_MAX_REGIONS],

    /// GOP-framebuffer (может быть нулевой).
    pub framebuffer: BootFramebuffer,
    /// Физический адрес ACPI RSDP (0 — не найден).
    pub acpi_rsdp: u64,

    /// initramfs (может отсутствовать).
    pub initramfs: BootInitramfs,
    /// Физический адрес начала загруженного ELF-образа ядра (учётный,
    /// для self-test).
    pub kernel_phys: u64,
    /// Размер образа ядра в памяти (максимум `vaddr + memsz` PT_LOAD-сегментов
    /// минус `min(vaddr)`, до выравнивания).
    pub kernel_size: u64,

    /// Boot-стек для CPU, на котором стартовало ядро.
    pub boot_stack: BootStack,
}

// Compile-time проверка layout ABI.
const _: () = assert!(core::mem::size_of::<BootFramebuffer>() == 32);
const _: () = assert!(core::mem::size_of::<BootStack>() == 16);
const _: () = assert!(core::mem::size_of::<BootInitramfs>() == 16);
const _: () = assert!(core::mem::align_of::<BootInfo>() == 8);

impl BootInfo {
    /// Валидация структуры до использования полей.
    ///
    /// Возвращает `true`, только если магия и версия корректны, количество
    /// регионов в допустимых пределах и все регионы имеют валидные `kind`.
    pub fn validate(&self) -> bool {
        if self.magic != BOOT_INFO_MAGIC || self.version != BOOT_INFO_VERSION {
            return false;
        }
        if self.memmap_count as usize > MEMMAP_MAX_REGIONS {
            return false;
        }
        for i in 0..self.memmap_count as usize {
            let r = &self.memmap[i];
            if !super::memmap::MemRegion::is_valid_kind(r.kind) {
                return false;
            }
            // Физический адрес и размер обязаны быть выровнены по странице;
            // нулевой размер — ошибка загрузчика.
            if !r.phys_start.is_multiple_of(super::PAGE_SIZE)
                || !r.size.is_multiple_of(super::PAGE_SIZE)
            {
                return false;
            }
            if r.size == 0 {
                return false;
            }
        }
        true
    }

    /// Суммарный объём «usable» физической памяти (для banner'а и self-test).
    pub fn total_usable_ram(&self) -> u64 {
        (0..self.memmap_count as usize)
            .filter(|&i| self.memmap[i].kind == super::memmap::MemRegionKind::Usable as u32)
            .map(|i| self.memmap[i].size)
            .sum()
    }
}

//! Версионированная структура `BootInfo`, передаваемая загрузчиком ядру.
//!
//! ## Инварианты
//!
//! * Ядро проверяет `magic` и `version` до использования полей. Неизвестная
//!   версия = отказ загрузки (better safe than corrupt state).
//! * Все адреса — *физические*. Ядро работает в identity-маппинге, поэтому
//!   виртуальные адреса загрузчика в BootInfo не передаются.
//! * Структура размещена в зарезервированной загрузчиком памяти. GRUB передаёт
//!   исходные теги, а AMD64 bootstrap копирует их в этот единый ABI.
//! * Поле `version` монотонно растёт; совместимые расширения добавляются
//!   только в конец структуры, и ядро знает свой размер структуры.

use super::memmap::{MemRegion, MEMMAP_MAX_REGIONS};

/// Число-магия: ядро проверяет его первым. Значение выбрано произвольно, но
/// «читается» в hex: `0x5255_5354_4F53` = "RUSTOS".
pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53;
/// Текущая версия структуры BootInfo.
pub const BOOT_INFO_VERSION: u32 = 3;

/// Диагностическая консоль отсутствует.
pub const BOOT_CONSOLE_NONE: u32 = 0;
/// 16550 UART через x86 port I/O (`base` обычно 0x3f8).
pub const BOOT_CONSOLE_16550_PORT: u32 = 1;
/// 16550-compatible UART через MMIO.
pub const BOOT_CONSOLE_16550_MMIO: u32 = 2;
/// ARM PrimeCell PL011 через MMIO.
pub const BOOT_CONSOLE_PL011: u32 = 3;

/// Firmware root отсутствует.
pub const BOOT_FIRMWARE_NONE: u32 = 0;
/// `root` указывает на ACPI RSDP.
pub const BOOT_FIRMWARE_ACPI: u32 = 1;
/// `root` указывает на Flattened Device Tree blob.
pub const BOOT_FIRMWARE_DEVICE_TREE: u32 = 2;

/// Linear framebuffer хранит байты пикселя как R, G, B, reserved.
pub const FRAMEBUFFER_FORMAT_RGB: u32 = 0;
/// Linear framebuffer хранит байты пикселя как B, G, R, reserved.
pub const FRAMEBUFFER_FORMAT_BGR: u32 = 1;
/// Framebuffer был настроен непосредственно UEFI GOP loader'ом.
pub const FRAMEBUFFER_SOURCE_UEFI_GOP: u32 = 0;
/// Framebuffer и выбранный видеорежим переданы GRUB по Multiboot2.
pub const FRAMEBUFFER_SOURCE_GRUB: u32 = 1;

/// Размер boot-стека CPU0. 512 KiB позволяют безопасно конструировать bounded
/// no-heap GUI/session state и обрабатывать ранние прерывания до перехода на
/// отдельные scheduler stacks. При минимальных 128 MiB RAM это менее 0,4%.
pub const KERNEL_STACK_SIZE: u64 = 512 * 1024;

/// Бюджет физической памяти под page tables в резерве загрузчика (16 MiB).
///
/// Разрежённый identity/direct map с крупными блоками расходует память
/// пропорционально числу отображённых диапазонов, а не объёму RAM; 16 MiB
/// хватает для bootstrap tables AMD64 и AArch64 с большим запасом.
/// Это константа ABI: загрузчик резервирует, а ядро (этап 2) уже не
/// перемещает таблицы.
pub const PAGE_TABLE_BUDGET: u64 = 16 * 1024 * 1024;

/// Информация о framebuffer, настроенном firmware/загрузчиком.
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
    /// Один из `FRAMEBUFFER_SOURCE_*`. Поле раньше было резервом, поэтому
    /// значение 0 совместимо с прежним UEFI GOP loader'ом.
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

/// Ранняя debug-консоль, уже настроенная firmware/загрузчиком.
///
/// Описание находится в ABI, потому что UART зависит от платы, а не от ISA:
/// AArch64 может иметь PL011/16550, x86 embedded — MMIO 16550.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootConsole {
    /// Один из `BOOT_CONSOLE_*`.
    pub kind: u32,
    /// Резерв флагов транспорта; пока обязан быть нулём.
    pub flags: u32,
    /// Номер port I/O либо физический MMIO base.
    pub base: u64,
    /// Частота входного clock, если она известна.
    pub clock_hz: u32,
    /// Настроенная firmware скорость.
    pub baud: u32,
}

impl BootConsole {
    /// Консоль отсутствует.
    pub const NONE: Self = Self {
        kind: BOOT_CONSOLE_NONE,
        flags: 0,
        base: 0,
        clock_hz: 0,
        baud: 0,
    };
}

/// Корень аппаратного описания. ACPI и Device Tree нормализуются platform
/// layer'ом в одинаковый набор устройств/CPU, но исходный blob остаётся
/// доступен драйверам.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootFirmware {
    /// Один из `BOOT_FIRMWARE_*`.
    pub kind: u32,
    /// Резерв ABI; обязан быть нулём.
    pub _reserved: u32,
    /// Физический адрес RSDP либо FDT blob.
    pub root: u64,
}

impl BootFirmware {
    /// Firmware description отсутствует.
    pub const NONE: Self = Self {
        kind: BOOT_FIRMWARE_NONE,
        _reserved: 0,
        root: 0,
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

/// Полная структура BootInfo (текущая версия — [`BOOT_INFO_VERSION`] = 3).
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

    /// Linear framebuffer (может быть нулевым).
    pub framebuffer: BootFramebuffer,
    /// Ранняя UART-консоль, выбранная загрузчиком из platform description.
    pub console: BootConsole,
    /// ACPI RSDP либо FDT — без архитектурного предположения в общем ABI.
    pub firmware: BootFirmware,

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
const _: () = assert!(core::mem::size_of::<BootConsole>() == 24);
const _: () = assert!(core::mem::size_of::<BootFirmware>() == 16);
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
        if !matches!(
            self.console.kind,
            BOOT_CONSOLE_NONE
                | BOOT_CONSOLE_16550_PORT
                | BOOT_CONSOLE_16550_MMIO
                | BOOT_CONSOLE_PL011
        ) || !matches!(
            self.firmware.kind,
            BOOT_FIRMWARE_NONE | BOOT_FIRMWARE_ACPI | BOOT_FIRMWARE_DEVICE_TREE
        ) {
            return false;
        }
        if (self.console.kind == BOOT_CONSOLE_NONE) != (self.console.base == 0)
            || (self.firmware.kind == BOOT_FIRMWARE_NONE) != (self.firmware.root == 0)
            || self.console.flags != 0
            || self.firmware._reserved != 0
            || (self.console.kind == BOOT_CONSOLE_16550_PORT && self.console.base > u16::MAX as u64)
        {
            return false;
        }
        let framebuffer = &self.framebuffer;
        if framebuffer.phys_addr == 0 {
            if framebuffer.width != 0
                || framebuffer.height != 0
                || framebuffer.stride != 0
                || framebuffer.bpp != 0
            {
                return false;
            }
        } else if framebuffer.width == 0
            || framebuffer.height == 0
            || framebuffer.bpp != 32
            || !framebuffer.stride.is_multiple_of(4)
            || framebuffer.stride < framebuffer.width.saturating_mul(4)
            || !matches!(
                framebuffer.format,
                FRAMEBUFFER_FORMAT_RGB | FRAMEBUFFER_FORMAT_BGR
            )
            || !matches!(
                framebuffer._reserved,
                FRAMEBUFFER_SOURCE_UEFI_GOP | FRAMEBUFFER_SOURCE_GRUB
            )
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemRegion;

    fn empty_info() -> BootInfo {
        BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            _pad: 0,
            memmap_count: 0,
            _pad2: 0,
            memmap: [MemRegion::ZERO; MEMMAP_MAX_REGIONS],
            framebuffer: BootFramebuffer::ZERO,
            console: BootConsole::NONE,
            firmware: BootFirmware::NONE,
            initramfs: BootInitramfs {
                phys_addr: 0,
                size: 0,
            },
            kernel_phys: 0,
            kernel_size: 0,
            boot_stack: BootStack { top: 0, size: 0 },
        }
    }

    #[test]
    fn platform_descriptors_are_validated_as_pairs() {
        let mut info = empty_info();
        assert!(info.validate());

        info.console.kind = BOOT_CONSOLE_PL011;
        assert!(!info.validate(), "MMIO console without base must fail");
        info.console.base = 0x0900_0000;
        assert!(info.validate());

        info.firmware.kind = BOOT_FIRMWARE_DEVICE_TREE;
        assert!(!info.validate(), "Device Tree kind without root must fail");
        info.firmware.root = 0x4000_0000;
        assert!(info.validate());
    }

    #[test]
    fn reserved_platform_bits_are_rejected() {
        let mut info = empty_info();
        info.console.flags = 1;
        assert!(!info.validate());
        info.console.flags = 0;
        info.firmware._reserved = 1;
        assert!(!info.validate());
    }

    #[test]
    fn framebuffer_layout_and_source_are_validated() {
        let mut info = empty_info();
        info.framebuffer = BootFramebuffer {
            phys_addr: 0x8000_0000,
            width: 1280,
            height: 800,
            stride: 1280 * 4,
            bpp: 32,
            format: FRAMEBUFFER_FORMAT_BGR,
            _reserved: FRAMEBUFFER_SOURCE_GRUB,
        };
        assert!(info.validate());
        info.framebuffer.stride -= 4;
        assert!(!info.validate());
        info.framebuffer.stride += 4;
        info.framebuffer._reserved = 99;
        assert!(!info.validate());
    }
}

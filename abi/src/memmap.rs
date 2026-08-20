//! Упрощённое представление физической карты памяти, передаваемое загрузчиком
//! ядру.
//!
//! UEFI передаёт карту в собственном формате (EFI_MEMORY_DESCRIPTOR) с
//! виртуальными адресами и флагами атрибутов. Для ядра достаточно трёх фактов
//! о каждом диапазоне: физический адрес, размер и допустимость использования.
//! Поэтому загрузчик *нормализует* карту в плоский массив [`MemRegion`]:
//! это уменьшает поверхность ABI и делает разбор карты независимым от деталей
//! UEFI-формата (документировано в docs/adr/0002).

/// Максимум регионов в карте памяти, передаваемой ядру.
///
/// UEFI-карты на реальных системах содержат десятки регионов; 256 — запас с
/// огромным избытком. Ограничение фиксировано, чтобы BootInfo имел постоянный
/// размер и ядро не нуждалось в выделении памяти под саму карту на раннем этапе.
pub const MEMMAP_MAX_REGIONS: usize = 256;

/// Тип региона физической памяти.
///
/// Значения совпадают с семантикой UEFI-типов, но перечислены заново:
/// ABI не должен зависеть от версии UEFI-спецификации.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemRegionKind {
    /// Свободная конвенциональная память (EfiConventionalMemory).
    Usable = 1,
    /// Резерв firmware/драйверов (EfiReservedMemoryType).
    Reserved = 2,
    /// Память ACPI таблиц (EfiACPIReclaimMemory).
    AcpiReclaim = 3,
    /// Низкая память BIOS (EfiACPIMemoryNVS).
    AcpiNvs = 4,
    /// Память, помеченная дефектной (EfiMemoryMappedIO — *не* RAM!).
    Mmio = 5,
    /// Память, которая принадлежит runtime-сервисам UEFI.
    RuntimeServices = 6,
}

/// Плоский регион физической памяти.
///
/// # Layout
///
/// `u32 kind + u32 _pad + u64 phys_start + u64 size = 24 байта`, 8-байтовое
/// выравнивание. Проверено compile-time (см. конец модуля).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MemRegion {
    /// Тип региона.
    pub kind: u32,
    /// Запасной padding (выравнивание phys_start под 8 байт).
    pub _pad: u32,
    /// Физический адрес начала региона.
    pub phys_start: u64,
    /// Размер региона в байтах (всегда кратно [`crate::PAGE_SIZE`]).
    pub size: u64,
}

impl MemRegion {
    /// Нулевой регион (заглушка для не заполненных слотов массива).
    pub const ZERO: Self = Self {
        kind: 0,
        _pad: 0,
        phys_start: 0,
        size: 0,
    };

    /// Проверка, что значение `kind` соответствует известному типу.
    ///
    /// Дискриминаторы [`MemRegionKind`] — непрерывный диапазон `1..=6`;
    /// при расширении перечисления этот диапазон нужно обновить.
    #[inline]
    pub fn is_valid_kind(kind: u32) -> bool {
        matches!(kind, 1..=6)
    }

    /// Физический адрес конца региона (не включая его).
    #[inline]
    pub fn phys_end(&self) -> u64 {
        self.phys_start + self.size
    }
}

impl From<MemRegionKind> for u32 {
    fn from(kind: MemRegionKind) -> Self {
        kind as u32
    }
}

// Compile-time проверка layout ABI: размер и выравнивание не могут измениться
// без явного изменения этого модуля.
const _: () = assert!(core::mem::size_of::<MemRegion>() == 24);
const _: () = assert!(core::mem::align_of::<MemRegion>() == 8);

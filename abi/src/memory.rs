//! Пользовательский ABI виртуальной и разделяемой памяти.

/// Версия структур memory ABI.
pub const MEMORY_ABI_VERSION: u32 = 1;

/// Права пользовательского отображения.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmFlags(pub u64);

impl VmFlags {
    /// Отображение доступно для чтения.
    pub const READ: Self = Self(1 << 0);
    /// Отображение доступно для записи.
    pub const WRITE: Self = Self(1 << 1);
    /// Отображение содержит исполняемый код.
    pub const EXECUTE: Self = Self(1 << 2);

    /// Проверяет наличие всех указанных флагов.
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Возвращает объединение наборов флагов.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Запрос анонимного отображения zero-filled страниц.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VmMapRequest {
    /// [`MEMORY_ABI_VERSION`].
    pub version: u32,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u32,
    /// Желаемый page-aligned адрес либо ноль для автоматического выбора.
    pub address: u64,
    /// Размер отображения, кратный размеру страницы.
    pub length: u64,
    /// [`VmFlags`]; W+X запрещено политикой ядра.
    pub flags: VmFlags,
}

/// Запрос создания физического объекта shared memory.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SharedMemoryCreate {
    /// [`MEMORY_ABI_VERSION`].
    pub version: u32,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u32,
    /// Размер объекта, кратный размеру страницы.
    pub length: u64,
    /// Максимальные разрешённые права будущих отображений.
    pub flags: VmFlags,
}

/// Запрос отображения диапазона shared-memory object.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SharedMemoryMap {
    /// [`MEMORY_ABI_VERSION`].
    pub version: u32,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u32,
    /// Желаемый адрес либо ноль для автоматического выбора.
    pub address: u64,
    /// Page-aligned смещение от начала shared-memory object.
    pub offset: u64,
    /// Размер отображаемого диапазона.
    pub length: u64,
    /// Права конкретного отображения.
    pub flags: VmFlags,
}

const _: () = assert!(core::mem::size_of::<VmFlags>() == 8);
const _: () = assert!(core::mem::size_of::<VmMapRequest>() == 32);
const _: () = assert!(core::mem::size_of::<SharedMemoryCreate>() == 24);
const _: () = assert!(core::mem::size_of::<SharedMemoryMap>() == 40);

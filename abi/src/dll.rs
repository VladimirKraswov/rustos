//! Метаданные динамических библиотек RustOS.
//!
//! Файл с расширением `.dll` остаётся стандартным ELF64 `ET_DYN`. Импорты,
//! экспорты и релокации задаются обычными `.dynamic`/`.dynsym`/`.rela.*`, а
//! секция `.rustos.module` содержит этот небольшой descriptor. Благодаря
//! стандартному ELF инструменты не приходится писать заново.

/// Магия descriptor'а: ASCII `RUSTDLL\0` в little-endian.
pub const DLL_MAGIC: u64 = u64::from_le_bytes(*b"RUSTDLL\0");
/// Версия формата [`ModuleDescriptor`].
pub const DLL_ABI_VERSION: u32 = 1;
/// Максимальная длина SONAME вместе с завершающим NUL.
pub const DLL_SONAME_BYTES: usize = 48;

/// Значения [`ModuleDescriptor::kind`].
pub mod kind {
    /// Обычная shared library с вызываемыми функциями.
    pub const LIBRARY: u16 = 1;
    /// Исполняемое приложение PIE.
    pub const EXECUTABLE: u16 = 2;
    /// Тонкий client runtime системного сервиса.
    pub const SERVICE_CLIENT: u16 = 3;
}

/// Флаги [`ModuleDescriptor::flags`].
pub mod flags {
    /// Модуль можно загружать только один раз в процесс.
    pub const SINGLETON: u32 = 1 << 0;
    /// Имеется функция инициализации по `init_rva`.
    pub const HAS_INIT: u32 = 1 << 1;
    /// Имеется функция завершения по `fini_rva`.
    pub const HAS_FINI: u32 = 1 << 2;
    /// Модуль использует static TLS и требует TLS reservation loader'а.
    pub const HAS_TLS: u32 = 1 << 3;
}

/// Descriptor секции `.rustos.module` ELF64-модуля.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ModuleDescriptor {
    /// [`DLL_MAGIC`].
    pub magic: u64,
    /// [`DLL_ABI_VERSION`].
    pub abi_version: u32,
    /// Размер descriptor'а для совместимого расширения.
    pub descriptor_size: u16,
    /// Одно из значений модуля [`kind`].
    pub kind: u16,
    /// Маска из модуля [`flags`].
    pub flags: u32,
    /// Минимальная версия RustOS system ABI.
    pub min_system_abi: u32,
    /// Major-версия публичного ABI библиотеки.
    pub version_major: u16,
    /// Minor-версия публичного ABI библиотеки.
    pub version_minor: u16,
    /// Patch-версия реализации.
    pub version_patch: u16,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u16,
    /// NUL-terminated UTF-8 SONAME, например `vfs-1.dll`.
    pub soname: [u8; DLL_SONAME_BYTES],
    /// Relative virtual address функции `extern "C" fn() -> i32` или ноль.
    pub init_rva: u64,
    /// Relative virtual address функции `extern "C" fn()` или ноль.
    pub fini_rva: u64,
}

const _: () = assert!(core::mem::size_of::<ModuleDescriptor>() == 96);

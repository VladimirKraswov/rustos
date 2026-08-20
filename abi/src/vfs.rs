//! Версионированный протокол между `vfs.dll` и user-space сервером `vfsd`.
//!
//! Пути и file payload не встраиваются в IPC-сообщение. Клиент передаёт
//! capability на shared buffer и диапазон внутри него. Это одинаково хорошо
//! работает для коротких имён и многомегабайтных файлов компилятора.

use crate::Handle;

/// Версия VFS service protocol.
pub const VFS_ABI_VERSION: u32 = 1;

/// Коды операций VFS protocol.
pub mod opcode {
    /// Открыть файл или каталог относительно directory capability.
    pub const OPEN: u16 = 1;
    /// Закрыть file description; сам handle также можно закрыть syscall'ом.
    pub const CLOSE: u16 = 2;
    /// Прочитать данные в shared buffer.
    pub const READ: u16 = 3;
    /// Записать данные из shared buffer.
    pub const WRITE: u16 = 4;
    /// Получить метаданные объекта.
    pub const STAT: u16 = 5;
    /// Прочитать следующую порцию directory entries.
    pub const READ_DIR: u16 = 6;
    /// Создать каталог.
    pub const MAKE_DIR: u16 = 7;
    /// Удалить имя из каталога.
    pub const UNLINK: u16 = 8;
    /// Атомарно переименовать или переместить объект.
    pub const RENAME: u16 = 9;
    /// Синхронизировать файл или весь mount с устройством.
    pub const SYNC: u16 = 10;
}

/// Ссылка на UTF-8 путь внутри shared-memory buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PathRef {
    /// Shared-memory capability.
    pub buffer: Handle,
    /// Зарезервировано для выравнивания, должно быть нулём.
    pub reserved: u32,
    /// Смещение первого байта пути в buffer.
    pub offset: u64,
    /// Длина пути без завершающего NUL.
    pub length: u32,
    /// Флаги кодировки; в v1 должно быть нулём (UTF-8).
    pub flags: u32,
}

/// Запрос открытия пути.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct OpenRequest {
    /// Directory capability или [`Handle::INVALID`] для process root.
    pub directory: Handle,
    /// Флаги из модуля [`open_flags`].
    pub open_flags: u32,
    /// Путь относительно `directory`.
    pub path: PathRef,
    /// Желаемые права нового file capability.
    pub requested_rights: u64,
}

/// Запрос потокового чтения или записи.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IoRequest {
    /// Capability открытого файла.
    pub file: Handle,
    /// Shared-memory capability для данных.
    pub buffer: Handle,
    /// Смещение в shared buffer.
    pub buffer_offset: u64,
    /// Максимальное число байт операции.
    pub length: u64,
    /// Смещение в файле или `u64::MAX` для текущей позиции description.
    pub file_offset: u64,
}

/// Унифицированный ответ VFS.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Reply {
    /// Ноль при успехе, отрицательный код ошибки при отказе.
    pub status: i32,
    /// Тип возвращаемого объекта; 0 — объект не возвращается. Набор
    /// значений определяется протоколом VFS (см. docs/VFS.md).
    pub object_kind: u32,
    /// Новый file/directory capability либо [`Handle::INVALID`].
    pub handle: Handle,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u32,
    /// Число обработанных байт или размер объекта.
    pub value: u64,
    /// Дополнительное versioned значение операции.
    pub auxiliary: u64,
}

/// Флаги [`OpenRequest::open_flags`].
pub mod open_flags {
    /// Открыть для чтения.
    pub const READ: u32 = 1 << 0;
    /// Открыть для записи.
    pub const WRITE: u32 = 1 << 1;
    /// Создать отсутствующий файл.
    pub const CREATE: u32 = 1 << 2;
    /// Требовать отсутствия файла при CREATE.
    pub const EXCLUSIVE: u32 = 1 << 3;
    /// Обрезать существующий файл до нулевой длины.
    pub const TRUNCATE: u32 = 1 << 4;
    /// Позиционировать каждую запись в конец файла.
    pub const APPEND: u32 = 1 << 5;
    /// Требовать каталог, а не обычный файл.
    pub const DIRECTORY: u32 = 1 << 6;
}

const _: () = assert!(core::mem::size_of::<PathRef>() == 24);
const _: () = assert!(core::mem::size_of::<OpenRequest>() == 40);
const _: () = assert!(core::mem::size_of::<IoRequest>() == 32);
const _: () = assert!(core::mem::size_of::<Reply>() == 32);

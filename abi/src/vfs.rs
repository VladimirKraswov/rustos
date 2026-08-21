//! Версионированный RPC-протокол `vfs.dll <-> vfsd`.
//!
//! IPC остаётся control plane: пути и file payload находятся в shared memory.
//! `VfsObject` является непрозрачным server-side file description, а не
//! kernel handle; `vfsd` связывает его с PID, поэтому чужой token бесполезен.

/// Версия VFS service protocol.
pub const VFS_ABI_VERSION: u16 = 2;

/// Непрозрачный идентификатор открытого объекта внутри конкретной сессии vfsd.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsObject(
    /// Wire-значение; клиент не интерпретирует его как индекс или указатель.
    pub u64,
);

impl VfsObject {
    /// Начальный каталог namespace, выданного клиенту.
    pub const ROOT: Self = Self(0);
    /// Sentinel, который никогда не обозначает открытый объект.
    pub const INVALID: Self = Self(u64::MAX);
}

/// Коды операций в поле `MessageHeader.opcode`.
pub mod opcode {
    /// Открыть существующий объект или создать его по флагам.
    pub const OPEN: u16 = 1;
    /// Закрыть file description.
    pub const CLOSE: u16 = 2;
    /// Прочитать байты в shared-memory buffer.
    pub const READ: u16 = 3;
    /// Записать байты из shared-memory buffer.
    pub const WRITE: u16 = 4;
    /// Получить тип и размер объекта.
    pub const STAT: u16 = 5;
    /// Прочитать bounded набор [`super::DirectoryEntry`].
    pub const READ_DIR: u16 = 6;
    /// Создать каталог.
    pub const MAKE_DIR: u16 = 7;
    /// Удалить файл либо пустой каталог.
    pub const UNLINK: u16 = 8;
    /// Атомарно переименовать или переместить объект.
    pub const RENAME: u16 = 9;
    /// Зафиксировать накопленные изменения на устройстве.
    pub const SYNC: u16 = 10;
    /// Изменить текущую позицию file description.
    pub const SEEK: u16 = 11;
    /// Изменить логический размер открытого файла (`File::set_len`).
    pub const RESIZE: u16 = 12;
    /// Test/supervisor control: корректно синхронизировать и завершить vfsd.
    pub const SHUTDOWN: u16 = 0xff;
}

/// Стабильные результаты VFS RPC; отрицательные значения обозначают ошибку.
pub mod status {
    /// Операция завершена успешно.
    pub const OK: i32 = 0;
    /// Неверные флаги, диапазон, UTF-8 или сочетание аргументов.
    pub const INVALID_ARGUMENT: i32 = -100;
    /// Объект по указанному пути не найден.
    pub const NOT_FOUND: i32 = -101;
    /// Создаваемый объект уже существует.
    pub const ALREADY_EXISTS: i32 = -102;
    /// Компонент пути не является каталогом.
    pub const NOT_DIRECTORY: i32 = -103;
    /// Операция над файлом неприменима к каталогу.
    pub const IS_DIRECTORY: i32 = -104;
    /// Каталог нельзя удалить, пока он содержит записи.
    pub const NOT_EMPTY: i32 = -105;
    /// Namespace или объект открыт только для чтения.
    pub const READ_ONLY: i32 = -106;
    /// На постоянном устройстве не осталось места.
    pub const NO_SPACE: i32 = -107;
    /// `VfsObject` закрыт, устарел либо принадлежит другой сессии.
    pub const BAD_OBJECT: i32 = -108;
    /// Ошибка нижележащего block/filesystem backend.
    pub const IO: i32 = -109;
    /// Исчерпан bounded лимит объектов, записей или длины пути.
    pub const LIMIT_REACHED: i32 = -110;
    /// Нарушена версия или структура RPC.
    pub const PROTOCOL: i32 = -111;
}

/// Значения типа объекта в ответах и directory records.
pub mod object_kind {
    /// Объект отсутствует либо тип не применим.
    pub const NONE: u32 = 0;
    /// Обычный файл.
    pub const FILE: u32 = 1;
    /// Каталог.
    pub const DIRECTORY: u32 = 2;
}

/// Флаги открытия объекта.
pub mod open_flags {
    /// Разрешить чтение.
    pub const READ: u32 = 1 << 0;
    /// Разрешить запись.
    pub const WRITE: u32 = 1 << 1;
    /// Создать объект, если его нет.
    pub const CREATE: u32 = 1 << 2;
    /// Вместе с [`CREATE`] потребовать отсутствия объекта.
    pub const EXCLUSIVE: u32 = 1 << 3;
    /// После открытия установить размер файла в ноль.
    pub const TRUNCATE: u32 = 1 << 4;
    /// Каждую запись выполнять в текущий конец файла.
    pub const APPEND: u32 = 1 << 5;
    /// Требовать, чтобы результат был каталогом.
    pub const DIRECTORY: u32 = 1 << 6;
}

/// База смещения для операции seek.
pub mod seek_from {
    /// От начала файла.
    pub const START: u32 = 0;
    /// От текущей позиции description.
    pub const CURRENT: u32 = 1;
    /// От текущего конца файла.
    pub const END: u32 = 2;
}

/// Путь лежит в переданном shared-memory object. Capability самого объекта —
/// `message.handles[1]`; slot 0 зарезервирован reply endpoint'у.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PathRequest {
    /// Каталог, относительно которого разрешается путь.
    pub directory: VfsObject,
    /// Смещение UTF-8 пути внутри shared-memory object.
    pub path_offset: u64,
    /// Длина пути в байтах без завершающего NUL.
    pub path_length: u32,
    /// Флаги конкретной операции; неизвестные биты запрещены.
    pub flags: u32,
}

/// Запрос открытия пути.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct OpenRequest {
    /// Каталог, относительно которого разрешается путь.
    pub directory: VfsObject,
    /// Смещение UTF-8 пути внутри shared memory.
    pub path_offset: u64,
    /// Длина пути в байтах.
    pub path_length: u32,
    /// Маска из [`open_flags`].
    pub open_flags: u32,
    /// При отправке равно нулю.
    pub reserved: u64,
}

/// Потоковый запрос чтения или записи.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IoRequest {
    /// Открытый файл.
    pub file: VfsObject,
    /// Начало bounded диапазона внутри shared memory.
    pub buffer_offset: u64,
    /// Максимальное число передаваемых байтов.
    pub length: u64,
    /// `u64::MAX` использует и обновляет текущую позицию description.
    pub file_offset: u64,
}

/// Запрос изменения текущей позиции файла.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SeekRequest {
    /// Открытый файл.
    pub file: VfsObject,
    /// Знаковое смещение относительно выбранной базы.
    pub offset: i64,
    /// Одно из значений [`seek_from`].
    pub whence: u32,
    /// При отправке равно нулю.
    pub reserved: u32,
}

/// Запрос изменения длины файла. Расширение создаёт sparse-диапазон,
/// читающийся как нули; уменьшение сразу возвращает лишние блоки allocator'у.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ResizeRequest {
    /// Открытый для записи файл.
    pub file: VfsObject,
    /// Новый логический размер в байтах.
    pub length: u64,
    /// При отправке равно нулю.
    pub reserved: u64,
}

/// Запрос атомарного переименования между двумя каталогами.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RenameRequest {
    /// Исходный каталог.
    pub old_directory: VfsObject,
    /// Целевой каталог.
    pub new_directory: VfsObject,
    /// Смещение старого имени в shared memory.
    pub old_offset: u64,
    /// Смещение нового имени в shared memory.
    pub new_offset: u64,
    /// Длина старого имени в байтах.
    pub old_length: u32,
    /// Длина нового имени в байтах.
    pub new_length: u32,
    /// Версионированные флаги rename; в v2 должны быть нулевыми.
    pub flags: u32,
    /// При отправке равно нулю.
    pub reserved: u32,
}

/// Унифицированный inline-ответ.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Reply {
    /// Одно из значений [`status`].
    pub status: i32,
    /// Одно из значений [`object_kind`].
    pub object_kind: u32,
    /// Новый/затронутый объект либо [`VfsObject::INVALID`].
    pub object: VfsObject,
    /// Bytes processed, size, position либо число directory records.
    pub value: u64,
    /// Дополнительное operation-specific значение.
    pub auxiliary: u64,
}

impl Reply {
    /// Безопасное начальное значение до получения ответа сервиса.
    pub const EMPTY: Self = Self {
        status: status::PROTOCOL,
        object_kind: object_kind::NONE,
        object: VfsObject::INVALID,
        value: 0,
        auxiliary: 0,
    };
}

/// Один `readdir` record в shared memory. Имя не NUL-terminated.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirectoryEntry {
    /// Идентификатор объекта для последующей операции в той же сессии.
    pub object: VfsObject,
    /// Логический размер файла; для каталога равен нулю.
    pub size: u64,
    /// Одно из значений [`object_kind`].
    pub kind: u32,
    /// Число значимых UTF-8 байтов в [`Self::name`].
    pub name_length: u16,
    /// При отправке равно нулю.
    pub reserved: u16,
    /// UTF-8 имя без NUL; хвост после `name_length` обнулён.
    pub name: [u8; 232],
}

impl DirectoryEntry {
    /// Пустая запись для предварительного заполнения shared buffer.
    pub const EMPTY: Self = Self {
        object: VfsObject::INVALID,
        size: 0,
        kind: object_kind::NONE,
        name_length: 0,
        reserved: 0,
        name: [0; 232],
    };
}

const _: () = assert!(core::mem::size_of::<PathRequest>() == 24);
const _: () = assert!(core::mem::size_of::<OpenRequest>() == 32);
const _: () = assert!(core::mem::size_of::<IoRequest>() == 32);
const _: () = assert!(core::mem::size_of::<SeekRequest>() == 24);
const _: () = assert!(core::mem::size_of::<ResizeRequest>() == 24);
const _: () = assert!(core::mem::size_of::<RenameRequest>() == 48);
const _: () = assert!(core::mem::size_of::<Reply>() == 32);
const _: () = assert!(core::mem::size_of::<DirectoryEntry>() == 256);

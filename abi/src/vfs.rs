//! Версионированный RPC-протокол `vfs.dll <-> vfsd`.
//!
//! IPC остаётся control plane: пути и file payload находятся в shared memory.
//! `VfsObject` является непрозрачным server-side file description, а не
//! kernel handle; `vfsd` связывает его с PID, поэтому чужой token бесполезен.

#![allow(missing_docs)] // Полная таблица операций приведена в docs/VFS.md.

/// Версия VFS service protocol.
pub const VFS_ABI_VERSION: u16 = 2;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsObject(pub u64);

impl VfsObject {
    pub const ROOT: Self = Self(0);
    pub const INVALID: Self = Self(u64::MAX);
}

pub mod opcode {
    pub const OPEN: u16 = 1;
    pub const CLOSE: u16 = 2;
    pub const READ: u16 = 3;
    pub const WRITE: u16 = 4;
    pub const STAT: u16 = 5;
    pub const READ_DIR: u16 = 6;
    pub const MAKE_DIR: u16 = 7;
    pub const UNLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const SYNC: u16 = 10;
    pub const SEEK: u16 = 11;
    /// Test/supervisor control: корректно синхронизировать и завершить vfsd.
    pub const SHUTDOWN: u16 = 0xff;
}

pub mod status {
    pub const OK: i32 = 0;
    pub const INVALID_ARGUMENT: i32 = -100;
    pub const NOT_FOUND: i32 = -101;
    pub const ALREADY_EXISTS: i32 = -102;
    pub const NOT_DIRECTORY: i32 = -103;
    pub const IS_DIRECTORY: i32 = -104;
    pub const NOT_EMPTY: i32 = -105;
    pub const READ_ONLY: i32 = -106;
    pub const NO_SPACE: i32 = -107;
    pub const BAD_OBJECT: i32 = -108;
    pub const IO: i32 = -109;
    pub const LIMIT_REACHED: i32 = -110;
    pub const PROTOCOL: i32 = -111;
}

pub mod object_kind {
    pub const NONE: u32 = 0;
    pub const FILE: u32 = 1;
    pub const DIRECTORY: u32 = 2;
}

pub mod open_flags {
    pub const READ: u32 = 1 << 0;
    pub const WRITE: u32 = 1 << 1;
    pub const CREATE: u32 = 1 << 2;
    pub const EXCLUSIVE: u32 = 1 << 3;
    pub const TRUNCATE: u32 = 1 << 4;
    pub const APPEND: u32 = 1 << 5;
    pub const DIRECTORY: u32 = 1 << 6;
}

pub mod seek_from {
    pub const START: u32 = 0;
    pub const CURRENT: u32 = 1;
    pub const END: u32 = 2;
}

/// Путь лежит в переданном shared-memory object. Capability самого объекта —
/// `message.handles[1]`; slot 0 зарезервирован reply endpoint'у.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PathRequest {
    pub directory: VfsObject,
    pub path_offset: u64,
    pub path_length: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct OpenRequest {
    pub directory: VfsObject,
    pub path_offset: u64,
    pub path_length: u32,
    pub open_flags: u32,
    pub reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IoRequest {
    pub file: VfsObject,
    pub buffer_offset: u64,
    pub length: u64,
    /// `u64::MAX` использует и обновляет текущую позицию description.
    pub file_offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SeekRequest {
    pub file: VfsObject,
    pub offset: i64,
    pub whence: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RenameRequest {
    pub old_directory: VfsObject,
    pub new_directory: VfsObject,
    pub old_offset: u64,
    pub new_offset: u64,
    pub old_length: u32,
    pub new_length: u32,
    pub flags: u32,
    pub reserved: u32,
}

/// Унифицированный inline-ответ.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Reply {
    pub status: i32,
    pub object_kind: u32,
    pub object: VfsObject,
    /// Bytes processed, size, position либо число directory records.
    pub value: u64,
    pub auxiliary: u64,
}

impl Reply {
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
    pub object: VfsObject,
    pub size: u64,
    pub kind: u32,
    pub name_length: u16,
    pub reserved: u16,
    pub name: [u8; 232],
}

impl DirectoryEntry {
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
const _: () = assert!(core::mem::size_of::<RenameRequest>() == 48);
const _: () = assert!(core::mem::size_of::<Reply>() == 32);
const _: () = assert!(core::mem::size_of::<DirectoryEntry>() == 256);

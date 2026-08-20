//! Однонаправленные byte-stream pipes для stdio и build tools.

/// Версия pipe ABI.
pub const PIPE_ABI_VERSION: u32 = 1;

/// Результат `pipe_create`: два capability handles с разными правами.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PipeCreateResult {
    /// Endpoint чтения (`READ | TRANSFER`).
    pub reader: crate::Handle,
    /// Endpoint записи (`WRITE | TRANSFER`).
    pub writer: crate::Handle,
    /// [`PIPE_ABI_VERSION`].
    pub version: u32,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u32,
}

const _: () = assert!(core::mem::size_of::<PipeCreateResult>() == 16);

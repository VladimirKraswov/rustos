//! Минимальный capability ABI блочных устройств.
//!
//! Эти вызовы предназначены для изолированных `blockd`/filesystem services,
//! а не для обычных приложений. Буфер обязан целиком лежать в user mappings;
//! kernel driver копирует данные через bounded DMA bounce page.

use crate::PAGE_SIZE;

/// Текущая версия протокола блочного устройства.
pub const BLOCK_ABI_VERSION: u32 = 1;
/// Логический блок RustOS: одна общая страница AMD64/AArch64.
pub const LOGICAL_BLOCK_SIZE: u64 = PAGE_SIZE;

/// Один bounded запрос чтения или записи блочному сервису.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BlockIoRequest {
    /// Должно быть равно [`BLOCK_ABI_VERSION`].
    pub version: u32,
    /// Флаги операции; неизвестные биты получатель обязан отвергнуть.
    pub flags: u32,
    /// Номер логического 4-KiB блока, не 512-байтного hardware sector.
    pub block: u64,
    /// User virtual address буфера.
    pub buffer_address: u64,
    /// В v1 разрешена одна страница за syscall. Streaming выполняется
    /// последовательными запросами без неограниченного kernel allocation.
    pub block_count: u32,
    /// При отправке равно нулю.
    pub reserved: u32,
}

const _: () = assert!(core::mem::size_of::<BlockIoRequest>() == 32);

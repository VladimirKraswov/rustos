//! Минимальный capability ABI блочных устройств.
//!
//! Эти вызовы предназначены для изолированных `blockd`/filesystem services,
//! а не для обычных приложений. Буфер обязан целиком лежать в user mappings;
//! kernel driver копирует данные через bounded DMA bounce page.

#![allow(missing_docs)] // Поля образуют одну компактную таблицу ABI.

use crate::PAGE_SIZE;

pub const BLOCK_ABI_VERSION: u32 = 1;
pub const LOGICAL_BLOCK_SIZE: u64 = PAGE_SIZE;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BlockIoRequest {
    pub version: u32,
    pub flags: u32,
    /// Номер логического 4-KiB блока, не 512-байтного hardware sector.
    pub block: u64,
    /// User virtual address буфера.
    pub buffer_address: u64,
    /// В v1 разрешена одна страница за syscall. Streaming выполняется
    /// последовательными запросами без неограниченного kernel allocation.
    pub block_count: u32,
    pub reserved: u32,
}

const _: () = assert!(core::mem::size_of::<BlockIoRequest>() == 32);

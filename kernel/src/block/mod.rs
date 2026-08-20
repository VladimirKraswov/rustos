//! Kernel bootstrap block transport.
//!
//! Filesystem semantics здесь отсутствуют. Модуль владеет только одним
//! virtio-blk устройством и экспортирует bounded 4-KiB операции capability
//! syscall'ам. После появления user-space PCI/DMA API этот backend без смены
//! ABI переедет в `virtioblkd`, а kernel оставит IOMMU/IRQ primitives.

// На AArch64 transport пока является честной заглушкой, поэтому часть
// аппаратных ошибок там ещё не конструируется.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum BlockError {
    Unsupported,
    InvalidRange,
    OutOfMemory,
    Device,
    Timeout,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    pub blocks: u64,
    pub transport: &'static str,
}

#[cfg(target_arch = "x86_64")]
#[path = "virtio_legacy.rs"]
mod platform;

#[cfg(target_arch = "aarch64")]
mod platform {
    use super::{BlockError, BlockInfo};

    pub fn initialize() -> Result<BlockInfo, BlockError> {
        Err(BlockError::Unsupported)
    }
    pub fn info() -> Result<BlockInfo, BlockError> {
        Err(BlockError::Unsupported)
    }
    pub fn read_block(_block: u64, _output: &mut [u8; 4096]) -> Result<(), BlockError> {
        Err(BlockError::Unsupported)
    }
    pub fn write_block(_block: u64, _input: &[u8; 4096]) -> Result<(), BlockError> {
        Err(BlockError::Unsupported)
    }
    pub fn flush() -> Result<(), BlockError> {
        Err(BlockError::Unsupported)
    }
}

pub use platform::{flush, info, initialize, read_block, write_block};

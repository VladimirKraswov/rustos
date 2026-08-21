//! Kernel bootstrap block transport.
//!
//! Filesystem semantics здесь отсутствуют. Модуль владеет только одним
//! virtio-blk устройством и экспортирует bounded 4-KiB операции capability
//! syscall'ам. После появления user-space PCI/DMA API этот backend без смены
//! ABI переедет в `virtioblkd`, а kernel оставит IOMMU/IRQ primitives.

// Конкретный транспорт выбирается на границе платформы: legacy PCI на
// эталонном x86 QEMU и modern virtio-mmio на QEMU `virt` AArch64.
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
#[path = "virtio_mmio.rs"]
mod platform;

pub use platform::{flush, info, initialize, read_block, write_block};

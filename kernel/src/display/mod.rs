//! Kernel display drivers и их аппаратно-независимая граница.
//!
//! `graphics` владеет software surfaces и compositor'ом, а этот модуль —
//! конкретным scanout-устройством. Такое разделение позволит вынести драйвер
//! в user-space `displayd`, не перенося туда rasterizer и оконную логику.

mod edid;
#[cfg(target_arch = "x86_64")]
mod pci;
pub mod scanout;
mod virtio_gpu;
#[cfg(target_arch = "x86_64")]
mod virtqueue;
#[cfg(target_arch = "aarch64")]
mod virtqueue_mmio;

pub use virtio_gpu::VirtioGpu;

/// Ошибки transport-слоя не зависят от шины: GPU protocol одинаков поверх
/// modern PCI на AMD64 и modern MMIO на эталонной ARM-платформе QEMU `virt`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransportError {
    Unsupported,
    InvalidConfiguration,
    OutOfMemory,
    RejectedFeatures,
    Timeout,
    Busy,
    DeviceError,
}

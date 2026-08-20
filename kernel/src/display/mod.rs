//! Kernel display drivers и их аппаратно-независимая граница.
//!
//! `graphics` владеет software surfaces и compositor'ом, а этот модуль —
//! конкретным scanout-устройством. Такое разделение позволит вынести драйвер
//! в user-space `displayd`, не перенося туда rasterizer и оконную логику.

#[cfg(target_arch = "x86_64")]
mod edid;
#[cfg(target_arch = "x86_64")]
mod pci;
#[cfg(target_arch = "x86_64")]
mod virtio_gpu;
#[cfg(target_arch = "x86_64")]
mod virtqueue;

#[cfg(target_arch = "x86_64")]
pub use virtio_gpu::VirtioGpu;

#[cfg(target_arch = "aarch64")]
pub struct VirtioGpu;

#[cfg(target_arch = "aarch64")]
impl VirtioGpu {
    /// AArch64 сохраняет тот же display contract, но PCI transport там
    /// не предполагается. Будущий virtio-mmio backend заменит только
    /// эту transport-границу, не `graphics` и не compositor.
    pub fn initialize(
        _fallback: rustos_video::DisplayMode,
    ) -> Result<Self, rustos_video::ModeSetError> {
        Err(rustos_video::ModeSetError::DeviceLost)
    }

    pub const fn mode(&self) -> rustos_video::DisplayMode {
        rustos_video::DisplayMode {
            width: 0,
            height: 0,
            stride_pixels: 0,
            format: rustos_video::PixelFormat::Bgr888,
            refresh_millihertz: 0,
        }
    }

    pub const fn connector(&self) -> rustos_video::ConnectorInfo {
        rustos_video::ConnectorInfo {
            kind: rustos_video::ConnectorKind::Unknown,
            connected: false,
            preferred_mode: self.mode(),
            width_mm: 0,
            height_mm: 0,
        }
    }

    pub const fn capabilities(&self) -> rustos_video::ScanoutCapabilities {
        rustos_video::ScanoutCapabilities {
            page_flip: false,
            vsync_event: false,
            hardware_cursor: false,
            multiple_outputs: false,
        }
    }

    pub fn modes(&self, _output: &mut [rustos_video::DisplayMode]) -> usize {
        0
    }

    pub fn set_mode(
        &mut self,
        _requested: rustos_video::DisplayMode,
    ) -> Result<rustos_video::DisplayMode, rustos_video::ModeSetError> {
        Err(rustos_video::ModeSetError::UnsupportedMode)
    }

    pub fn present(
        &mut self,
        _source: rustos_video::Surface<'_>,
        _damage: &[rustos_video::Rect],
        _sequence: u64,
    ) -> Result<rustos_video::PresentStats, rustos_video::ScanoutError> {
        Err(rustos_video::ScanoutError::DeviceLost)
    }
}

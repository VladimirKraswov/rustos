//! Контракт между compositor'ом и конкретным display/scanout driver.

use crate::{PixelFormat, Rect, Surface};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub stride_pixels: u32,
    pub format: PixelFormat,
    /// 0 означает, что firmware/driver не сообщил refresh rate.
    pub refresh_millihertz: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanoutCapabilities {
    pub page_flip: bool,
    pub vsync_event: bool,
    pub hardware_cursor: bool,
    pub multiple_outputs: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentStats {
    pub sequence: u64,
    pub rectangles: u32,
    pub pixels: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanoutError {
    InvalidSurface,
    UnsupportedFormat,
    DeviceLost,
}

/// Минимальный интерфейс display driver. GOP реализует только immediate copy;
/// будущий virtio/GPU backend сможет вернуть page-flip/vsync capabilities,
/// сохранив тот же surface/damage контракт compositor'а.
pub trait Scanout {
    fn mode(&self) -> DisplayMode;
    fn capabilities(&self) -> ScanoutCapabilities;
    fn present(
        &mut self,
        source: Surface<'_>,
        damage: &[Rect],
        sequence: u64,
    ) -> Result<PresentStats, ScanoutError>;
}

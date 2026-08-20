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

/// Пользовательский профиль цветности software renderer'а. Физический
/// scanout остаётся XRGB/BGRX8888: packed RGB888 намеренно не используется,
/// чтобы не терять выравнивание и возможность wide stores.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    TrueColor24,
    HighColor16,
    Grayscale8,
}

impl ColorMode {
    pub const fn pixel_format(self) -> PixelFormat {
        match self {
            Self::TrueColor24 => PixelFormat::Rgb888,
            Self::HighColor16 => PixelFormat::Rgb565,
            Self::Grayscale8 => PixelFormat::Grayscale8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorKind {
    /// Видеорежим настроен firmware/GRUB и после hand-off неизменяем.
    FirmwareFramebuffer,
    /// Виртуальный monitor (например, будущий virtio-gpu backend).
    Virtual,
    DisplayPort,
    Hdmi,
    EmbeddedPanel,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectorInfo {
    pub kind: ConnectorKind,
    pub connected: bool,
    pub preferred_mode: DisplayMode,
    /// 0, если EDID/driver не передал физический размер панели.
    pub width_mm: u16,
    pub height_mm: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeSetError {
    UnsupportedMode,
    /// Для нового scanout и software surfaces не удалось выделить память.
    OutOfMemory,
    /// Bootstrap framebuffer не имеет runtime mode-set API. Режим доступен
    /// через меню загрузчика и будет применён после перезапуска.
    RequiresReboot,
    DeviceLost,
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

/// Минимальный интерфейс display driver. Firmware framebuffer реализует только
/// immediate copy; будущий virtio/GPU backend сможет вернуть page-flip/vsync
/// capabilities, сохранив тот же surface/damage контракт compositor'а.
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

/// Расширение `Scanout` для monitor drivers. Compositor зависит только от
/// `Scanout`; display manager дополнительно получает enumeration/mode-set.
pub trait DisplayDriver: Scanout {
    fn connector(&self) -> ConnectorInfo;
    /// Записывает доступные режимы в caller-owned массив и возвращает число
    /// записей. Это no_std API без обязательного heap allocation.
    fn modes(&self, output: &mut [DisplayMode]) -> usize;
    fn set_mode(&mut self, requested: DisplayMode) -> Result<DisplayMode, ModeSetError>;
}

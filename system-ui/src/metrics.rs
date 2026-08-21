//! Метрики окна и перевод logical units в физические пиксели.
//!
//! Layout приложения не должен знать размер framebuffer. Он работает в
//! логических единицах, а raster backend применяет device scale перед тем,
//! как построить контуры, glyph mask и display surface. Compositor получает
//! уже готовую поверхность физического размера и потому не растягивает bitmap.

/// Единичный масштаб в fixed-point формате с тремя десятичными знаками.
pub const SCALE_MILLI_ONE: u16 = 1_000;

/// Связь логической области окна с его физической raster surface.
///
/// Floating point намеренно не используется: тип остаётся дешёвым и
/// детерминированным в `no_std`, kernel и раннем user space. Значение
/// `device_scale_milli = 1_600` означает коэффициент `1.6`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowMetrics {
    logical_width: u32,
    logical_height: u32,
    physical_width: u32,
    physical_height: u32,
    device_scale_milli: u16,
}

impl WindowMetrics {
    /// Создаёт метрики поверхности, которая выводится строго пиксель-в-пиксель.
    pub const fn one_to_one(width: u32, height: u32) -> Self {
        Self {
            logical_width: width,
            logical_height: height,
            physical_width: width,
            physical_height: height,
            device_scale_milli: SCALE_MILLI_ONE,
        }
    }

    /// Создаёт HiDPI-метрики из физического размера и device scale.
    ///
    /// Логический размер вычисляется с округлением к ближайшей единице.
    /// Нулевой размер или нулевой scale возвращает `None`.
    pub fn from_physical(
        physical_width: u32,
        physical_height: u32,
        device_scale_milli: u16,
    ) -> Option<Self> {
        if physical_width == 0 || physical_height == 0 || device_scale_milli == 0 {
            return None;
        }
        let logical_width = physical_to_logical(physical_width, device_scale_milli);
        let logical_height = physical_to_logical(physical_height, device_scale_milli);
        if logical_width == 0 || logical_height == 0 {
            return None;
        }
        Some(Self {
            logical_width,
            logical_height,
            physical_width,
            physical_height,
            device_scale_milli,
        })
    }

    /// Ширина окна, которую видит layout приложения.
    pub const fn logical_width(self) -> u32 {
        self.logical_width
    }

    /// Высота окна, которую видит layout приложения.
    pub const fn logical_height(self) -> u32 {
        self.logical_height
    }

    /// Ширина raster surface в физических пикселях.
    pub const fn physical_width(self) -> u32 {
        self.physical_width
    }

    /// Высота raster surface в физических пикселях.
    pub const fn physical_height(self) -> u32 {
        self.physical_height
    }

    /// Device scale в тысячных долях (`1_600` соответствует `1.6`).
    pub const fn device_scale_milli(self) -> u16 {
        self.device_scale_milli
    }

    /// Scale compositor'а.
    ///
    /// RustOS растрирует клиентскую поверхность сразу в physical size, поэтому
    /// compositor всегда копирует её `1:1` и не интерполирует готовый bitmap.
    pub const fn compositor_scale_milli(self) -> u16 {
        SCALE_MILLI_ONE
    }

    /// Переводит логический размер/координату в physical pixels.
    pub fn logical_to_physical(self, value: u32) -> u32 {
        scale_round(value, self.device_scale_milli)
    }

    /// Переводит физический размер/координату в logical units.
    pub fn physical_to_logical(self, value: u32) -> u32 {
        physical_to_logical(value, self.device_scale_milli)
    }
}

fn scale_round(value: u32, scale_milli: u16) -> u32 {
    let scaled = u64::from(value) * u64::from(scale_milli) + 500;
    (scaled / u64::from(SCALE_MILLI_ONE)).min(u64::from(u32::MAX)) as u32
}

fn physical_to_logical(value: u32, scale_milli: u16) -> u32 {
    let scale = u64::from(scale_milli);
    let scaled = u64::from(value) * u64::from(SCALE_MILLI_ONE) + scale / 2;
    (scaled / scale).min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_to_one_never_requests_compositor_scaling() {
        let metrics = WindowMetrics::one_to_one(1280, 800);
        assert_eq!(metrics.logical_width(), 1280);
        assert_eq!(metrics.physical_width(), 1280);
        assert_eq!(metrics.device_scale_milli(), 1_000);
        assert_eq!(metrics.compositor_scale_milli(), 1_000);
    }

    #[test]
    fn fractional_device_scale_happens_before_rasterization() {
        let metrics = WindowMetrics::from_physical(2048, 1280, 1_600).unwrap();
        assert_eq!(
            (metrics.logical_width(), metrics.logical_height()),
            (1280, 800)
        );
        assert_eq!(metrics.logical_to_physical(120), 192);
        assert_eq!(metrics.logical_to_physical(36), 58);
        assert_eq!(metrics.physical_to_logical(192), 120);
        assert_eq!(metrics.compositor_scale_milli(), 1_000);
    }

    #[test]
    fn invalid_physical_metrics_are_rejected() {
        assert!(WindowMetrics::from_physical(0, 800, 1_000).is_none());
        assert!(WindowMetrics::from_physical(1280, 800, 0).is_none());
    }
}

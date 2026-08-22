//! Чистая политика выбора начального видеорежима.
//!
//! Драйвер сообщает физический/preferred mode, а этот модуль выбирает
//! комфортную logical surface для первого рабочего стола. Политика не знает
//! о PCI, EDID wire format и конкретном compositor'е, поэтому её можно
//! проверять обычными host-тестами.

use crate::DisplayMode;

/// Ограничения bootstrap desktop до появления полноценного HiDPI layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupModePolicy {
    pub max_width: u32,
    pub max_height: u32,
    pub min_width: u32,
    pub min_height: u32,
    /// Максимальный целочисленный scale, который имеет смысл искать.
    pub max_integer_scale: u32,
}

impl StartupModePolicy {
    pub const fn desktop(max_width: u32, max_height: u32) -> Self {
        Self {
            max_width,
            max_height,
            min_width: 800,
            min_height: 540,
            max_integer_scale: 6,
        }
    }
}

/// Выбирает рекомендуемый startup mode в порядке качества:
///
/// 1. native/preferred, если он уже укладывается в UI budget;
/// 2. точная целая доля native surface (Retina 2x/3x без интерполяции);
/// 3. aspect-preserving fit в UI budget;
/// 4. валидный firmware fallback.
pub fn select_startup_mode(
    preferred: DisplayMode,
    fallback: DisplayMode,
    policy: StartupModePolicy,
) -> DisplayMode {
    if usable(preferred, policy)
        && preferred.width <= policy.max_width
        && preferred.height <= policy.max_height
    {
        return normalized(preferred);
    }

    if usable(preferred, policy) {
        for scale in 2..=policy.max_integer_scale.max(2) {
            if !preferred.width.is_multiple_of(scale) || !preferred.height.is_multiple_of(scale) {
                continue;
            }
            let width = preferred.width / scale;
            let height = preferred.height / scale;
            if width <= policy.max_width
                && height <= policy.max_height
                && width >= policy.min_width
                && height >= policy.min_height
            {
                return with_size(preferred, width, height);
            }
        }

        if let Some((width, height)) = aspect_fit(preferred.width, preferred.height, policy) {
            return with_size(preferred, width, height);
        }
    }

    if usable(fallback, policy)
        && fallback.width <= policy.max_width
        && fallback.height <= policy.max_height
    {
        return normalized(fallback);
    }

    // Последний безопасный вариант не превышает policy даже в тестовых
    // конфигурациях с меньшим лимитом. Формат/refresh наследуются от fallback.
    let width = 1280.min(policy.max_width).max(policy.min_width);
    let height = 800.min(policy.max_height).max(policy.min_height);
    with_size(fallback, width, height)
}

fn usable(mode: DisplayMode, policy: StartupModePolicy) -> bool {
    mode.width >= policy.min_width && mode.height >= policy.min_height
}

fn normalized(mode: DisplayMode) -> DisplayMode {
    DisplayMode {
        stride_pixels: mode.width,
        ..mode
    }
}

fn with_size(mode: DisplayMode, width: u32, height: u32) -> DisplayMode {
    DisplayMode {
        width,
        height,
        stride_pixels: width,
        ..mode
    }
}

fn aspect_fit(width: u32, height: u32, policy: StartupModePolicy) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || policy.max_width == 0 || policy.max_height == 0 {
        return None;
    }
    let width_limited = u64::from(width) * u64::from(policy.max_height)
        > u64::from(height) * u64::from(policy.max_width);
    let (fit_width, fit_height) = if width_limited {
        let fit_height = u64::from(height)
            .checked_mul(u64::from(policy.max_width))?
            .checked_div(u64::from(width))? as u32;
        (policy.max_width, fit_height)
    } else {
        let fit_width = u64::from(width)
            .checked_mul(u64::from(policy.max_height))?
            .checked_div(u64::from(height))? as u32;
        (fit_width, policy.max_height)
    };
    // Чётные размеры удобнее для будущих YUV surfaces и не создают
    // однопиксельный перекос при центрировании letterbox.
    let fit_width = fit_width & !1;
    let fit_height = fit_height & !1;
    (fit_width >= policy.min_width && fit_height >= policy.min_height)
        .then_some((fit_width, fit_height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CpuPixelFormat;

    fn mode(width: u32, height: u32) -> DisplayMode {
        DisplayMode {
            width,
            height,
            stride_pixels: width,
            format: CpuPixelFormat::Bgr888,
            refresh_millihertz: 60_000,
        }
    }

    const POLICY: StartupModePolicy = StartupModePolicy::desktop(1600, 900);

    #[test]
    fn keeps_native_mode_inside_budget() {
        assert_eq!(
            select_startup_mode(mode(1280, 800), mode(1024, 768), POLICY),
            mode(1280, 800)
        );
    }

    #[test]
    fn chooses_exact_retina_scale_before_fractional_fit() {
        assert_eq!(
            select_startup_mode(mode(2880, 1800), mode(1024, 768), POLICY),
            mode(1440, 900)
        );
        assert_eq!(
            select_startup_mode(mode(2048, 1280), mode(1024, 768), POLICY),
            mode(1024, 640)
        );
    }

    #[test]
    fn never_clamps_only_one_axis() {
        let selected = select_startup_mode(mode(3440, 1440), mode(1024, 768), POLICY);
        assert_eq!((selected.width, selected.height), (1600, 668));
        let source_ratio = 3440.0 / 1440.0;
        let selected_ratio = selected.width as f64 / selected.height as f64;
        assert!((source_ratio - selected_ratio).abs() < 0.01);
    }

    #[test]
    fn uses_firmware_fallback_for_invalid_preferred() {
        assert_eq!(
            select_startup_mode(mode(0, 0), mode(1280, 720), POLICY),
            mode(1280, 720)
        );
    }
}

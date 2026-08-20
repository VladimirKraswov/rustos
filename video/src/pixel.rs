//! Форматы пикселей и integer alpha blending.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn mix(self, other: Self, amount: u8) -> Self {
        let alpha = amount as u16;
        let inverse = 255 - alpha;
        Self {
            r: ((self.r as u16 * inverse + other.r as u16 * alpha) / 255) as u8,
            g: ((self.g as u16 * inverse + other.g as u16 * alpha) / 255) as u8,
            b: ((self.b as u16 * inverse + other.b as u16 * alpha) / 255) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(color: Color) -> Self {
        Self::new(color.r, color.g, color.b, 255)
    }
}

/// 32-bit форматы, достаточные для GOP scanout, окон и изображений с alpha.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    /// Байты в памяти: R, G, B, reserved (UEFI PixelRedGreenBlueReserved8Bit).
    Rgb888,
    /// Байты в памяти: B, G, R, reserved (UEFI PixelBlueGreenRedReserved8Bit).
    Bgr888,
    /// Числовое представление `0xAARRGGBB`, удобно для декодеров/иконок.
    Argb8888,
    /// Логический high-color профиль 5:6:5. Значение хранится в младших
    /// 16 битах u32 surface: это сохраняет выравнивание и быстрые spans.
    Rgb565,
    /// Логический 8-битный grayscale профиль в младшем байте u32 surface.
    Grayscale8,
}

impl PixelFormat {
    pub const fn pack(self, value: Rgba) -> u32 {
        match self {
            Self::Rgb888 => value.r as u32 | ((value.g as u32) << 8) | ((value.b as u32) << 16),
            Self::Bgr888 => value.b as u32 | ((value.g as u32) << 8) | ((value.r as u32) << 16),
            Self::Argb8888 => {
                ((value.a as u32) << 24)
                    | ((value.r as u32) << 16)
                    | ((value.g as u32) << 8)
                    | value.b as u32
            }
            Self::Rgb565 => {
                (((value.r as u32 * 31 + 127) / 255) << 11)
                    | (((value.g as u32 * 63 + 127) / 255) << 5)
                    | ((value.b as u32 * 31 + 127) / 255)
            }
            Self::Grayscale8 => {
                // Integer BT.601 luminance; сумма коэффициентов равна 256.
                (77 * value.r as u32 + 150 * value.g as u32 + 29 * value.b as u32 + 128) >> 8
            }
        }
    }

    pub const fn unpack(self, raw: u32) -> Rgba {
        match self {
            Self::Rgb888 => Rgba::new(raw as u8, (raw >> 8) as u8, (raw >> 16) as u8, 255),
            Self::Bgr888 => Rgba::new((raw >> 16) as u8, (raw >> 8) as u8, raw as u8, 255),
            Self::Argb8888 => Rgba::new(
                (raw >> 16) as u8,
                (raw >> 8) as u8,
                raw as u8,
                (raw >> 24) as u8,
            ),
            Self::Rgb565 => {
                let red = (raw >> 11) & 0x1f;
                let green = (raw >> 5) & 0x3f;
                let blue = raw & 0x1f;
                Rgba::new(
                    ((red * 255 + 15) / 31) as u8,
                    ((green * 255 + 31) / 63) as u8,
                    ((blue * 255 + 15) / 31) as u8,
                    255,
                )
            }
            Self::Grayscale8 => {
                let value = raw as u8;
                Rgba::new(value, value, value, 255)
            }
        }
    }

    pub const fn pack_color(self, color: Color) -> u32 {
        self.pack(Rgba::opaque(color))
    }
}

pub(crate) fn blend(source: Rgba, destination: Rgba, opacity: u8) -> Rgba {
    let alpha = (u16::from(source.a) * u16::from(opacity) + 127) / 255;
    let inverse = 255 - alpha;
    let channel =
        |src: u8, dst: u8| ((u16::from(src) * alpha + u16::from(dst) * inverse + 127) / 255) as u8;
    Rgba::new(
        channel(source.r, destination.r),
        channel(source.g, destination.g),
        channel(source.b, destination.b),
        255,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip_channels() {
        let pixel = Rgba::new(12, 34, 56, 78);
        for format in [PixelFormat::Rgb888, PixelFormat::Bgr888] {
            let unpacked = format.unpack(format.pack(pixel));
            assert_eq!(unpacked, Rgba::new(12, 34, 56, 255));
        }
        assert_eq!(
            PixelFormat::Argb8888.unpack(PixelFormat::Argb8888.pack(pixel)),
            pixel
        );
    }

    #[test]
    fn reduced_colour_profiles_are_explicit_and_aligned() {
        let source = Rgba::new(240, 128, 16, 255);
        let rgb565 = PixelFormat::Rgb565.unpack(PixelFormat::Rgb565.pack(source));
        assert!(rgb565.r.abs_diff(source.r) <= 8);
        assert!(rgb565.g.abs_diff(source.g) <= 4);
        assert!(rgb565.b.abs_diff(source.b) <= 8);

        let gray = PixelFormat::Grayscale8.unpack(PixelFormat::Grayscale8.pack(source));
        assert_eq!(gray.r, gray.g);
        assert_eq!(gray.g, gray.b);
    }

    #[test]
    fn alpha_blend_uses_source_and_global_opacity() {
        let result = blend(Rgba::new(200, 100, 0, 128), Rgba::new(0, 20, 100, 255), 128);
        assert_eq!(result, Rgba::new(50, 40, 75, 255));
    }
}

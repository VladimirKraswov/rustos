//! Встроенные CPU-friendly обои.

use rustos_video::Color;

/// Идентификатор системных обоев.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WallpaperId {
    /// Весенняя река на рассвете.
    #[default]
    SpringRiver,
    /// Осенний лес и река.
    AutumnRiver,
    /// Зимнее поле и замёрзшая река.
    WinterField,
}

/// Неизменяемое RGB565-изображение. Формат вдвое компактнее XRGB и
/// декодируется без PNG/JPEG allocator'а в раннем desktop service.
#[derive(Clone, Copy)]
pub struct Wallpaper {
    /// Идентификатор.
    pub id: WallpaperId,
    /// Имя для UI.
    pub name: &'static str,
    /// Ширина исходной текстуры.
    pub width: u32,
    /// Высота исходной текстуры.
    pub height: u32,
    pixels: &'static [u8],
}

impl Wallpaper {
    /// Возвращает RGB-пиксель, обрезая координаты к размеру изображения.
    pub fn pixel(self, x: u32, y: u32) -> Color {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let offset = ((y * self.width + x) * 2) as usize;
        let packed = u16::from_le_bytes([self.pixels[offset], self.pixels[offset + 1]]);
        let r = ((packed >> 11) & 0x1f) as u8;
        let g = ((packed >> 5) & 0x3f) as u8;
        let b = (packed & 0x1f) as u8;
        Color::rgb(
            (r << 3) | (r >> 2),
            (g << 2) | (g >> 4),
            (b << 3) | (b >> 2),
        )
    }
}

const SPRING: &[u8] = include_bytes!("../assets/wallpapers/packed/spring-river.rgb565");
const AUTUMN: &[u8] = include_bytes!("../assets/wallpapers/packed/autumn-river.rgb565");
const WINTER: &[u8] = include_bytes!("../assets/wallpapers/packed/winter-field.rgb565");

/// Три базовых природных изображения.
pub const WALLPAPERS: [Wallpaper; 3] = [
    Wallpaper {
        id: WallpaperId::SpringRiver,
        name: "spring",
        width: 640,
        height: 360,
        pixels: SPRING,
    },
    Wallpaper {
        id: WallpaperId::AutumnRiver,
        name: "autumn",
        width: 640,
        height: 360,
        pixels: AUTUMN,
    },
    Wallpaper {
        id: WallpaperId::WinterField,
        name: "winter",
        width: 640,
        height: 360,
        pixels: WINTER,
    },
];

/// Находит встроенные обои.
pub const fn wallpaper(id: WallpaperId) -> Wallpaper {
    match id {
        WallpaperId::SpringRiver => WALLPAPERS[0],
        WallpaperId::AutumnRiver => WALLPAPERS[1],
        WallpaperId::WinterField => WALLPAPERS[2],
    }
}

const _: () = assert!(SPRING.len() == 640 * 360 * 2);
const _: () = assert!(AUTUMN.len() == 640 * 360 * 2);
const _: () = assert!(WINTER.len() == 640 * 360 * 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_wallpapers_have_valid_corner_pixels() {
        for image in WALLPAPERS {
            let _ = image.pixel(0, 0);
            let _ = image.pixel(image.width - 1, image.height - 1);
            let _ = image.pixel(u32::MAX, u32::MAX);
        }
    }
}

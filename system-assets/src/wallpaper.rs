//! Встроенные обои в полном системном качестве.

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

/// Неизменяемое 4×4 block-compressed изображение. Каждый блок содержит два
/// RGB565 endpoint и 2-bit индекс каждого texel: random access не требует
/// декодировать целый файл или выделять heap, а HD texture занимает 450 KiB.
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
        let (palette, indices) = self.block(x, y);
        select_block_color(palette, indices, x, y)
    }

    fn block(self, x: u32, y: u32) -> ([Color; 4], u32) {
        let blocks_per_row = self.width.div_ceil(4);
        let block = ((y / 4) * blocks_per_row + x / 4) as usize * 8;
        let first = u16::from_le_bytes([self.pixels[block], self.pixels[block + 1]]);
        let second = u16::from_le_bytes([self.pixels[block + 2], self.pixels[block + 3]]);
        let indices = u32::from_le_bytes([
            self.pixels[block + 4],
            self.pixels[block + 5],
            self.pixels[block + 6],
            self.pixels[block + 7],
        ]);
        let first = unpack_rgb565(first);
        let second = unpack_rgb565(second);
        (
            [
                first,
                second,
                interpolate(first, second, 2, 1),
                interpolate(first, second, 1, 2),
            ],
            indices,
        )
    }

    /// Bilinear sample из координат 16.16. Интерполяция выполняется до
    /// упаковки в scanout format: при любом нецелом scale исчезают крупные
    /// nearest-neighbor блоки, но локальный RGB565/gray режим по-прежнему
    /// остаётся явным пользовательским выбором renderer'а.
    pub fn pixel_bilinear(self, x_16: u64, y_16: u64) -> Color {
        let x0 = ((x_16 >> 16) as u32).min(self.width.saturating_sub(1));
        let y0 = ((y_16 >> 16) as u32).min(self.height.saturating_sub(1));
        let x1 = x0.saturating_add(1).min(self.width.saturating_sub(1));
        let y1 = y0.saturating_add(1).min(self.height.saturating_sub(1));
        let x_amount = ((x_16 & 0xffff) >> 8) as u8;
        let y_amount = ((y_16 & 0xffff) >> 8) as u8;
        let (top_left, top_right, bottom_left, bottom_right) =
            if x0 / 4 == x1 / 4 && y0 / 4 == y1 / 4 {
                // При близком к 1:1 масштабе четыре texel почти всегда лежат
                // в одном блоке. Декодируем endpoints один раз вместо четырёх.
                let (palette, indices) = self.block(x0, y0);
                (
                    select_block_color(palette, indices, x0, y0),
                    select_block_color(palette, indices, x1, y0),
                    select_block_color(palette, indices, x0, y1),
                    select_block_color(palette, indices, x1, y1),
                )
            } else {
                (
                    self.pixel(x0, y0),
                    self.pixel(x1, y0),
                    self.pixel(x0, y1),
                    self.pixel(x1, y1),
                )
            };
        let top = top_left.mix(top_right, x_amount);
        let bottom = bottom_left.mix(bottom_right, x_amount);
        top.mix(bottom, y_amount)
    }
}

const SPRING: &[u8] = include_bytes!("../assets/wallpapers/packed/spring-river.rbc1");
const AUTUMN: &[u8] = include_bytes!("../assets/wallpapers/packed/autumn-river.rbc1");
const WINTER: &[u8] = include_bytes!("../assets/wallpapers/packed/winter-field.rbc1");

/// Три базовых природных изображения.
pub const WALLPAPERS: [Wallpaper; 3] = [
    Wallpaper {
        id: WallpaperId::SpringRiver,
        name: "spring",
        width: 1280,
        height: 720,
        pixels: SPRING,
    },
    Wallpaper {
        id: WallpaperId::AutumnRiver,
        name: "autumn",
        width: 1280,
        height: 720,
        pixels: AUTUMN,
    },
    Wallpaper {
        id: WallpaperId::WinterField,
        name: "winter",
        width: 1280,
        height: 720,
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

const _: () = assert!(SPRING.len() == 1280 / 4 * (720 / 4) * 8);
const _: () = assert!(AUTUMN.len() == 1280 / 4 * (720 / 4) * 8);
const _: () = assert!(WINTER.len() == 1280 / 4 * (720 / 4) * 8);

fn unpack_rgb565(value: u16) -> Color {
    let red = u32::from((value >> 11) & 0x1f);
    let green = u32::from((value >> 5) & 0x3f);
    let blue = u32::from(value & 0x1f);
    Color::rgb(
        ((red * 255 + 15) / 31) as u8,
        ((green * 255 + 31) / 63) as u8,
        ((blue * 255 + 15) / 31) as u8,
    )
}

fn interpolate(first: Color, second: Color, left: u16, right: u16) -> Color {
    let channel = |a: u8, b: u8| ((u16::from(a) * left + u16::from(b) * right + 1) / 3) as u8;
    Color::rgb(
        channel(first.r, second.r),
        channel(first.g, second.g),
        channel(first.b, second.b),
    )
}

fn select_block_color(palette: [Color; 4], indices: u32, x: u32, y: u32) -> Color {
    let local = (y % 4) * 4 + x % 4;
    palette[((indices >> (local * 2)) & 3) as usize]
}

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

    #[test]
    fn bilinear_sampling_blends_neighbouring_pixels() {
        let image = WALLPAPERS[0];
        let left = image.pixel(10, 10);
        let right = image.pixel(11, 10);
        let middle = image.pixel_bilinear((10 << 16) | 0x8000, 10 << 16);
        for (value, a, b) in [
            (middle.r, left.r, right.r),
            (middle.g, left.g, right.g),
            (middle.b, left.b, right.b),
        ] {
            assert!(value >= a.min(b) && value <= a.max(b));
        }
    }
}

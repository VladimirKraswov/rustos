//! Семантические курсоры и их темы.

use rustos_abi::input::PointerCursor;
use rustos_video::Color;

use crate::{PackId, PackMetadata, ResourcePack};

/// Максимальный холст встроенного курсора.
pub const CURSOR_EXTENT: u16 = 24;

/// Роль пикселя в cursor sprite. Цвет берётся из палитры активной темы.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorPixel {
    /// Прозрачный пиксель.
    Transparent,
    /// Мягкая тень.
    Shadow,
    /// Контур.
    Outline,
    /// Основная заливка.
    Fill,
    /// Акцент или активный кадр анимации.
    Accent,
}

/// Палитра cursor pack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorPalette {
    /// Тень.
    pub shadow: Color,
    /// Контур.
    pub outline: Color,
    /// Заливка.
    pub fill: Color,
    /// Акцент.
    pub accent: Color,
}

/// Снимок курсора: семантика, кадр анимации, размер и hotspot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorImage {
    /// Семантический вид.
    pub kind: PointerCursor,
    /// Кадр циклической анимации.
    pub frame: u8,
    /// Ширина sprite.
    pub width: u16,
    /// Высота sprite.
    pub height: u16,
    /// X точки, совпадающей с pointer position.
    pub hotspot_x: i16,
    /// Y точки, совпадающей с pointer position.
    pub hotspot_y: i16,
}

impl CursorImage {
    /// Создаёт 24×24 sprite с правильным hotspot для семантики.
    pub const fn new(kind: PointerCursor, frame: u8) -> Self {
        let (hotspot_x, hotspot_y) = match kind {
            PointerCursor::Arrow | PointerCursor::Link => (2, 2),
            PointerCursor::Grab | PointerCursor::Grabbing => (11, 10),
            _ => (12, 12),
        };
        Self {
            kind,
            frame: frame % 8,
            width: CURSOR_EXTENT,
            height: CURSOR_EXTENT,
            hotspot_x,
            hotspot_y,
        }
    }
}

/// Подключаемая тема курсоров. Геометрию можно заменить собственной функцией
/// rasterizer; приложения при этом продолжат использовать [`PointerCursor`].
#[derive(Clone, Copy)]
pub struct CursorPack {
    metadata: PackMetadata,
    /// Цвета темы.
    pub palette: CursorPalette,
    rasterizer: fn(CursorImage, u16, u16) -> CursorPixel,
}

impl CursorPack {
    /// Создаёт пользовательский cursor pack. Пакет можно установить в
    /// [`crate::PackRegistry`] и удалить без изменения приложений.
    pub const fn new(
        metadata: PackMetadata,
        palette: CursorPalette,
        rasterizer: fn(CursorImage, u16, u16) -> CursorPixel,
    ) -> Self {
        Self {
            metadata,
            palette,
            rasterizer,
        }
    }

    /// Возвращает пиксель sprite в координатах изображения.
    pub fn pixel(self, image: CursorImage, x: u16, y: u16) -> CursorPixel {
        if x >= image.width || y >= image.height {
            CursorPixel::Transparent
        } else {
            (self.rasterizer)(image, x, y)
        }
    }
}

impl ResourcePack for CursorPack {
    fn metadata(&self) -> PackMetadata {
        self.metadata
    }
}

/// Светлая современная тема.
pub const LIGHT_CURSOR_PACK: CursorPack = CursorPack {
    metadata: PackMetadata {
        id: PackId(0x1001),
        name: "light",
        version: 1,
    },
    palette: CursorPalette {
        shadow: Color::rgb(0, 0, 0),
        outline: Color::rgb(8, 14, 22),
        fill: Color::rgb(246, 250, 252),
        accent: Color::rgb(57, 202, 220),
    },
    rasterizer: standard_cursor,
};

/// Тёмная тема для светлых обоев.
pub const MIDNIGHT_CURSOR_PACK: CursorPack = CursorPack {
    metadata: PackMetadata {
        id: PackId(0x1002),
        name: "midnight",
        version: 1,
    },
    palette: CursorPalette {
        shadow: Color::rgb(246, 250, 252),
        outline: Color::rgb(238, 245, 250),
        fill: Color::rgb(18, 27, 39),
        accent: Color::rgb(255, 191, 82),
    },
    rasterizer: standard_cursor,
};

/// Контрастная тема доступности.
pub const HIGH_CONTRAST_CURSOR_PACK: CursorPack = CursorPack {
    metadata: PackMetadata {
        id: PackId(0x1003),
        name: "contrast",
        version: 1,
    },
    palette: CursorPalette {
        shadow: Color::rgb(0, 0, 0),
        outline: Color::rgb(0, 0, 0),
        fill: Color::rgb(255, 255, 255),
        accent: Color::rgb(255, 225, 0),
    },
    rasterizer: standard_cursor,
};

fn standard_cursor(image: CursorImage, x: u16, y: u16) -> CursorPixel {
    let x = i32::from(x);
    let y = i32::from(y);
    match image.kind {
        PointerCursor::Arrow => arrow(x, y),
        PointerCursor::Text => text(x, y),
        PointerCursor::Link => hand(x, y, false),
        PointerCursor::Grab => hand(x, y, false),
        PointerCursor::Grabbing => hand(x, y, true),
        PointerCursor::Busy => busy(x, y, i32::from(image.frame)),
        PointerCursor::Crosshair => crosshair(x, y),
        PointerCursor::NotAllowed => not_allowed(x, y),
        PointerCursor::ResizeHorizontal => resize_horizontal(x, y),
        PointerCursor::ResizeVertical => resize_vertical(x, y),
        PointerCursor::ResizeNwSe => resize_diagonal(x, y, false),
        PointerCursor::ResizeNeSw => resize_diagonal(x, y, true),
    }
}

fn arrow(x: i32, y: i32) -> CursorPixel {
    let main = (1..=17).contains(&y) && x >= 1 && x <= y / 2 + 2;
    let stem = (12..=21).contains(&y) && (7..=10).contains(&x);
    if !main && !stem {
        return if (3..=22).contains(&y) && x >= 3 && x <= y / 2 + 4 {
            CursorPixel::Shadow
        } else {
            CursorPixel::Transparent
        };
    }
    let boundary = x <= 2 || x > y / 2 || y <= 2 || (stem && (x == 7 || x == 10));
    if boundary {
        CursorPixel::Outline
    } else {
        CursorPixel::Fill
    }
}

fn text(x: i32, y: i32) -> CursorPixel {
    let bar = ((5..=18).contains(&x) && (y == 3 || y == 20))
        || ((10..=13).contains(&x) && (3..=20).contains(&y));
    let inside = (11..=12).contains(&x) && (5..=18).contains(&y);
    if inside {
        CursorPixel::Fill
    } else if bar {
        CursorPixel::Outline
    } else {
        CursorPixel::Transparent
    }
}

fn hand(x: i32, y: i32, closed: bool) -> CursorPixel {
    let palm_top = if closed { 8 } else { 10 };
    let palm = (6..=17).contains(&x) && (palm_top..=20).contains(&y);
    let finger = if closed {
        (7..=16).contains(&x) && (5..=11).contains(&y)
    } else {
        ((10..=13).contains(&x) && (2..=13).contains(&y))
            || ((6..=9).contains(&x) && (7..=14).contains(&y))
            || ((14..=17).contains(&x) && (8..=14).contains(&y))
    };
    if !(palm || finger) {
        return CursorPixel::Transparent;
    }
    if x == 6 || x == 17 || y == 20 || (!closed && y == 2 && (10..=13).contains(&x)) {
        CursorPixel::Outline
    } else if closed && (9..=11).contains(&y) {
        CursorPixel::Accent
    } else {
        CursorPixel::Fill
    }
}

fn busy(x: i32, y: i32, frame: i32) -> CursorPixel {
    let dx = x - 12;
    let dy = y - 12;
    let radius = dx * dx + dy * dy;
    if !(45..=100).contains(&radius) {
        return CursorPixel::Transparent;
    }
    let sector = if dy <= -dx.abs() {
        0
    } else if dx >= dy.abs() {
        2
    } else if dy >= dx.abs() {
        4
    } else {
        6
    } + if dx.signum() == dy.signum() { 1 } else { 0 };
    if sector == frame || sector == (frame + 7) % 8 {
        CursorPixel::Accent
    } else {
        CursorPixel::Fill
    }
}

fn crosshair(x: i32, y: i32) -> CursorPixel {
    let line = (x == 12 && !(9..=15).contains(&y)) || (y == 12 && !(9..=15).contains(&x));
    let center = (x - 12).abs() <= 2 && (y - 12).abs() <= 2;
    if center {
        CursorPixel::Accent
    } else if line {
        CursorPixel::Outline
    } else {
        CursorPixel::Transparent
    }
}

fn not_allowed(x: i32, y: i32) -> CursorPixel {
    let dx = x - 12;
    let dy = y - 12;
    let radius = dx * dx + dy * dy;
    let ring = (70..=105).contains(&radius);
    let slash = (dx - dy).abs() <= 1 && dx.abs() <= 7;
    if slash {
        CursorPixel::Accent
    } else if ring {
        CursorPixel::Outline
    } else {
        CursorPixel::Transparent
    }
}

fn resize_horizontal(x: i32, y: i32) -> CursorPixel {
    let line = (10..=13).contains(&y) && (3..=20).contains(&x);
    let heads = (x <= 7 && (x - 3).abs() >= (y - 11).abs() * 2)
        || (x >= 16 && (20 - x).abs() >= (y - 11).abs() * 2);
    if line || heads {
        if y == 10 || y == 13 || x == 3 || x == 20 {
            CursorPixel::Outline
        } else {
            CursorPixel::Fill
        }
    } else {
        CursorPixel::Transparent
    }
}

fn resize_vertical(x: i32, y: i32) -> CursorPixel {
    resize_horizontal(y, x)
}

fn resize_diagonal(x: i32, y: i32, rising: bool) -> CursorPixel {
    let distance = if rising {
        (x + y - 23).abs()
    } else {
        (x - y).abs()
    };
    let in_span = (3..=20).contains(&x) && (3..=20).contains(&y);
    if in_span && distance <= 2 {
        if distance == 2 {
            CursorPixel::Outline
        } else {
            CursorPixel::Fill
        }
    } else {
        CursorPixel::Transparent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cursor_has_visible_pixels_and_valid_hotspot() {
        let kinds = [
            PointerCursor::Arrow,
            PointerCursor::Text,
            PointerCursor::Link,
            PointerCursor::Grab,
            PointerCursor::Grabbing,
            PointerCursor::Busy,
            PointerCursor::Crosshair,
            PointerCursor::NotAllowed,
            PointerCursor::ResizeHorizontal,
            PointerCursor::ResizeVertical,
            PointerCursor::ResizeNwSe,
            PointerCursor::ResizeNeSw,
        ];
        for kind in kinds {
            let image = CursorImage::new(kind, 0);
            let mut visible = 0;
            for y in 0..image.height {
                for x in 0..image.width {
                    visible += usize::from(
                        LIGHT_CURSOR_PACK.pixel(image, x, y) != CursorPixel::Transparent,
                    );
                }
            }
            assert!(visible > 8, "empty cursor: {kind:?}");
            assert!(image.hotspot_x >= 0 && image.hotspot_x < image.width as i16);
            assert!(image.hotspot_y >= 0 && image.hotspot_y < image.height as i16);
        }
    }

    #[test]
    fn busy_cursor_really_animates() {
        let first = CursorImage::new(PointerCursor::Busy, 0);
        let second = CursorImage::new(PointerCursor::Busy, 1);
        let differs = (0..first.height).any(|y| {
            (0..first.width).any(|x| {
                LIGHT_CURSOR_PACK.pixel(first, x, y) != LIGHT_CURSOR_PACK.pixel(second, x, y)
            })
        });
        assert!(differs);
    }
}

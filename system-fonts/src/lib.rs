//! Единая растеризация системных шрифтов RustOS.
//!
//! Модуль не знает ни о framebuffer, ни о VirGL. Он превращает Unicode-глиф
//! в bounded 8-bit coverage tile с точными bearing/advance. CPU fallback
//! смешивает этот tile напрямую, а `renderd` один раз строит из него SDF и
//! помещает в GPU atlas. Поэтому смена backend не меняет layout текста.

#![no_std]

use core::convert::Infallible;

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{Dimensions, OriginDimensions},
    pixelcolor::{BinaryColor, Gray4, GrayColor},
    prelude::{Pixel, Point, Size},
};
use mplusfonts::{image::Colors, mplus, BitmapFont};
use u8g2_fonts::{
    fonts,
    types::{FontColor, VerticalPosition},
    FontRenderer,
};

const SOURCE_FONT_SIZE: i32 = 24;
const SOURCE_BASELINE: i32 = 25;
const SOURCE_CONSOLE_ADVANCE: i32 = 10;
/// Максимальная сторона одного raster tile.
pub const GLYPH_SIDE: usize = 64;
/// Число coverage samples одного glyph record.
pub const GLYPH_CAPACITY: usize = GLYPH_SIDE * GLYPH_SIDE;

/// Семейство системного шрифта.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Family {
    /// Моноширинный M+ Code.
    Console,
    /// Пропорциональный M+ Sans.
    Sans,
}

/// Начертание.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Weight {
    /// Обычное.
    Regular,
    /// Жирное.
    Bold,
}

/// Renderer-neutral описание глифа.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Style {
    /// Семейство.
    pub family: Family,
    /// Начертание.
    pub weight: Weight,
    /// Программный наклон.
    pub italic: bool,
    /// Em-size в physical pixels.
    pub size: u16,
}

impl Style {
    /// Нормализованный размер 10..=48.
    pub const fn normalized_size(self) -> i32 {
        if self.size < 10 {
            10
        } else if self.size > 48 {
            48
        } else {
            self.size as i32
        }
    }

    /// Высота строки.
    pub const fn line_height(self) -> i32 {
        self.normalized_size() * 4 / 3
    }

    /// Ширина console cell.
    pub const fn cell_width(self) -> i32 {
        let value = SOURCE_CONSOLE_ADVANCE * self.normalized_size() / SOURCE_FONT_SIZE;
        if value < 1 {
            1
        } else {
            value
        }
    }
}

/// Готовый 8-bit coverage tile. Значимы первые `width * height` байт.
#[derive(Clone, Copy)]
pub struct RasterGlyph {
    /// Coverage row-major.
    pub pixels: [u8; GLYPH_CAPACITY],
    /// Ширина tile.
    pub width: u16,
    /// Высота tile.
    pub height: u16,
    /// Смещение tile от pen X.
    pub bearing_x: i16,
    /// Смещение tile от top строки.
    pub bearing_y: i16,
    /// Advance следующего pen.
    pub advance: i16,
}

/// Геометрия glyph без coverage bitmap. GPU display-list builder использует
/// её на каждом кадре, тогда как дорогая растеризация выполняется renderd
/// только при первом попадании glyph/style/color в atlas.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlyphMetrics {
    /// Ширина итогового tile с учётом italic shear.
    pub width: u16,
    /// Высота tile.
    pub height: u16,
    /// Смещение от pen X.
    pub bearing_x: i16,
    /// Смещение от top строки.
    pub bearing_y: i16,
    /// Advance следующего pen.
    pub advance: i16,
}

impl RasterGlyph {
    const fn empty(advance: i32) -> Self {
        Self {
            pixels: [0; GLYPH_CAPACITY],
            width: 0,
            height: 0,
            bearing_x: 0,
            bearing_y: 0,
            advance: advance as i16,
        }
    }
}

type SystemFont = BitmapFont<'static, Gray4, 1>;

fn system_font(style: Style) -> SystemFont {
    match (style.family, style.weight) {
        (Family::Console, Weight::Regular) => mplus!(
            code(100),
            500,
            24,
            true,
            1,
            4,
            '\u{0020}'..='\u{007e}',
            '\u{00a0}'..='\u{00ff}',
            ["�"]
        ),
        (Family::Console, Weight::Bold) => mplus!(
            code(100),
            700,
            24,
            true,
            1,
            4,
            '\u{0020}'..='\u{007e}',
            '\u{00a0}'..='\u{00ff}',
            ["�"]
        ),
        (Family::Sans, Weight::Regular) => mplus!(
            1,
            450,
            24,
            true,
            1,
            4,
            '\u{0020}'..='\u{007e}',
            '\u{00a0}'..='\u{00ff}',
            ["�"]
        ),
        (Family::Sans, Weight::Bold) => mplus!(
            1,
            700,
            24,
            true,
            1,
            4,
            '\u{0020}'..='\u{007e}',
            '\u{00a0}'..='\u{00ff}',
            ["�"]
        ),
    }
}

/// Растеризует один Unicode-глиф. Неподдерживаемый символ заменяется `�`.
pub fn rasterize(character: char, style: Style) -> RasterGlyph {
    if ('\u{0400}'..='\u{052f}').contains(&character) {
        rasterize_cyrillic(character, style)
    } else {
        rasterize_latin(character, style)
    }
}

/// Вычисляет только layout metrics, не разворачивая и не фильтруя bitmap.
pub fn metrics(character: char, style: Style) -> GlyphMetrics {
    if ('\u{0400}'..='\u{052f}').contains(&character) {
        metrics_cyrillic(character, style)
    } else {
        metrics_latin(character, style)
    }
}

/// Возвращает advance без растеризации строки целиком.
pub fn advance(character: char, style: Style) -> i32 {
    if style.family == Family::Console {
        return style.cell_width();
    }
    if ('\u{0400}'..='\u{052f}').contains(&character) {
        let (renderer, source_size) = cyrillic_font(style);
        if let Ok(metrics) =
            renderer.get_rendered_dimensions(character, Point::zero(), VerticalPosition::Top)
        {
            return (metrics.advance.x * style.normalized_size() / source_size).max(1)
                + i32::from(style.italic);
        }
    }
    if character == ' ' || character == '\u{00a0}' {
        return (style.normalized_size() * 4 / 9).max(1);
    }
    let font = system_font(style);
    let mut encoded = [0u8; 4];
    let key = character.encode_utf8(&mut encoded);
    let mut entry = font.charmap.get(key);
    if entry.key != key {
        entry = font.charmap.get("�");
    }
    let bounds = entry.glyph.images.get(0).bounding_box();
    let source = bounds.top_left.x.max(0) + bounds.size.width as i32 + 1;
    (source * style.normalized_size() / SOURCE_FONT_SIZE)
        .max(style.normalized_size() / 4)
        .max(1)
        + i32::from(style.italic)
}

fn rasterize_latin(character: char, style: Style) -> RasterGlyph {
    let advance = advance(character, style);
    let font = system_font(style);
    let mut encoded = [0u8; 4];
    let key = character.encode_utf8(&mut encoded);
    let mut entry = font.charmap.get(key);
    if entry.key != key {
        entry = font.charmap.get("�");
    }
    let image = entry.glyph.images.get(0);
    let bounds = image.bounding_box();
    let source_width = bounds.size.width as usize;
    let source_height = bounds.size.height as usize;
    if source_width == 0
        || source_height == 0
        || source_width > GLYPH_SIDE
        || source_height > GLYPH_SIDE
    {
        return RasterGlyph::empty(advance);
    }
    let mut source = [0u8; GLYPH_CAPACITY];
    for (index, gray) in image.colors().into_iter().enumerate() {
        if index >= source_width * source_height {
            break;
        }
        source[index] = gray.luma().saturating_mul(17);
    }
    let size = style.normalized_size();
    let width = div_ceil(source_width as i32 * size, SOURCE_FONT_SIZE).max(1) as usize;
    let height = div_ceil(source_height as i32 * size, SOURCE_FONT_SIZE).max(1) as usize;
    let bearing_x = bounds.top_left.x * size / SOURCE_FONT_SIZE;
    let bearing_y =
        SOURCE_BASELINE * size / SOURCE_FONT_SIZE + (-bounds.top_left.y) * size / SOURCE_FONT_SIZE;
    resample_final(
        &source,
        source_width,
        source_height,
        width,
        height,
        bearing_x,
        bearing_y,
        advance,
        style,
    )
}

fn metrics_latin(character: char, style: Style) -> GlyphMetrics {
    let advance = advance(character, style);
    let font = system_font(style);
    let mut encoded = [0u8; 4];
    let key = character.encode_utf8(&mut encoded);
    let mut entry = font.charmap.get(key);
    if entry.key != key {
        entry = font.charmap.get("�");
    }
    let bounds = entry.glyph.images.get(0).bounding_box();
    let source_width = bounds.size.width as i32;
    let source_height = bounds.size.height as i32;
    if source_width <= 0 || source_height <= 0 {
        return GlyphMetrics {
            width: 0,
            height: 0,
            bearing_x: 0,
            bearing_y: 0,
            advance: advance as i16,
        };
    }
    let size = style.normalized_size();
    let width = div_ceil(source_width * size, SOURCE_FONT_SIZE).max(1);
    let height = div_ceil(source_height * size, SOURCE_FONT_SIZE).max(1);
    GlyphMetrics {
        width: (width
            + if style.italic {
                style.line_height() / 4
            } else {
                0
            })
        .min(GLYPH_SIDE as i32) as u16,
        height: height.min(GLYPH_SIDE as i32) as u16,
        bearing_x: (bounds.top_left.x * size / SOURCE_FONT_SIZE) as i16,
        bearing_y: (SOURCE_BASELINE * size / SOURCE_FONT_SIZE
            + (-bounds.top_left.y) * size / SOURCE_FONT_SIZE) as i16,
        advance: advance as i16,
    }
}

fn cyrillic_font(style: Style) -> (FontRenderer, i32) {
    match (style.family, style.weight) {
        (Family::Console, Weight::Regular) => {
            (FontRenderer::new::<fonts::u8g2_font_9x15_t_cyrillic>(), 15)
        }
        (Family::Console, Weight::Bold) => {
            (FontRenderer::new::<fonts::u8g2_font_6x13B_t_cyrillic>(), 13)
        }
        (Family::Sans, Weight::Regular | Weight::Bold) => {
            (FontRenderer::new::<fonts::u8g2_font_inr24_t_cyrillic>(), 24)
        }
    }
}

fn rasterize_cyrillic(character: char, style: Style) -> RasterGlyph {
    let advance = advance(character, style);
    let (renderer, source_size) = cyrillic_font(style);
    let Ok(metrics) =
        renderer.get_rendered_dimensions(character, Point::zero(), VerticalPosition::Top)
    else {
        return RasterGlyph::empty(advance);
    };
    let Some(bounds) = metrics.bounding_box else {
        return RasterGlyph::empty(advance);
    };
    let source_width = bounds.size.width as usize;
    let source_height = bounds.size.height as usize;
    if source_width == 0
        || source_height == 0
        || source_width > GLYPH_SIDE
        || source_height > GLYPH_SIDE
    {
        return RasterGlyph::empty(advance);
    }
    let mut source = [0u8; GLYPH_CAPACITY];
    let mut target = BinaryTarget {
        bitmap: &mut source,
        width: source_width,
        height: source_height,
        embolden: style.family == Family::Sans && style.weight == Weight::Bold,
    };
    let position = Point::new(-bounds.top_left.x, -bounds.top_left.y);
    let _ = renderer.render(
        character,
        position,
        VerticalPosition::Top,
        FontColor::Transparent(BinaryColor::On),
        &mut target,
    );
    let size = style.normalized_size();
    let width = div_ceil(source_width as i32 * size, source_size).max(1) as usize;
    let height = div_ceil(source_height as i32 * size, source_size).max(1) as usize;
    let bearing_x = bounds.top_left.x * size / source_size;
    let bearing_y = bounds.top_left.y * size / source_size;
    resample_final(
        &source,
        source_width,
        source_height,
        width,
        height,
        bearing_x,
        bearing_y,
        advance,
        style,
    )
}

fn metrics_cyrillic(character: char, style: Style) -> GlyphMetrics {
    let advance = advance(character, style);
    let (renderer, source_size) = cyrillic_font(style);
    let Ok(metrics) =
        renderer.get_rendered_dimensions(character, Point::zero(), VerticalPosition::Top)
    else {
        return GlyphMetrics {
            width: 0,
            height: 0,
            bearing_x: 0,
            bearing_y: 0,
            advance: advance as i16,
        };
    };
    let Some(bounds) = metrics.bounding_box else {
        return GlyphMetrics {
            width: 0,
            height: 0,
            bearing_x: 0,
            bearing_y: 0,
            advance: advance as i16,
        };
    };
    let size = style.normalized_size();
    let width = div_ceil(bounds.size.width as i32 * size, source_size).max(1);
    let height = div_ceil(bounds.size.height as i32 * size, source_size).max(1);
    GlyphMetrics {
        width: (width
            + if style.italic {
                style.line_height() / 4
            } else {
                0
            })
        .min(GLYPH_SIDE as i32) as u16,
        height: height.min(GLYPH_SIDE as i32) as u16,
        bearing_x: (bounds.top_left.x * size / source_size) as i16,
        bearing_y: (bounds.top_left.y * size / source_size) as i16,
        advance: advance as i16,
    }
}

#[allow(clippy::too_many_arguments)]
fn resample_final(
    source: &[u8; GLYPH_CAPACITY],
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
    bearing_x: i32,
    bearing_y: i32,
    advance: i32,
    style: Style,
) -> RasterGlyph {
    let mut pixels = [0u8; GLYPH_CAPACITY];
    let max_shear = if style.italic {
        style.line_height() / 4
    } else {
        0
    } as usize;
    let output_width = (width + max_shear).min(GLYPH_SIDE);
    let output_height = height.min(GLYPH_SIDE);
    for y in 0..output_height {
        let shear = if style.italic {
            (style.line_height() - (bearing_y + y as i32)).clamp(0, style.line_height()) / 4
        } else {
            0
        } as usize;
        for x in 0..width.min(GLYPH_SIDE.saturating_sub(shear)) {
            pixels[y * output_width + x + shear] =
                resampled_coverage(source, source_width, source_height, width, height, x, y);
        }
    }
    RasterGlyph {
        pixels,
        width: output_width as u16,
        height: output_height as u16,
        bearing_x: bearing_x as i16,
        bearing_y: bearing_y as i16,
        advance: advance as i16,
    }
}

struct BinaryTarget<'a> {
    bitmap: &'a mut [u8; GLYPH_CAPACITY],
    width: usize,
    height: usize,
    embolden: bool,
}

impl OriginDimensions for BinaryTarget<'_> {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

impl DrawTarget for BinaryTarget<'_> {
    type Color = BinaryColor;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, value) in pixels {
            if value == BinaryColor::Off || point.x < 0 || point.y < 0 {
                continue;
            }
            let x = point.x as usize;
            let y = point.y as usize;
            if x < self.width && y < self.height {
                self.bitmap[y * self.width + x] = u8::MAX;
                if self.embolden && x + 1 < self.width {
                    self.bitmap[y * self.width + x + 1] = u8::MAX;
                }
            }
        }
        Ok(())
    }
}

fn div_ceil(value: i32, divisor: i32) -> i32 {
    value.saturating_add(divisor - 1) / divisor
}

fn resampled_coverage(
    bitmap: &[u8; GLYPH_CAPACITY],
    source_width: usize,
    source_height: usize,
    target_width: usize,
    target_height: usize,
    target_x: usize,
    target_y: usize,
) -> u8 {
    if target_width < source_width || target_height < source_height {
        let mut sum = 0u32;
        for sample_y in 0..4 {
            for sample_x in 0..4 {
                let x = ((target_x * 4 + sample_x) * source_width)
                    / target_width.saturating_mul(4).max(1);
                let y = ((target_y * 4 + sample_y) * source_height)
                    / target_height.saturating_mul(4).max(1);
                sum += u32::from(
                    bitmap[y.min(source_height - 1) * source_width + x.min(source_width - 1)],
                );
            }
        }
        return ((sum + 8) / 16) as u8;
    }
    let x_fixed = (((target_x * 2 + 1) * source_width * 256)
        / target_width.saturating_mul(2).max(1))
    .saturating_sub(128)
    .min((source_width - 1) * 256);
    let y_fixed = (((target_y * 2 + 1) * source_height * 256)
        / target_height.saturating_mul(2).max(1))
    .saturating_sub(128)
    .min((source_height - 1) * 256);
    let x0 = x_fixed / 256;
    let y0 = y_fixed / 256;
    let x1 = (x0 + 1).min(source_width - 1);
    let y1 = (y0 + 1).min(source_height - 1);
    let fx = (x_fixed % 256) as u32;
    let fy = (y_fixed % 256) as u32;
    let top = u32::from(bitmap[y0 * source_width + x0]) * (256 - fx)
        + u32::from(bitmap[y0 * source_width + x1]) * fx;
    let bottom = u32::from(bitmap[y1 * source_width + x0]) * (256 - fx)
        + u32::from(bitmap[y1 * source_width + x1]) * fx;
    (((top * (256 - fy) + bottom * fy) + 32_768) / 65_536) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const UI: Style = Style {
        family: Family::Sans,
        weight: Weight::Regular,
        italic: false,
        size: 16,
    };

    #[test]
    fn latin_and_cyrillic_share_bounded_contract() {
        for character in ['A', 'Я', 'ё'] {
            let glyph = rasterize(character, UI);
            let metrics = metrics(character, UI);
            assert!(glyph.width > 0 && glyph.width as usize <= GLYPH_SIDE);
            assert!(glyph.height > 0 && glyph.height as usize <= GLYPH_SIDE);
            assert!(glyph.advance > 0);
            assert_eq!(glyph.width, metrics.width);
            assert_eq!(glyph.height, metrics.height);
            assert_eq!(glyph.bearing_x, metrics.bearing_x);
            assert_eq!(glyph.bearing_y, metrics.bearing_y);
            assert_eq!(glyph.advance, metrics.advance);
            assert!(glyph.pixels[..glyph.width as usize * glyph.height as usize]
                .iter()
                .any(|coverage| *coverage != 0));
        }
    }

    #[test]
    fn style_changes_are_part_of_raster_identity() {
        let regular = rasterize('R', UI);
        let bold = rasterize(
            'R',
            Style {
                weight: Weight::Bold,
                ..UI
            },
        );
        assert_ne!(
            regular.pixels[..regular.width as usize * regular.height as usize],
            bold.pixels[..bold.width as usize * bold.height as usize]
        );
    }
}

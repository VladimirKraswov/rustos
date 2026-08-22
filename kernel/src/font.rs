//! Системные шрифты и единый API типографики RustOS.
//!
//! Два семейства встраиваются в образ ядра, поэтому ранняя диагностика и GUI
//! не зависят от исправности VFS:
//! * [`FontFamily::Console`] — M+ Code + X11 Cyrillic для terminal/editor/log;
//! * [`FontFamily::Sans`] — M+ 1 + Inconsolata Cyrillic, аналог Arial/Helvetica.
//!
//! Basic Latin хранится как 4-bit coverage, кириллица U+0400..U+052F
//! (включая Ё/ё) преобразуется из полного bitmap-набора в 8-bit coverage.
//! Regular и Bold — настоящие начертания шрифта, Italic выполняется лёгким
//! наклоном rasterizer'а. Размер задаётся высотой строки в пикселях, поэтому
//! один и тот же API пригоден для DPI-настроек и приложений user space.

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

use crate::graphics::{Color, Framebuffer, Rect};

/// Outline растеризуется host-макросом в 24 px/4-bit coverage. Это даёт
/// достаточно subpixel-информации для качественного уменьшения до обычных
/// UI-размеров 13–20 px, не заставляя ядро разбирать TTF.
const SOURCE_FONT_SIZE: i32 = 24;
/// Компактный terminal baseline: стандартный M+ line gap слишком велик для
/// консоли, поэтому оставляем 19 px сверху и 5 px под baseline.
const SOURCE_BASELINE: i32 = 25;
const SOURCE_CONSOLE_ADVANCE: i32 = 10;
const GLYPH_BITMAP_SIDE: usize = 64;
const GLYPH_BITMAP_CAPACITY: usize = GLYPH_BITMAP_SIDE * GLYPH_BITMAP_SIDE;

/// Два системных семейства. Имена намеренно стабильны: они войдут в GUI ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontFamily {
    Console,
    Sans,
}

/// Реальные regular/bold face. Курсив задаётся отдельно в [`FontStyle`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontWeight {
    Regular,
    Bold,
}

/// Полное описание текста без ссылок и allocation — структуру можно прямо
/// передавать в GUI-командах между процессами.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontStyle {
    pub family: FontFamily,
    pub weight: FontWeight,
    pub italic: bool,
    /// Типографский em-size, 10..=48 px. Высота строки равна 4/3 em.
    pub size: u16,
}

impl FontStyle {
    pub const fn console(size: u16) -> Self {
        Self {
            family: FontFamily::Console,
            weight: FontWeight::Regular,
            italic: false,
            size,
        }
    }

    pub const fn sans(size: u16) -> Self {
        Self {
            family: FontFamily::Sans,
            weight: FontWeight::Regular,
            italic: false,
            size,
        }
    }

    pub const fn bold(mut self) -> Self {
        self.weight = FontWeight::Bold;
        self
    }

    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// Применяет общесистемный UI-scale. Terminal передаёт собственный style
    /// без этого преобразования, поэтому настройка desktop не ломает число
    /// колонок консоли и остаётся отдельной от настройки terminal font.
    pub const fn scaled(mut self, scale_milli: u16) -> Self {
        let scaled = self.size as u32 * scale_milli as u32 / 1_000;
        self.size = if scaled < 10 {
            10
        } else if scaled > 48 {
            48
        } else {
            scaled as u16
        };
        self
    }

    pub const fn normalized_size(self) -> i32 {
        if self.size < 10 {
            10
        } else if self.size > 48 {
            48
        } else {
            self.size as i32
        }
    }

    pub const fn line_height(self) -> i32 {
        // 4/3 em: достаточно для Ё/й и нижних выносных элементов, но без
        // типографского line gap, который неуместен в terminal/list widgets.
        self.normalized_size() * 4 / 3
    }

    /// Ширина ячейки моноширинного семейства. Для Sans возвращает разумный
    /// средний advance; точная ширина строки доступна через [`measure_text`].
    pub const fn cell_width(self) -> i32 {
        let width = SOURCE_CONSOLE_ADVANCE * self.normalized_size() / SOURCE_FONT_SIZE;
        if width < 1 {
            1
        } else {
            width
        }
    }
}

pub const TERMINAL_DEFAULT: FontStyle = FontStyle::console(18);
pub const UI_SMALL: FontStyle = FontStyle::sans(13);
pub const UI_TITLE: FontStyle = FontStyle::sans(15).bold();

type SystemFont = BitmapFont<'static, Gray4, 1>;

/// Макрос M+ выполняет rasterization во время host-сборки. В kernel binary
/// попадает только компактная bitmap/charmap-таблица, а не TTF-парсер.
fn system_font(style: FontStyle) -> SystemFont {
    match (style.family, style.weight) {
        (FontFamily::Console, FontWeight::Regular) => mplus!(
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
        (FontFamily::Console, FontWeight::Bold) => mplus!(
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
        (FontFamily::Sans, FontWeight::Regular) => mplus!(
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
        (FontFamily::Sans, FontWeight::Bold) => mplus!(
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

/// Результат измерения UTF-8 текста в физических пикселях.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextMetrics {
    pub width: u32,
    pub height: u32,
}

pub fn measure_text(text: &str, style: FontStyle) -> TextMetrics {
    let mut width = 0i32;
    for character in text.chars() {
        width = width.saturating_add(character_advance(character, style));
    }
    TextMetrics {
        width: width.max(0) as u32,
        height: style.line_height() as u32,
    }
}

/// Рисует UTF-8 строку и возвращает X сразу после последнего символа.
pub fn draw_text(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    text: &str,
    color: Color,
    style: FontStyle,
) -> i32 {
    let clip = Rect::new(0, 0, fb.width(), fb.height());
    draw_text_clipped(fb, x, y, text, color, style, clip)
}

/// Рисует UTF-8 строку строго внутри clip. Это обязательный путь для retained
/// UI: ушедшая за ScrollView строка не должна протекать в toolbar или footer.
pub fn draw_text_clipped(
    fb: &mut Framebuffer,
    mut x: i32,
    y: i32,
    text: &str,
    color: Color,
    style: FontStyle,
    clip: Rect,
) -> i32 {
    for character in text.chars() {
        x = x.saturating_add(draw_char_clipped(fb, x, y, character, color, style, clip));
    }
    x
}

/// Рисует один Unicode-глиф и возвращает его advance.
pub fn draw_char(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    character: char,
    color: Color,
    style: FontStyle,
) -> i32 {
    let clip = Rect::new(0, 0, fb.width(), fb.height());
    draw_char_clipped(fb, x, y, character, color, style, clip)
}

fn draw_char_clipped(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    character: char,
    color: Color,
    style: FontStyle,
    clip: Rect,
) -> i32 {
    if is_cyrillic(character) {
        return draw_cyrillic(fb, x, y, character, color, style, clip);
    }
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
    let target_size = style.normalized_size();
    let baseline = y + SOURCE_BASELINE * target_size / SOURCE_FONT_SIZE;
    if source_width == 0
        || source_height == 0
        || source_width > GLYPH_BITMAP_SIDE
        || source_height > GLYPH_BITMAP_SIDE
    {
        return character_advance(character, style);
    }

    // Один bounded coverage tile на стеке. Раньше каждый source pixel
    // превращался в целый target rectangle; при 24→13 px несколько таких
    // rectangle накладывались друг на друга и давали грубую «лесенку».
    let mut bitmap = [0u8; GLYPH_BITMAP_CAPACITY];
    for (index, gray) in image.colors().into_iter().enumerate() {
        if index >= source_width.saturating_mul(source_height) {
            break;
        }
        bitmap[index] = gray.luma().saturating_mul(17);
    }

    let target_width = div_ceil(source_width as i32 * target_size, SOURCE_FONT_SIZE).max(1);
    let target_height = div_ceil(source_height as i32 * target_size, SOURCE_FONT_SIZE).max(1);
    let origin_x = x + bounds.top_left.x * target_size / SOURCE_FONT_SIZE;
    // M+ хранит vertical bearing вверх от baseline.
    let origin_y = baseline + (-bounds.top_left.y) * target_size / SOURCE_FONT_SIZE;
    for target_y in 0..target_height {
        let screen_y = origin_y + target_y;
        let shear = if style.italic {
            (style.line_height() - (screen_y - y)).clamp(0, style.line_height()) / 4
        } else {
            0
        };
        let mut target_x = 0;
        while target_x < target_width {
            let coverage = resampled_coverage(
                &bitmap,
                source_width,
                source_height,
                target_width as usize,
                target_height as usize,
                target_x as usize,
                target_y as usize,
            );
            let mut run_end = target_x + 1;
            while run_end < target_width
                && resampled_coverage(
                    &bitmap,
                    source_width,
                    source_height,
                    target_width as usize,
                    target_height as usize,
                    run_end as usize,
                    target_y as usize,
                ) == coverage
            {
                run_end += 1;
            }
            let first = (origin_x + target_x + shear).max(clip.x);
            let last = (origin_x + run_end + shear).min(clip.right());
            if coverage != 0 && clip.contains(first, screen_y) && first < last {
                fb.blend_span(first, screen_y, (last - first) as u32, color, coverage);
            }
            target_x = run_end;
        }
    }
    character_advance(character, style)
}

fn character_advance(character: char, style: FontStyle) -> i32 {
    if style.family == FontFamily::Console {
        return style.cell_width();
    }
    if is_cyrillic(character) {
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
    let scaled = source * style.normalized_size() / SOURCE_FONT_SIZE;
    scaled.max(style.normalized_size() / 4).max(1) + i32::from(style.italic)
}

fn is_cyrillic(character: char) -> bool {
    ('\u{0400}'..='\u{052f}').contains(&character)
}

fn cyrillic_font(style: FontStyle) -> (FontRenderer, i32) {
    match (style.family, style.weight) {
        (FontFamily::Console, FontWeight::Regular) => {
            (FontRenderer::new::<fonts::u8g2_font_9x15_t_cyrillic>(), 15)
        }
        (FontFamily::Console, FontWeight::Bold) => {
            (FontRenderer::new::<fonts::u8g2_font_6x13B_t_cyrillic>(), 13)
        }
        (FontFamily::Sans, FontWeight::Regular | FontWeight::Bold) => {
            (FontRenderer::new::<fonts::u8g2_font_inr24_t_cyrillic>(), 24)
        }
    }
}

/// U8g2 хранит кириллицу компактно как 1-bit bitmap. Сначала разворачиваем
/// один глиф в bounded coverage tile, затем применяем тот же box/bilinear
/// filter, что и к Latin. Поэтому русский набор остаётся полным, а diagonal
/// strokes получают полутоновые края вместо nearest-neighbour ступенек.
fn draw_cyrillic(
    framebuffer: &mut Framebuffer,
    x: i32,
    y: i32,
    character: char,
    color: Color,
    style: FontStyle,
    clip: Rect,
) -> i32 {
    let (renderer, source_size) = cyrillic_font(style);
    let Ok(metrics) =
        renderer.get_rendered_dimensions(character, Point::zero(), VerticalPosition::Top)
    else {
        return style.cell_width();
    };
    let Some(bounds) = metrics.bounding_box else {
        return character_advance(character, style);
    };
    let source_width = bounds.size.width as usize;
    let source_height = bounds.size.height as usize;
    if source_width == 0
        || source_height == 0
        || source_width > GLYPH_BITMAP_SIDE
        || source_height > GLYPH_BITMAP_SIDE
    {
        return character_advance(character, style);
    }

    let mut bitmap = [0u8; GLYPH_BITMAP_CAPACITY];
    let mut target = BinaryGlyphTarget {
        bitmap: &mut bitmap,
        width: source_width,
        height: source_height,
        embolden: style.family == FontFamily::Sans && style.weight == FontWeight::Bold,
    };
    let position = Point::new(-bounds.top_left.x, -bounds.top_left.y);
    let _ = renderer.render(
        character,
        position,
        VerticalPosition::Top,
        FontColor::Transparent(BinaryColor::On),
        &mut target,
    );

    let target_size = style.normalized_size();
    let target_width = div_ceil(source_width as i32 * target_size, source_size).max(1) as usize;
    let target_height = div_ceil(source_height as i32 * target_size, source_size).max(1) as usize;
    let origin_x = x + bounds.top_left.x * target_size / source_size;
    let origin_y = y + bounds.top_left.y * target_size / source_size;
    for target_y in 0..target_height {
        let screen_y = origin_y + target_y as i32;
        let shear = if style.italic {
            (style.line_height() - (screen_y - y)).clamp(0, style.line_height()) / 4
        } else {
            0
        };
        let mut target_x = 0;
        while target_x < target_width {
            let coverage = resampled_coverage(
                &bitmap,
                source_width,
                source_height,
                target_width,
                target_height,
                target_x,
                target_y,
            );
            let mut run_end = target_x + 1;
            while run_end < target_width
                && resampled_coverage(
                    &bitmap,
                    source_width,
                    source_height,
                    target_width,
                    target_height,
                    run_end,
                    target_y,
                ) == coverage
            {
                run_end += 1;
            }
            let first = (origin_x + target_x as i32 + shear).max(clip.x);
            let last = (origin_x + run_end as i32 + shear).min(clip.right());
            if coverage != 0 && clip.contains(first, screen_y) && first < last {
                framebuffer.blend_span(first, screen_y, (last - first) as u32, color, coverage);
            }
            target_x = run_end;
        }
    }
    if style.family == FontFamily::Console {
        style.cell_width()
    } else {
        (metrics.advance.x * target_size / source_size).max(1) + i32::from(style.italic)
    }
}

struct BinaryGlyphTarget<'a> {
    bitmap: &'a mut [u8; GLYPH_BITMAP_CAPACITY],
    width: usize,
    height: usize,
    embolden: bool,
}

impl OriginDimensions for BinaryGlyphTarget<'_> {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

impl DrawTarget for BinaryGlyphTarget<'_> {
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
            if x >= self.width || y >= self.height {
                continue;
            }
            self.bitmap[y * self.width + x] = u8::MAX;
            if self.embolden && x + 1 < self.width {
                self.bitmap[y * self.width + x + 1] = u8::MAX;
            }
        }
        Ok(())
    }
}

fn div_ceil(value: i32, divisor: i32) -> i32 {
    value.saturating_add(divisor - 1) / divisor
}

/// Box filter при уменьшении и bilinear filter при увеличении. Оба пути
/// используют только fixed-point integer math и один раз смешивают target
/// pixel с framebuffer — без накопления тёмных overlapping rectangles.
fn resampled_coverage(
    bitmap: &[u8; GLYPH_BITMAP_CAPACITY],
    source_width: usize,
    source_height: usize,
    target_width: usize,
    target_height: usize,
    target_x: usize,
    target_y: usize,
) -> u8 {
    if target_width < source_width || target_height < source_height {
        let mut sum = 0u32;
        for sample_y in 0..4usize {
            for sample_x in 0..4usize {
                let x = ((target_x * 4 + sample_x) * source_width)
                    / target_width.saturating_mul(4).max(1);
                let y = ((target_y * 4 + sample_y) * source_height)
                    / target_height.saturating_mul(4).max(1);
                sum = sum.saturating_add(u32::from(
                    bitmap[y.min(source_height - 1) * source_width + x.min(source_width - 1)],
                ));
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

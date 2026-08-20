//! Системные шрифты и единый API типографики RustOS.
//!
//! Два семейства встраиваются в образ ядра, поэтому ранняя диагностика и GUI
//! не зависят от исправности VFS:
//! * [`FontFamily::Console`] — M+ Code + X11 Cyrillic для terminal/editor/log;
//! * [`FontFamily::Sans`] — M+ 1 + Inconsolata Cyrillic, системный аналог Arial.
//!
//! В bitmap включены Basic Latin и кириллица U+0400..U+052F (включая Ё/ё).
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

use crate::graphics::{Color, Framebuffer};

/// Базовый bitmap растеризован с em-size 18 px. Остальные размеры
/// масштабируются integer-rasterizer'ом без FPU и heap, что важно для
/// раннего ядра и ARM.
const SOURCE_FONT_SIZE: i32 = 18;
/// Компактный terminal baseline: стандартный M+ line gap слишком велик для
/// консоли, поэтому оставляем 19 px сверху и 5 px под baseline.
const SOURCE_BASELINE: i32 = 19;
const SOURCE_CONSOLE_ADVANCE: i32 = 10;

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
pub const UI_NORMAL: FontStyle = FontStyle::sans(15);
pub const UI_TITLE: FontStyle = FontStyle::sans(15).bold();

type SystemFont = BitmapFont<'static, Gray4, 1>;

/// Макрос M+ выполняет rasterization во время host-сборки. В kernel binary
/// попадает только компактная bitmap/charmap-таблица, а не TTF-парсер.
fn system_font(style: FontStyle) -> SystemFont {
    match (style.family, style.weight) {
        (FontFamily::Console, FontWeight::Regular) => mplus!(
            code(100),
            500,
            18,
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
            18,
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
            18,
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
            18,
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
    mut x: i32,
    y: i32,
    text: &str,
    color: Color,
    style: FontStyle,
) -> i32 {
    for character in text.chars() {
        x = x.saturating_add(draw_char(fb, x, y, character, color, style));
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
    if is_cyrillic(character) {
        return draw_cyrillic(fb, x, y, character, color, style);
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
    let source_width = bounds.size.width as i32;
    let target_size = style.normalized_size();
    let baseline = y + SOURCE_BASELINE * target_size / SOURCE_FONT_SIZE;

    for (index, gray) in image.colors().into_iter().enumerate() {
        if source_width == 0 {
            break;
        }
        let source_x = index as i32 % source_width;
        let source_y = index as i32 / source_width;
        let coverage = gray.luma().saturating_mul(17);
        if coverage == 0 {
            continue;
        }

        let glyph_x = bounds.top_left.x + source_x;
        // M+ хранит вертикальный offset вверх от baseline.
        let glyph_y = -bounds.top_left.y + source_y;
        let target_y = baseline + glyph_y * target_size / SOURCE_FONT_SIZE;
        let shear = if style.italic {
            // Верх глифа сдвигается вправо максимум на четверть строки.
            (style.line_height() - (target_y - y)).clamp(0, style.line_height()) / 4
        } else {
            0
        };
        let target_x = x + glyph_x * target_size / SOURCE_FONT_SIZE + shear;

        let next_x =
            x + ((glyph_x + 1) * target_size + SOURCE_FONT_SIZE - 1) / SOURCE_FONT_SIZE + shear;
        let next_y =
            baseline + ((glyph_y + 1) * target_size + SOURCE_FONT_SIZE - 1) / SOURCE_FONT_SIZE;
        for py in target_y..next_y.max(target_y + 1) {
            for px in target_x..next_x.max(target_x + 1) {
                fb.blend_pixel(px, py, color, coverage);
            }
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
    let entry = font.charmap.get(key);
    let bounds = entry.glyph.images.get(0).bounding_box();
    let source = bounds.top_left.x.max(0) + bounds.size.width as i32 + 1;
    let scaled = source * style.normalized_size() / SOURCE_FONT_SIZE;
    scaled.max(style.normalized_size() / 4).max(1) + i32::from(style.italic)
}

fn is_cyrillic(character: char) -> bool {
    ('\u{0400}'..='\u{052f}').contains(&character)
}

/// M+ Code не содержит кириллицу в исходном face. Для русского текста
/// используем проверенные U8g2 Cyrillic fonts: X11 в console и Inconsolata
/// Cyrillic в UI. Выбор скрыт внутри общего FontStyle API.
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

fn draw_cyrillic(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    character: char,
    color: Color,
    style: FontStyle,
) -> i32 {
    let (renderer, source_size) = cyrillic_font(style);
    let advance = renderer
        .get_rendered_dimensions(character, Point::zero(), VerticalPosition::Top)
        .ok()
        .map(|metrics| metrics.advance.x * style.normalized_size() / source_size)
        .unwrap_or(style.cell_width())
        .max(1);
    let mut target = ScaledGlyphTarget {
        framebuffer: fb,
        origin_x: x,
        origin_y: y,
        source_size,
        target_size: style.normalized_size(),
        italic: style.italic,
        embolden: style.family == FontFamily::Sans && style.weight == FontWeight::Bold,
        color,
    };
    let _ = renderer.render(
        character,
        Point::zero(),
        VerticalPosition::Top,
        FontColor::Transparent(BinaryColor::On),
        &mut target,
    );
    if style.family == FontFamily::Console {
        style.cell_width()
    } else {
        advance + i32::from(style.italic)
    }
}

/// Адаптер U8g2 → framebuffer одновременно масштабирует bitmap и выполняет
/// синтетический italic. Он создаётся на стеке на один глиф и не аллоцирует.
struct ScaledGlyphTarget<'a> {
    framebuffer: &'a mut Framebuffer,
    origin_x: i32,
    origin_y: i32,
    source_size: i32,
    target_size: i32,
    italic: bool,
    embolden: bool,
    color: Color,
}

impl OriginDimensions for ScaledGlyphTarget<'_> {
    fn size(&self) -> Size {
        Size::new(96, 96)
    }
}

impl DrawTarget for ScaledGlyphTarget<'_> {
    type Color = BinaryColor;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, pixel) in pixels {
            if pixel == BinaryColor::Off {
                continue;
            }
            let shear = if self.italic {
                (self.source_size - point.y).clamp(0, self.source_size) / 4
            } else {
                0
            };
            let x0 = self.origin_x + (point.x + shear) * self.target_size / self.source_size;
            let y0 = self.origin_y + point.y * self.target_size / self.source_size;
            let x1 = self.origin_x
                + ((point.x + shear + 1) * self.target_size + self.source_size - 1)
                    / self.source_size;
            let y1 = self.origin_y
                + ((point.y + 1) * self.target_size + self.source_size - 1) / self.source_size;
            for py in y0..y1.max(y0 + 1) {
                for px in x0..x1.max(x0 + 1) {
                    self.framebuffer.put_pixel(px, py, self.color);
                    if self.embolden {
                        self.framebuffer.put_pixel(px + 1, py, self.color);
                    }
                }
            }
        }
        Ok(())
    }
}

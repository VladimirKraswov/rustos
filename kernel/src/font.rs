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

use crate::graphics::{Color, Framebuffer, Rect};

/// Outline растеризуется host-макросом в 24 px/4-bit coverage. Это даёт
/// достаточно subpixel-информации для качественного уменьшения до обычных
/// UI-размеров 13–20 px, не заставляя ядро разбирать TTF.
const SOURCE_FONT_SIZE: i32 = 24;
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
pub const UI_TITLE: FontStyle = FontStyle::sans(15).bold();

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
    if fb.gpu_recording() {
        let metrics = rustos_system_fonts::metrics(character, raster_style(style));
        let advance = i32::from(metrics.advance);
        if metrics.width == 0 || metrics.height == 0 {
            return advance;
        }
        let glyph_rect = Rect::new(
            x + i32::from(metrics.bearing_x),
            y + i32::from(metrics.bearing_y),
            u32::from(metrics.width),
            u32::from(metrics.height),
        );
        let screen = Rect::new(0, 0, fb.width(), fb.height());
        let visible = glyph_rect.intersection(clip).intersection(screen);
        if !visible.is_empty() {
            crate::gui::gpu_scene::glyph(
                visible,
                color,
                character,
                gpu_style(style),
                visible.x.saturating_sub(glyph_rect.x) as u16,
                visible.y.saturating_sub(glyph_rect.y) as u16,
            );
        }
        return advance;
    }
    let raster = rustos_system_fonts::rasterize(character, raster_style(style));
    let advance = i32::from(raster.advance);
    if raster.width == 0 || raster.height == 0 {
        return advance;
    }
    let glyph_rect = Rect::new(
        x + i32::from(raster.bearing_x),
        y + i32::from(raster.bearing_y),
        u32::from(raster.width),
        u32::from(raster.height),
    );
    let screen = Rect::new(0, 0, fb.width(), fb.height());
    let visible = glyph_rect.intersection(clip).intersection(screen);
    if visible.is_empty() {
        return advance;
    }
    let crop_x = visible.x.saturating_sub(glyph_rect.x) as usize;
    let crop_y = visible.y.saturating_sub(glyph_rect.y) as usize;
    let source_width = raster.width as usize;
    for row in 0..visible.height as usize {
        let source_y = crop_y + row;
        let mut column = 0usize;
        while column < visible.width as usize {
            let coverage = raster.pixels[source_y * source_width + crop_x + column];
            let mut run_end = column + 1;
            while run_end < visible.width as usize
                && raster.pixels[source_y * source_width + crop_x + run_end] == coverage
            {
                run_end += 1;
            }
            if coverage != 0 {
                fb.blend_span(
                    visible.x + column as i32,
                    visible.y + row as i32,
                    (run_end - column) as u32,
                    color,
                    coverage,
                );
            }
            column = run_end;
        }
    }
    advance
}

fn character_advance(character: char, style: FontStyle) -> i32 {
    rustos_system_fonts::advance(character, raster_style(style))
}

fn raster_style(style: FontStyle) -> rustos_system_fonts::Style {
    rustos_system_fonts::Style {
        family: match style.family {
            FontFamily::Console => rustos_system_fonts::Family::Console,
            FontFamily::Sans => rustos_system_fonts::Family::Sans,
        },
        weight: match style.weight {
            FontWeight::Regular => rustos_system_fonts::Weight::Regular,
            FontWeight::Bold => rustos_system_fonts::Weight::Bold,
        },
        italic: style.italic,
        size: style.size,
    }
}

fn gpu_style(style: FontStyle) -> u32 {
    rustos_abi::gpu::ui_glyph_style::pack(
        style.family == FontFamily::Sans,
        style.weight == FontWeight::Bold,
        style.italic,
        style.size.clamp(10, 48),
    )
}

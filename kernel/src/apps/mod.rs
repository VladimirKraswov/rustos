//! Приложения первого среза. Пока в ядре нет ring-3 процессов, они
//! компилируются в него; после IPC-milestone станут отдельными ELF-образами
//! (docs/ARCHITECTURE.md, «Путь к микроядру»).
pub mod desktop_settings;
pub mod file_explorer;
pub mod gpu_demo;
pub mod shell_ui;
pub mod terminal;
pub mod ui_showcase;

use crate::{
    font,
    graphics::{Color, Framebuffer, Rect},
};
use rustos_system_ui::{FontSpec, TextAlign};

/// Единый text adapter System UI к системному font rasterizer. Выравнивание
/// вычисляется после разрешения ResourceId, поэтому component runtime не знает
/// ни UTF-8 bytes, ни метрики конкретного шрифта.
pub(crate) fn draw_system_ui_text(
    framebuffer: &mut Framebuffer,
    rect: Rect,
    text: &str,
    color: Color,
    spec: FontSpec,
    clip: Rect,
) {
    let mut style = if spec.monospace {
        font::FontStyle::console(spec.size.clamp(10, 48))
    } else {
        font::FontStyle::sans(spec.size.clamp(10, 48))
    };
    if spec.bold {
        style = style.bold();
    }
    if spec.italic {
        style = style.italic();
    }
    let metrics = font::measure_text(text, style);
    let x = match spec.align {
        TextAlign::Start => rect.x,
        TextAlign::Center => rect
            .x
            .saturating_add((rect.width.saturating_sub(metrics.width) / 2) as i32),
        TextAlign::End => rect
            .right()
            .saturating_sub(metrics.width.min(rect.width) as i32),
    };
    let y = if spec.vertical_center {
        rect.y
            .saturating_add((rect.height.saturating_sub(metrics.height) / 2) as i32)
    } else {
        rect.y
    };
    font::draw_text_clipped(framebuffer, x, y, text, color, style, clip);
}

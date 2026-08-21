//! Оформление окон bootstrap-сеанса.
//!
//! Это не библиотека UI-компонентов. Общие controls, layout, события и
//! accessibility принадлежат `rustos-system-ui`; этот модуль лишь переводит
//! несколько элементов рамки окна в операции текущего CPU framebuffer.

use crate::{
    font,
    graphics::{Color, Framebuffer, Rect},
};

/// Палитра рамки и панели задач bootstrap-сеанса.
pub struct Theme;

impl Theme {
    pub const ACCENT: Color = Color::rgb(45, 124, 246);
    pub const ACCENT_SOFT: Color = Color::rgb(28, 57, 99);
    pub const PANEL: Color = Color::rgb(20, 28, 42);
    pub const PANEL_LIGHT: Color = Color::rgb(31, 42, 59);
    pub const BORDER: Color = Color::rgb(58, 73, 96);
    pub const TEXT: Color = Color::rgb(241, 246, 253);
    pub const TEXT_MUTED: Color = Color::rgb(155, 169, 190);
    pub const DANGER: Color = Color::rgb(239, 82, 91);
    pub const RADIUS: u8 = 12;
}

/// Элемент chrome получает framebuffer только на время построения кадра.
pub trait Widget {
    fn draw(&self, framebuffer: &mut Framebuffer);
}

/// Фон рамки окна или системной панели.
pub struct Panel {
    pub rect: Rect,
    pub color: Color,
    pub border: Option<Color>,
}

impl Widget for Panel {
    fn draw(&self, framebuffer: &mut Framebuffer) {
        framebuffer.fill_rounded_rect(self.rect, Theme::RADIUS, self.color);
        if let Some(color) = self.border {
            framebuffer.rounded_border(self.rect, Theme::RADIUS, 1, color);
        }
    }
}

/// Текст заголовка без собственного фона.
pub struct Label<'a> {
    pub rect: Rect,
    pub text: &'a str,
    pub color: Color,
    pub style: font::FontStyle,
}

impl Widget for Label<'_> {
    fn draw(&self, framebuffer: &mut Framebuffer) {
        font::draw_text(
            framebuffer,
            self.rect.x,
            self.rect.y,
            self.text,
            self.color,
            self.style,
        );
    }
}

/// Кнопка управления окном. Состояние ввода хранит session, а не виджет.
pub struct Button<'a> {
    pub rect: Rect,
    pub label: &'a str,
    pub hovered: bool,
    pub pressed: bool,
    pub danger: bool,
}

impl Widget for Button<'_> {
    fn draw(&self, framebuffer: &mut Framebuffer) {
        let base = if self.danger && self.hovered {
            Theme::DANGER
        } else if self.hovered {
            Theme::ACCENT_SOFT
        } else {
            Theme::PANEL_LIGHT
        };
        let color = if self.pressed {
            base.mix(Color::rgb(0, 0, 0), 70)
        } else {
            base
        };
        framebuffer.fill_rounded_rect(self.rect, 8, color);
        if self.hovered || self.danger {
            framebuffer.rounded_border(
                self.rect,
                8,
                1,
                if self.danger && self.hovered {
                    Theme::DANGER
                } else {
                    Theme::BORDER
                },
            );
        }

        let style = font::UI_TITLE;
        let metrics = font::measure_text(self.label, style);
        let x = self.rect.x + (self.rect.width as i32 - metrics.width as i32) / 2;
        let y = self.rect.y + (self.rect.height as i32 - metrics.height as i32) / 2;
        font::draw_text(framebuffer, x, y, self.label, Theme::TEXT, style);
    }
}

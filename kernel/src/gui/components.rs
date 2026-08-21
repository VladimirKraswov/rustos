//! Базовые UI-компоненты RustOS.
//!
//! Компоненты не владеют framebuffer и не содержат unsafe-кода. Они получают
//! `Framebuffer` только на время отрисовки; события проверяются через hit-test.

#![allow(dead_code)] // публичный SDK шире набора, используемого desktop v0.1

use crate::{
    font,
    graphics::{Color, Framebuffer, Rect},
};

/// Современная desktop-палитра v2: глубокий нейтральный chrome, спокойные
/// границы и насыщенный blue accent. Все токены остаются константами для
/// воспроизводимых screenshot-тестов.
pub struct Theme;

impl Theme {
    pub const DESKTOP_TOP: Color = Color::rgb(18, 37, 70);
    pub const DESKTOP_BOTTOM: Color = Color::rgb(25, 65, 91);
    pub const ACCENT: Color = Color::rgb(45, 124, 246);
    pub const ACCENT_HOVER: Color = Color::rgb(75, 148, 255);
    pub const ACCENT_SOFT: Color = Color::rgb(28, 57, 99);
    pub const PANEL: Color = Color::rgb(20, 28, 42);
    pub const PANEL_LIGHT: Color = Color::rgb(31, 42, 59);
    pub const SURFACE: Color = Color::rgb(12, 18, 28);
    pub const BORDER: Color = Color::rgb(58, 73, 96);
    pub const TEXT: Color = Color::rgb(241, 246, 253);
    pub const TEXT_MUTED: Color = Color::rgb(155, 169, 190);
    pub const DANGER: Color = Color::rgb(239, 82, 91);
    pub const RADIUS: u8 = 12;
}

/// Рендерящийся компонент: знает свои границы и умеет отрисовать себя.
/// State-машина ввода (hover/pressed/focus) принадлежит session, а не
/// widget'у — компоненты неизменяемы на время кадра.
pub trait Widget {
    /// Границы в координатах framebuffer'а (используются и для hit-test).
    fn bounds(&self) -> Rect;
    /// Отрисовка в back buffer; framebuffer передаётся только на время вызова.
    fn draw(&self, fb: &mut Framebuffer);

    /// По умолчанию — точка внутри `bounds`.
    fn hit_test(&self, x: i32, y: i32) -> bool {
        self.bounds().contains(x, y)
    }
}

/// Прямой прямоугольник заданного цвета, опционально с рамкой.
pub struct Panel {
    pub rect: Rect,
    pub color: Color,
    pub border: Option<Color>,
}

impl Widget for Panel {
    fn bounds(&self) -> Rect {
        self.rect
    }

    fn draw(&self, fb: &mut Framebuffer) {
        fb.fill_rounded_rect(self.rect, Theme::RADIUS, self.color);
        if let Some(color) = self.border {
            fb.rounded_border(self.rect, Theme::RADIUS, 1, color);
        }
    }
}

/// Текст без фона: рисуется из верхнего левого угла `rect` заданным системным
/// семейством, начертанием и размером.
pub struct Label<'a> {
    pub rect: Rect,
    pub text: &'a str,
    pub color: Color,
    pub style: font::FontStyle,
}

impl Widget for Label<'_> {
    fn bounds(&self) -> Rect {
        self.rect
    }

    fn draw(&self, fb: &mut Framebuffer) {
        font::draw_text(
            fb,
            self.rect.x,
            self.rect.y,
            self.text,
            self.color,
            self.style,
        );
    }
}

/// Кнопка: hover/pressed/danger задаёт владелец (session), сама кнопка
/// только описывает состояние кадра. Текст центрируется по bounding box.
pub struct Button<'a> {
    pub rect: Rect,
    pub label: &'a str,
    pub hovered: bool,
    pub pressed: bool,
    pub danger: bool,
}

impl Widget for Button<'_> {
    fn bounds(&self) -> Rect {
        self.rect
    }

    fn draw(&self, fb: &mut Framebuffer) {
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
        fb.fill_rounded_rect(self.rect, 8, color);
        if self.hovered || self.danger {
            fb.rounded_border(
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
        font::draw_text(fb, x, y, self.label, Theme::TEXT, style);
    }
}

/// Чекбокс: квадрат 18×18 + подпись справа (размер фиксирован в draw).
pub struct Checkbox<'a> {
    pub rect: Rect,
    pub label: &'a str,
    pub checked: bool,
}

impl Widget for Checkbox<'_> {
    fn bounds(&self) -> Rect {
        self.rect
    }

    fn draw(&self, fb: &mut Framebuffer) {
        let box_rect = Rect::new(self.rect.x, self.rect.y, 18, 18);
        fb.fill_rounded_rect(box_rect, 5, Theme::SURFACE);
        fb.rounded_border(box_rect, 5, 1, Theme::BORDER);
        if self.checked {
            fb.fill_rounded_rect(
                Rect::new(self.rect.x + 4, self.rect.y + 4, 10, 10),
                3,
                Theme::ACCENT,
            );
        }
        font::draw_text(
            fb,
            self.rect.x + 25,
            self.rect.y + 5,
            self.label,
            Theme::TEXT,
            font::UI_SMALL,
        );
    }
}

/// Радио-кнопка с круглым indicator и подписью.
pub struct RadioButton<'a> {
    pub rect: Rect,
    pub label: &'a str,
    pub selected: bool,
}

impl Widget for RadioButton<'_> {
    fn bounds(&self) -> Rect {
        self.rect
    }

    fn draw(&self, fb: &mut Framebuffer) {
        let indicator = Rect::new(self.rect.x, self.rect.y + 1, 18, 18);
        fb.fill_rounded_rect(indicator, 9, Theme::SURFACE);
        fb.rounded_border(indicator, 9, 1, Theme::BORDER);
        if self.selected {
            fb.fill_rounded_rect(
                Rect::new(self.rect.x + 5, self.rect.y + 6, 8, 8),
                4,
                Theme::ACCENT,
            );
        }
        font::draw_text(
            fb,
            self.rect.x + 20,
            self.rect.y + 4,
            self.label,
            Theme::TEXT,
            font::UI_SMALL,
        );
    }
}

/// Переключатель: трек 36×18 + «ползунок» + подпись справа.
pub struct Toggle<'a> {
    pub rect: Rect,
    pub label: &'a str,
    pub enabled: bool,
}

impl Widget for Toggle<'_> {
    fn bounds(&self) -> Rect {
        self.rect
    }

    fn draw(&self, fb: &mut Framebuffer) {
        let track = Rect::new(self.rect.x, self.rect.y, 36, 18);
        fb.fill_rounded_rect(
            track,
            9,
            if self.enabled {
                Theme::ACCENT
            } else {
                Theme::BORDER
            },
        );
        let knob_x = if self.enabled {
            self.rect.x + 20
        } else {
            self.rect.x + 2
        };
        fb.fill_rounded_rect(Rect::new(knob_x, self.rect.y + 2, 14, 14), 7, Theme::TEXT);
        font::draw_text(
            fb,
            self.rect.x + 44,
            self.rect.y + 5,
            self.label,
            Theme::TEXT,
            font::UI_SMALL,
        );
    }
}

/// Однострочное текстовое поле: рамка акцентируется при фокусе.
/// Реальное редактирование (курсор, ввод) появится вместе с user-space shell.
pub struct TextEdit<'a> {
    pub rect: Rect,
    pub text: &'a str,
    pub focused: bool,
}

impl Widget for TextEdit<'_> {
    fn bounds(&self) -> Rect {
        self.rect
    }

    fn draw(&self, fb: &mut Framebuffer) {
        fb.fill_rounded_rect(self.rect, 8, Theme::SURFACE);
        fb.rounded_border(
            self.rect,
            8,
            1,
            if self.focused {
                Theme::ACCENT
            } else {
                Theme::BORDER
            },
        );
        font::draw_text(
            fb,
            self.rect.x + 5,
            self.rect.y + 6,
            self.text,
            Theme::TEXT,
            font::UI_NORMAL,
        );
    }
}

/// Контейнерные компоненты SDK. Layout и scrolling будут расширены после
/// появления heap; уже сейчас они имеют стабильные границы и hit-test.
pub type ScrollView = Panel;
pub type ListView = Panel;
pub type Tabs = Panel;
pub type Image = Panel;
pub type IconButton<'a> = Button<'a>;

/// Логотип Start: четыре мягких цветных плитки.
pub fn start_icon(fb: &mut Framebuffer, x: i32, y: i32) {
    let colors = [
        Color::rgb(80, 196, 220),
        Color::rgb(105, 220, 192),
        Color::rgb(103, 140, 238),
        Color::rgb(170, 116, 235),
    ];
    for row in 0..2 {
        for column in 0..2 {
            fb.fill_rounded_rect(
                Rect::new(x + column * 12, y + row * 12, 10, 10),
                3,
                colors[(row * 2 + column) as usize],
            );
        }
    }
}

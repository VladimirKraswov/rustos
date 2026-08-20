//! Строго типизированные дизайн-токены и системные темы.

use rustos_video::Color;

use crate::{ComponentKind, NodeState};

/// Вариант системной темы.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeKind {
    /// Светлая.
    Light = 1,
    /// Тёмная.
    Dark = 2,
    /// Высококонтрастная.
    HighContrast = 3,
}

/// Палитра системных токенов. Компонент выбирает семантический токен, а не
/// жёстко прописывает цвет.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Palette {
    /// Фон окна.
    pub window: Color,
    /// Поверхность панели.
    pub surface: Color,
    /// Поднятая/интерактивная поверхность.
    pub raised: Color,
    /// Основной акцент.
    pub accent: Color,
    /// Акцент при hover.
    pub accent_hover: Color,
    /// Основной текст.
    pub text: Color,
    /// Вторичный текст.
    pub text_muted: Color,
    /// Граница.
    pub border: Color,
    /// Ошибка.
    pub danger: Color,
    /// Фокусное кольцо.
    pub focus: Color,
}

/// Системная тема и общие предпочтения accessibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    /// Версия токенов, независимая от ABI runtime.
    pub version: u16,
    /// Вариант.
    pub kind: ThemeKind,
    /// Цвета.
    pub palette: Palette,
    /// UI scale в тысячных.
    pub scale_milli: u16,
    /// Отключить/сократить движение.
    pub reduced_motion: bool,
    /// Базовый радиус (CPU backend v1 рисует прямые углы, значение сохранено).
    pub radius: u8,
}

impl Theme {
    /// Системная тёмная тема RustOS.
    pub const fn dark() -> Self {
        Self {
            version: 1,
            kind: ThemeKind::Dark,
            palette: Palette {
                window: Color::rgb(11, 17, 27),
                surface: Color::rgb(20, 29, 43),
                raised: Color::rgb(35, 47, 65),
                accent: Color::rgb(80, 196, 220),
                accent_hover: Color::rgb(114, 222, 238),
                text: Color::rgb(232, 239, 247),
                text_muted: Color::rgb(154, 172, 194),
                border: Color::rgb(75, 92, 116),
                danger: Color::rgb(224, 86, 94),
                focus: Color::rgb(145, 228, 240),
            },
            scale_milli: 1000,
            reduced_motion: false,
            radius: 5,
        }
    }

    /// Светлая тема с тем же поведением компонентов.
    pub const fn light() -> Self {
        Self {
            version: 1,
            kind: ThemeKind::Light,
            palette: Palette {
                window: Color::rgb(241, 245, 249),
                surface: Color::rgb(255, 255, 255),
                raised: Color::rgb(226, 234, 241),
                accent: Color::rgb(15, 126, 153),
                accent_hover: Color::rgb(8, 99, 123),
                text: Color::rgb(19, 31, 45),
                text_muted: Color::rgb(76, 94, 112),
                border: Color::rgb(145, 160, 176),
                danger: Color::rgb(183, 40, 55),
                focus: Color::rgb(0, 91, 138),
            },
            scale_milli: 1000,
            reduced_motion: false,
            radius: 5,
        }
    }

    /// Высококонтрастная тема.
    pub const fn high_contrast() -> Self {
        Self {
            version: 1,
            kind: ThemeKind::HighContrast,
            palette: Palette {
                window: Color::rgb(0, 0, 0),
                surface: Color::rgb(0, 0, 0),
                raised: Color::rgb(0, 0, 0),
                accent: Color::rgb(255, 255, 0),
                accent_hover: Color::rgb(255, 255, 255),
                text: Color::rgb(255, 255, 255),
                text_muted: Color::rgb(230, 230, 230),
                border: Color::rgb(255, 255, 255),
                danger: Color::rgb(255, 90, 90),
                focus: Color::rgb(255, 255, 0),
            },
            scale_milli: 1000,
            reduced_motion: true,
            radius: 0,
        }
    }

    /// Вычисляет визуальные свойства для kind/state/style class.
    pub fn resolve(self, kind: ComponentKind, state: NodeState, style: u16) -> ComputedStyle {
        let disabled = state.contains(NodeState::DISABLED);
        let hovered = state.contains(NodeState::HOVERED);
        let pressed = state.contains(NodeState::PRESSED);
        let invalid = state.contains(NodeState::INVALID);
        let interactive = kind.focusable();
        let mut background = match kind {
            ComponentKind::Root => Some(self.palette.window),
            ComponentKind::Panel | ComponentKind::ScrollView | ComponentKind::ListView => {
                Some(self.palette.surface)
            }
            ComponentKind::Button
            | ComponentKind::CheckBox
            | ComponentKind::RadioButton
            | ComponentKind::Switch
            | ComponentKind::TextField
            | ComponentKind::TextArea
            | ComponentKind::Select
            | ComponentKind::TabView
            | ComponentKind::Menu
            | ComponentKind::Dialog => Some(self.palette.raised),
            ComponentKind::ProgressBar | ComponentKind::Slider | ComponentKind::Divider => {
                Some(self.palette.border)
            }
            _ => None,
        };
        if interactive && hovered {
            background = Some(self.palette.accent_hover);
        }
        if pressed {
            background = background.map(|color| color.mix(Color::rgb(0, 0, 0), 70));
        }
        // Style class 1 — primary action; 2 — destructive action.
        if style == 1 {
            background = Some(self.palette.accent);
        } else if style == 2 {
            background = Some(self.palette.danger);
        }
        let foreground = if disabled {
            self.palette.text_muted.mix(self.palette.window, 100)
        } else {
            self.palette.text
        };
        ComputedStyle {
            background,
            foreground,
            border: if invalid {
                self.palette.danger
            } else {
                self.palette.border
            },
            focus: self.palette.focus,
            border_width: u8::from(interactive || background.is_some()),
            font_size: if kind == ComponentKind::Text { 16 } else { 14 },
            bold: style == 1,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Итоговые style-токены одного кадра.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComputedStyle {
    /// Фон; `None` означает прозрачный компонент.
    pub background: Option<Color>,
    /// Текст/иконка.
    pub foreground: Color,
    /// Граница.
    pub border: Color,
    /// Focus ring.
    pub focus: Color,
    /// Толщина границы.
    pub border_width: u8,
    /// Размер системного шрифта.
    pub font_size: u16,
    /// Жирное начертание.
    pub bold: bool,
}

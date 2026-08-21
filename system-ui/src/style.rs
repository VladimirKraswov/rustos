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
    /// Мягкая акцентная поверхность для selected/hover без заливки primary.
    pub accent_soft: Color,
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
    /// Успех.
    pub success: Color,
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
    /// Пользовательский accessibility scale в тысячных.
    ///
    /// Это увеличение самих компонентов, а не device scale монитора. Связь
    /// logical/physical surface хранится отдельно в `WindowMetrics`.
    pub scale_milli: u16,
    /// Отключить/сократить движение.
    pub reduced_motion: bool,
    /// Базовый радиус системных поверхностей и controls.
    pub radius: u8,
}

impl Theme {
    /// Системная тёмная тема RustOS.
    pub const fn dark() -> Self {
        Self {
            version: 2,
            kind: ThemeKind::Dark,
            palette: Palette {
                window: Color::rgb(12, 18, 28),
                surface: Color::rgb(20, 28, 42),
                raised: Color::rgb(31, 42, 59),
                accent_soft: Color::rgb(28, 57, 99),
                accent: Color::rgb(45, 124, 246),
                accent_hover: Color::rgb(75, 148, 255),
                text: Color::rgb(241, 246, 253),
                text_muted: Color::rgb(155, 169, 190),
                border: Color::rgb(58, 73, 96),
                danger: Color::rgb(239, 82, 91),
                success: Color::rgb(44, 190, 132),
                focus: Color::rgb(111, 174, 255),
            },
            scale_milli: 1000,
            reduced_motion: false,
            radius: 10,
        }
    }

    /// Светлая тема с тем же поведением компонентов.
    pub const fn light() -> Self {
        Self {
            version: 2,
            kind: ThemeKind::Light,
            palette: Palette {
                window: Color::rgb(244, 247, 252),
                surface: Color::rgb(255, 255, 255),
                raised: Color::rgb(249, 251, 254),
                accent_soft: Color::rgb(228, 238, 255),
                accent: Color::rgb(29, 112, 246),
                accent_hover: Color::rgb(18, 91, 216),
                text: Color::rgb(24, 35, 52),
                text_muted: Color::rgb(99, 114, 136),
                border: Color::rgb(215, 223, 235),
                danger: Color::rgb(231, 63, 72),
                success: Color::rgb(28, 164, 108),
                focus: Color::rgb(86, 152, 255),
            },
            scale_milli: 1000,
            reduced_motion: false,
            radius: 10,
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
                accent_soft: Color::rgb(0, 0, 0),
                accent: Color::rgb(255, 255, 0),
                accent_hover: Color::rgb(255, 255, 255),
                text: Color::rgb(255, 255, 255),
                text_muted: Color::rgb(230, 230, 230),
                border: Color::rgb(255, 255, 255),
                danger: Color::rgb(255, 90, 90),
                success: Color::rgb(0, 255, 0),
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
        let selected = state.contains(NodeState::SELECTED);
        let invalid = state.contains(NodeState::INVALID);
        let interactive = kind.focusable();
        let compact_choice = matches!(kind, ComponentKind::CheckBox | ComponentKind::RadioButton);
        let mut background = match kind {
            ComponentKind::Root => Some(self.palette.window),
            ComponentKind::Panel
            | ComponentKind::Menu
            | ComponentKind::Dialog
            | ComponentKind::Toolbar
            | ComponentKind::StatusBar => Some(self.palette.surface),
            ComponentKind::ScrollView
            | ComponentKind::ListView
            | ComponentKind::TreeView
            | ComponentKind::TableView
            | ComponentKind::GridView => Some(self.palette.raised),
            ComponentKind::Button
            | ComponentKind::TextField
            | ComponentKind::TextArea
            | ComponentKind::Select
            | ComponentKind::TabView => Some(self.palette.raised),
            ComponentKind::CheckBox | ComponentKind::RadioButton | ComponentKind::Switch => None,
            ComponentKind::ProgressBar | ComponentKind::Slider | ComponentKind::Divider => {
                Some(self.palette.border)
            }
            _ => None,
        };
        if interactive && selected && !compact_choice {
            background = Some(self.palette.accent);
        }
        if hovered && matches!(kind, ComponentKind::Button | ComponentKind::TabView) {
            background = Some(if style == style_class::PRIMARY {
                self.palette.accent_hover
            } else {
                self.palette.accent_soft
            });
        }
        if pressed {
            background = background.map(|color| color.mix(Color::rgb(0, 0, 0), 70));
        }
        if style == style_class::PRIMARY {
            background = Some(if hovered {
                self.palette.accent_hover
            } else {
                self.palette.accent
            });
        } else if style == style_class::DANGER {
            background = Some(if hovered {
                self.palette.danger.mix(Color::rgb(255, 255, 255), 32)
            } else {
                self.palette.danger
            });
        } else if style == style_class::GHOST {
            background = None;
        } else if style == style_class::CARD {
            background = Some(self.palette.surface);
        } else if style == style_class::SUBTLE {
            background = Some(self.palette.accent_soft);
        }
        let strong_fill = selected && !compact_choice && kind != ComponentKind::Switch
            || matches!(style, style_class::PRIMARY | style_class::DANGER);
        let foreground = if disabled {
            self.palette.text_muted.mix(self.palette.window, 100)
        } else if strong_fill {
            Color::rgb(255, 255, 255)
        } else if style == style_class::CAPTION {
            self.palette.text_muted
        } else if style == style_class::GHOST {
            self.palette.accent
        } else {
            self.palette.text
        };
        let base_font_size: u16 = if style == style_class::HEADING {
            19
        } else if style == style_class::CAPTION {
            13
        } else if kind == ComponentKind::Text {
            15
        } else {
            14
        };
        let font_size = (u32::from(base_font_size) * u32::from(self.scale_milli.max(500)) / 1_000)
            .clamp(10, 48) as u16;
        let radius = match kind {
            ComponentKind::Root | ComponentKind::Text | ComponentKind::Image => 0,
            ComponentKind::Menu | ComponentKind::Dialog => self.radius.saturating_add(4),
            ComponentKind::Panel if style == style_class::CARD => self.radius.saturating_add(2),
            ComponentKind::ProgressBar | ComponentKind::Slider => u8::MAX,
            _ => self.radius,
        };
        let border_width =
            if compact_choice || kind == ComponentKind::Switch || style == style_class::GHOST {
                0
            } else {
                u8::from(interactive || background.is_some())
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
            border_width,
            radius,
            font_size,
            bold: matches!(style, style_class::PRIMARY | style_class::HEADING),
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
    /// Скругление surface в логических пикселях; `u8::MAX` означает pill.
    pub radius: u8,
    /// Размер системного шрифта.
    pub font_size: u16,
    /// Жирное начертание.
    pub bold: bool,
}

/// Стабильные style classes стандартной темы. Это небольшой semantic API, а
/// не CSS: приложение выбирает назначение, theme — конкретные цвета и форму.
pub mod style_class {
    /// Обычный компонент.
    pub const DEFAULT: u16 = 0;
    /// Основное действие.
    pub const PRIMARY: u16 = 1;
    /// Опасное действие.
    pub const DANGER: u16 = 2;
    /// Текстовая/прозрачная кнопка.
    pub const GHOST: u16 = 3;
    /// Поднятая карточка.
    pub const CARD: u16 = 4;
    /// Мягко выделенная область.
    pub const SUBTLE: u16 = 5;
    /// Заголовок секции.
    pub const HEADING: u16 = 6;
    /// Вторичный поясняющий текст.
    pub const CAPTION: u16 = 7;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_control_and_scale_are_resolved_by_shared_theme() {
        let mut theme = Theme::dark();
        theme.scale_milli = 1_500;
        let style = theme.resolve(ComponentKind::Button, NodeState::SELECTED, 0);
        assert_eq!(style.background, Some(theme.palette.accent));
        assert_eq!(style.foreground, Color::rgb(255, 255, 255));
        assert_eq!(style.radius, theme.radius);
        assert_eq!(style.font_size, 21);
    }

    #[test]
    fn modern_theme_separates_card_ghost_and_choice_geometry() {
        let theme = Theme::light();
        let card = theme.resolve(ComponentKind::Panel, NodeState(0), style_class::CARD);
        let ghost = theme.resolve(ComponentKind::Button, NodeState(0), style_class::GHOST);
        let choice = theme.resolve(ComponentKind::CheckBox, NodeState(0), style_class::DEFAULT);
        let caption = theme.resolve(ComponentKind::Text, NodeState(0), style_class::CAPTION);
        assert_eq!(card.background, Some(theme.palette.surface));
        assert!(card.radius > theme.radius);
        assert_eq!(ghost.background, None);
        assert_eq!(ghost.foreground, theme.palette.accent);
        assert_eq!(choice.background, None);
        assert_eq!(choice.border_width, 0);
        assert_eq!(caption.foreground, theme.palette.text_muted);
        assert!(
            caption.font_size
                < theme
                    .resolve(ComponentKind::Text, NodeState(0), 0)
                    .font_size
        );
    }
}

//! Renderer-independent display list and backend contract.

use rustos_video::{Color, DamageRegion, Rect};

use crate::{ComponentKind, Content, NodeId, NodeState, ResourceId, Theme, Tree};

/// Независимое от конкретной font-библиотеки описание системного шрифта.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontSpec {
    /// Размер в логических пикселях.
    pub size: u16,
    /// Жирное начертание.
    pub bold: bool,
    /// Курсив.
    pub italic: bool,
    /// Моноширинное семейство.
    pub monospace: bool,
    /// Горизонтальное выравнивание внутри bounds компонента.
    pub align: TextAlign,
    /// Центрировать строку по вертикали внутри bounds.
    pub vertical_center: bool,
}

/// Выравнивание текста, независимое от конкретной font-библиотеки.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextAlign {
    /// От начала строки.
    #[default]
    Start = 0,
    /// По центру.
    Center = 1,
    /// К правому краю.
    End = 2,
}

/// Один визуальный primitive. Компоненты не видят методы framebuffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisualPrimitive {
    /// Мягкая тень поднятой поверхности.
    Shadow {
        /// Исходная поверхность: bounds команды шире и покрывают размытие.
        surface: Rect,
        /// Радиус поверхности.
        radius: u8,
        /// Семантический цвет тени, согласованный со светлой/тёмной темой.
        color: Color,
    },
    /// Непрозрачная заливка.
    Fill {
        /// Цвет заливки.
        color: Color,
        /// Скругление.
        radius: u8,
    },
    /// Прямоугольная рамка.
    Border {
        /// Цвет рамки.
        color: Color,
        /// Толщина в логических пикселях.
        width: u8,
        /// Скругление, совпадающее с surface.
        radius: u8,
    },
    /// Текст из package resource table.
    Text {
        /// String resource.
        resource: ResourceId,
        /// Цвет текста.
        color: Color,
        /// Системный шрифт.
        font: FontSpec,
    },
    /// Иконка/изображение. Backend может использовать общий resource cache.
    Image {
        /// Image/icon resource.
        resource: ResourceId,
        /// Цветовая модуляция.
        tint: Color,
    },
    /// Заполненная доля progress/slider, значение 0..=1000.
    Fraction {
        /// Цвет заполненной части.
        color: Color,
        /// Доля 0..=1000.
        value: u16,
        /// Pill/rounded geometry заполнения.
        radius: u8,
    },
    /// Галочка/точка выбора без отдельного bitmap resource.
    SelectionMark {
        /// Цвет отметки.
        color: Color,
        /// Скругление отметки.
        radius: u8,
    },
}

/// Команда display list вместе с bounds и owner для inspector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayCommand {
    /// Компонент-источник.
    pub owner: NodeId,
    /// Clip, накопленный от scrollable-предков владельца.
    pub clip: Rect,
    /// Bounds primitive в координатах surface.
    pub bounds: Rect,
    /// Primitive.
    pub primitive: VisualPrimitive,
}

impl DisplayCommand {
    const EMPTY: Self = Self {
        owner: NodeId::NONE,
        clip: Rect::EMPTY,
        bounds: Rect::EMPTY,
        primitive: VisualPrimitive::Fill {
            color: Color::rgb(0, 0, 0),
            radius: 0,
        },
    };
}

/// Интерфейс backend. CPU, GPU и headless реализации получают одинаковые
/// команды и clip; приложение никогда не зависит от выбранного backend.
pub trait RenderBackend {
    /// Заполнить rectangle внутри clip.
    fn fill(&mut self, rect: Rect, color: Color, clip: Rect);
    /// Нарисовать границу.
    fn border(&mut self, rect: Rect, color: Color, width: u8, clip: Rect);
    /// Нарисовать строковый ресурс.
    fn text(&mut self, rect: Rect, resource: ResourceId, color: Color, font: FontSpec, clip: Rect);
    /// Нарисовать image/icon resource.
    fn image(&mut self, rect: Rect, resource: ResourceId, tint: Color, clip: Rect);

    /// Нарисовать тень поверхности. Простые/headless backend могут ничего не
    /// делать, не меняя функциональную семантику интерфейса.
    fn shadow(&mut self, _rect: Rect, _radius: u8, _color: Color, _clip: Rect) {}

    /// Скруглённая заливка с обязательным прямоугольным fallback.
    fn rounded_fill(&mut self, rect: Rect, color: Color, radius: u8, clip: Rect) {
        let _ = radius;
        self.fill(rect, color, clip);
    }

    /// Скруглённая рамка с обязательным прямоугольным fallback.
    fn rounded_border(&mut self, rect: Rect, color: Color, width: u8, radius: u8, clip: Rect) {
        let _ = radius;
        self.border(rect, color, width, clip);
    }
}

/// Bounded display list. При переполнении frame считается неготовым: runtime
/// никогда не показывает частично построенный интерфейс.
pub struct DisplayList<const C: usize> {
    commands: [DisplayCommand; C],
    len: usize,
    overflowed: bool,
}

impl<const C: usize> DisplayList<C> {
    /// Пустой список.
    pub const fn new() -> Self {
        Self {
            commands: [DisplayCommand::EMPTY; C],
            len: 0,
            overflowed: false,
        }
    }

    /// Удалить команды предыдущего кадра, сохранив storage.
    pub fn clear(&mut self) {
        self.len = 0;
        self.overflowed = false;
    }

    /// Добавить primitive.
    pub fn push(&mut self, command: DisplayCommand) {
        if self.len == C {
            self.overflowed = true;
            return;
        }
        self.commands[self.len] = command;
        self.len += 1;
    }

    /// Команды в paint order.
    pub fn as_slice(&self) -> &[DisplayCommand] {
        &self.commands[..self.len]
    }

    /// Число команд.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Проверка пустоты.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Переполнение configured budget.
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub(crate) fn rebuild<const N: usize>(&mut self, tree: &Tree<N>, theme: Theme) {
        self.clear();
        for id in tree.ids() {
            let node = tree.get(id).expect("iterator yields live nodes");
            if node.rect.is_empty() || node.state.contains(NodeState::HIDDEN) {
                continue;
            }
            let clip = tree.paint_clip(id);
            let style = theme.resolve(node.kind, node.state, node.style);
            if node.style == crate::style_class::CARD
                || matches!(node.kind, ComponentKind::Menu | ComponentKind::Dialog)
            {
                self.push(DisplayCommand {
                    owner: id,
                    clip,
                    bounds: shadow_bounds(node.rect),
                    primitive: VisualPrimitive::Shadow {
                        surface: node.rect,
                        radius: style.radius,
                        color: theme.palette.border.mix(theme.palette.window, 160),
                    },
                });
            }
            if let Some(color) = style.background {
                self.push(DisplayCommand {
                    owner: id,
                    clip,
                    bounds: node.rect,
                    primitive: VisualPrimitive::Fill {
                        color,
                        radius: style.radius,
                    },
                });
            }
            if style.border_width != 0 && node.kind != ComponentKind::Root {
                self.push(DisplayCommand {
                    owner: id,
                    clip,
                    bounds: node.rect,
                    primitive: VisualPrimitive::Border {
                        color: style.border,
                        width: style.border_width,
                        radius: style.radius,
                    },
                });
            }
            match node.content {
                Content::Text(resource) => self.push(DisplayCommand {
                    owner: id,
                    clip,
                    bounds: text_bounds(node.rect, node.kind),
                    primitive: VisualPrimitive::Text {
                        resource,
                        color: style.foreground,
                        font: FontSpec {
                            size: style.font_size,
                            bold: style.bold,
                            italic: false,
                            monospace: false,
                            align: if node.kind == ComponentKind::Button
                                && node.role != crate::SemanticRole::MenuItem
                            {
                                TextAlign::Center
                            } else {
                                TextAlign::Start
                            },
                            vertical_center: node.kind != ComponentKind::Text,
                        },
                    },
                }),
                Content::Resource(resource) => self.push(DisplayCommand {
                    owner: id,
                    clip,
                    bounds: inset(node.rect, 5, 5),
                    primitive: VisualPrimitive::Image {
                        resource,
                        tint: style.foreground,
                    },
                }),
                Content::Value(value)
                    if matches!(
                        node.kind,
                        ComponentKind::ProgressBar | ComponentKind::Slider
                    ) =>
                {
                    self.push(DisplayCommand {
                        owner: id,
                        clip,
                        bounds: inset(node.rect, 2, 2),
                        primitive: VisualPrimitive::Fraction {
                            color: theme.palette.accent,
                            value: value.min(1000),
                            radius: style.radius,
                        },
                    });
                }
                _ => {}
            }
            self.push_choice_indicator(id, node, theme, clip);
            if node.state.contains(NodeState::FOCUS_VISIBLE) {
                self.push(DisplayCommand {
                    owner: id,
                    clip,
                    bounds: node.rect,
                    primitive: VisualPrimitive::Border {
                        color: style.focus,
                        width: 2,
                        radius: style.radius,
                    },
                });
            }
        }
        // Overlay scrollbars должны быть поверх дочернего содержимого, поэтому
        // формируются отдельным проходом после обычных primitives.
        for id in tree.ids() {
            let node = tree.get(id).expect("iterator yields live nodes");
            if !node.state.contains(NodeState::HIDDEN) {
                self.push_scrollbars(id, node, theme, tree.paint_clip(id), tree);
            }
        }
    }

    pub(crate) fn execute<B: RenderBackend, const D: usize>(
        &self,
        backend: &mut B,
        damage: &DamageRegion<D>,
    ) -> (u32, u64) {
        let mut executed = 0u32;
        let mut pixels = 0u64;
        for clip in damage.iter().copied() {
            for command in self.as_slice() {
                let command_clip = command.clip.intersection(clip);
                let visible = command.bounds.intersection(command_clip);
                if visible.is_empty() {
                    continue;
                }
                executed = executed.saturating_add(1);
                pixels = pixels.saturating_add(visible.area());
                match command.primitive {
                    VisualPrimitive::Shadow {
                        surface,
                        radius,
                        color,
                    } => backend.shadow(surface, radius, color, command_clip),
                    VisualPrimitive::Fill { color, radius } => {
                        backend.rounded_fill(command.bounds, color, radius, command_clip)
                    }
                    VisualPrimitive::Border {
                        color,
                        width,
                        radius,
                    } => backend.rounded_border(command.bounds, color, width, radius, command_clip),
                    VisualPrimitive::Text {
                        resource,
                        color,
                        font,
                    } => backend.text(command.bounds, resource, color, font, command_clip),
                    VisualPrimitive::Image { resource, tint } => {
                        backend.image(command.bounds, resource, tint, command_clip)
                    }
                    VisualPrimitive::Fraction {
                        color,
                        value,
                        radius,
                    } => {
                        let inner = Rect::new(
                            command.bounds.x,
                            command.bounds.y,
                            command.bounds.width.saturating_mul(u32::from(value)) / 1000,
                            command.bounds.height,
                        );
                        backend.rounded_fill(inner, color, radius, command_clip);
                    }
                    VisualPrimitive::SelectionMark { color, radius } => {
                        backend.rounded_fill(command.bounds, color, radius, command_clip)
                    }
                }
            }
        }
        (executed, pixels)
    }
}

impl<const C: usize> DisplayList<C> {
    fn push_choice_indicator(
        &mut self,
        owner: NodeId,
        node: &crate::Node,
        theme: Theme,
        clip: Rect,
    ) {
        let checked = node.state.contains(NodeState::CHECKED);
        match node.kind {
            ComponentKind::CheckBox | ComponentKind::RadioButton => {
                let bounds = choice_bounds(node.rect);
                let radius = if node.kind == ComponentKind::RadioButton {
                    u8::MAX
                } else {
                    5
                };
                self.push(DisplayCommand {
                    owner,
                    clip,
                    bounds,
                    primitive: VisualPrimitive::Fill {
                        color: if checked {
                            theme.palette.accent
                        } else {
                            theme.palette.raised
                        },
                        radius,
                    },
                });
                self.push(DisplayCommand {
                    owner,
                    clip,
                    bounds,
                    primitive: VisualPrimitive::Border {
                        color: if checked {
                            theme.palette.accent
                        } else {
                            theme.palette.border
                        },
                        width: 1,
                        radius,
                    },
                });
                if checked {
                    self.push(DisplayCommand {
                        owner,
                        clip,
                        bounds: inset(bounds, 5, 5),
                        primitive: VisualPrimitive::SelectionMark {
                            color: Color::rgb(255, 255, 255),
                            radius: if node.kind == ComponentKind::RadioButton {
                                u8::MAX
                            } else {
                                2
                            },
                        },
                    });
                }
            }
            ComponentKind::Switch => {
                let track = switch_bounds(node.rect);
                self.push(DisplayCommand {
                    owner,
                    clip,
                    bounds: track,
                    primitive: VisualPrimitive::Fill {
                        color: if checked {
                            theme.palette.accent
                        } else {
                            theme.palette.border
                        },
                        radius: u8::MAX,
                    },
                });
                let knob_size = track.height.saturating_sub(4);
                let knob_x = if checked {
                    track.right().saturating_sub(knob_size as i32 + 2)
                } else {
                    track.x.saturating_add(2)
                };
                self.push(DisplayCommand {
                    owner,
                    clip,
                    bounds: Rect::new(knob_x, track.y.saturating_add(2), knob_size, knob_size),
                    primitive: VisualPrimitive::SelectionMark {
                        color: Color::rgb(255, 255, 255),
                        radius: u8::MAX,
                    },
                });
            }
            _ => {}
        }
    }

    fn push_scrollbars<const N: usize>(
        &mut self,
        owner: NodeId,
        node: &crate::Node,
        theme: Theme,
        clip: Rect,
        tree: &Tree<N>,
    ) {
        if node.kind == ComponentKind::ScrollBar {
            let Some(target) = tree.get(node.scroll_target) else {
                return;
            };
            let model = target.scroll.model(node.scroll_axis);
            let thickness = match node.scroll_axis {
                crate::ScrollAxis::Horizontal => node.rect.height,
                crate::ScrollAxis::Vertical => node.rect.width,
            };
            let geometry = crate::ScrollbarGeometry::with_visibility(
                node.rect,
                model,
                node.scroll_axis,
                thickness,
                24,
                true,
            );
            self.push_scrollbar_geometry(owner, node, theme, clip, geometry);
            return;
        }
        for axis in [crate::ScrollAxis::Horizontal, crate::ScrollAxis::Vertical] {
            let policy = match axis {
                crate::ScrollAxis::Horizontal => node.scroll.config.horizontal,
                crate::ScrollAxis::Vertical => node.scroll.config.vertical,
            };
            if policy == crate::ScrollBarPolicy::Hidden {
                continue;
            }
            let model = node.scroll.model(axis);
            let geometry = crate::ScrollbarGeometry::with_visibility(
                node.rect,
                model,
                axis,
                10,
                24,
                model.can_scroll() || policy == crate::ScrollBarPolicy::Always,
            );
            if !geometry.visible {
                continue;
            }
            self.push_scrollbar_geometry(owner, node, theme, clip, geometry);
        }
    }

    fn push_scrollbar_geometry(
        &mut self,
        owner: NodeId,
        node: &crate::Node,
        theme: Theme,
        clip: Rect,
        geometry: crate::ScrollbarGeometry,
    ) {
        self.push(DisplayCommand {
            owner,
            clip,
            bounds: geometry.track,
            primitive: VisualPrimitive::Fill {
                color: theme.palette.border.mix(theme.palette.window, 208),
                radius: u8::MAX,
            },
        });
        self.push(DisplayCommand {
            owner,
            clip,
            bounds: geometry.thumb,
            primitive: VisualPrimitive::Fill {
                color: if node.state.contains(NodeState::HOVERED) {
                    theme.palette.accent
                } else {
                    theme.palette.text_muted
                },
                radius: u8::MAX,
            },
        });
    }
}

impl<const C: usize> Default for DisplayList<C> {
    fn default() -> Self {
        Self::new()
    }
}

fn inset(rect: Rect, horizontal: u32, vertical: u32) -> Rect {
    Rect::new(
        rect.x.saturating_add(horizontal as i32),
        rect.y.saturating_add(vertical as i32),
        rect.width.saturating_sub(horizontal.saturating_mul(2)),
        rect.height.saturating_sub(vertical.saturating_mul(2)),
    )
}

fn shadow_bounds(rect: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_sub(9),
        rect.y.saturating_sub(6),
        rect.width.saturating_add(18),
        rect.height.saturating_add(18),
    )
}

fn choice_bounds(rect: Rect) -> Rect {
    let size = rect.height.saturating_sub(10).min(20);
    Rect::new(
        rect.x.saturating_add(4),
        rect.y
            .saturating_add((rect.height.saturating_sub(size) / 2) as i32),
        size,
        size,
    )
}

fn switch_bounds(rect: Rect) -> Rect {
    let height = rect.height.saturating_sub(10).clamp(16, 22);
    let width = height.saturating_mul(2);
    Rect::new(
        rect.right().saturating_sub(width as i32 + 5),
        rect.y
            .saturating_add((rect.height.saturating_sub(height) / 2) as i32),
        width,
        height,
    )
}

fn text_bounds(rect: Rect, kind: ComponentKind) -> Rect {
    if matches!(kind, ComponentKind::CheckBox | ComponentKind::RadioButton) {
        Rect::new(
            rect.x.saturating_add(28),
            rect.y.saturating_add(5),
            rect.width.saturating_sub(34),
            rect.height.saturating_sub(10),
        )
    } else {
        inset(rect, 8, 5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ComponentKind, LayoutSpec, NodeSpec, Tree};

    struct Counter(u32);

    impl RenderBackend for Counter {
        fn fill(&mut self, _: Rect, _: Color, _: Rect) {
            self.0 += 1;
        }
        fn border(&mut self, _: Rect, _: Color, _: u8, _: Rect) {
            self.0 += 1;
        }
        fn text(&mut self, _: Rect, _: ResourceId, _: Color, _: FontSpec, _: Rect) {
            self.0 += 1;
        }
        fn image(&mut self, _: Rect, _: ResourceId, _: Color, _: Rect) {
            self.0 += 1;
        }
    }

    #[test]
    fn execution_is_clipped_to_damage() {
        let mut tree = Tree::<4>::new();
        let mut spec = NodeSpec::new(ComponentKind::Button);
        spec.layout = LayoutSpec {
            width: crate::Length::Px(50),
            height: crate::Length::Px(30),
            ..LayoutSpec::default()
        };
        tree.create(tree.root(), spec).unwrap();
        crate::layout::perform(&mut tree, Rect::new(0, 0, 200, 100), |_| {});
        let mut list = DisplayList::<16>::new();
        list.rebuild(&tree, Theme::dark());
        let mut damage = DamageRegion::<2>::new(Rect::new(0, 0, 200, 100));
        damage.add(Rect::new(150, 50, 10, 10));
        let mut counter = Counter(0);
        list.execute(&mut counter, &damage);
        assert_eq!(counter.0, 1); // only root background intersects
    }

    #[test]
    fn card_emits_theme_aware_shadow_before_surface() {
        let mut tree = Tree::<4>::new();
        let mut spec = NodeSpec::new(ComponentKind::Panel);
        spec.style = crate::style_class::CARD;
        spec.layout = LayoutSpec {
            width: crate::Length::Px(100),
            height: crate::Length::Px(60),
            ..LayoutSpec::default()
        };
        let card = tree.create(tree.root(), spec).unwrap();
        crate::layout::perform(&mut tree, Rect::new(0, 0, 200, 100), |_| {});

        let theme = Theme::light();
        let mut list = DisplayList::<16>::new();
        list.rebuild(&tree, theme);
        let card_commands: [Option<VisualPrimitive>; 3] = [
            list.as_slice()
                .iter()
                .find(|command| command.owner == card)
                .map(|command| command.primitive),
            list.as_slice()
                .iter()
                .filter(|command| command.owner == card)
                .nth(1)
                .map(|command| command.primitive),
            list.as_slice()
                .iter()
                .filter(|command| command.owner == card)
                .nth(2)
                .map(|command| command.primitive),
        ];
        assert!(matches!(
            card_commands[0],
            Some(VisualPrimitive::Shadow { color, .. })
                if color == theme.palette.border.mix(theme.palette.window, 160)
        ));
        assert!(matches!(
            card_commands[1],
            Some(VisualPrimitive::Fill { .. })
        ));
        assert!(matches!(
            card_commands[2],
            Some(VisualPrimitive::Border { .. })
        ));
    }
}

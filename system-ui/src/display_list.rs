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
}

/// Один визуальный primitive. Компоненты не видят методы framebuffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisualPrimitive {
    /// Непрозрачная заливка.
    Fill {
        /// Цвет заливки.
        color: Color,
    },
    /// Прямоугольная рамка.
    Border {
        /// Цвет рамки.
        color: Color,
        /// Толщина в логических пикселях.
        width: u8,
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
    },
    /// Галочка/точка выбора без отдельного bitmap resource.
    SelectionMark {
        /// Цвет отметки.
        color: Color,
    },
}

/// Команда display list вместе с bounds и owner для inspector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayCommand {
    /// Компонент-источник.
    pub owner: NodeId,
    /// Bounds primitive в координатах surface.
    pub bounds: Rect,
    /// Primitive.
    pub primitive: VisualPrimitive,
}

impl DisplayCommand {
    const EMPTY: Self = Self {
        owner: NodeId::NONE,
        bounds: Rect::EMPTY,
        primitive: VisualPrimitive::Fill {
            color: Color::rgb(0, 0, 0),
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
            if node.rect.is_empty() {
                continue;
            }
            let style = theme.resolve(node.kind, node.state, node.style);
            if let Some(color) = style.background {
                self.push(DisplayCommand {
                    owner: id,
                    bounds: node.rect,
                    primitive: VisualPrimitive::Fill { color },
                });
            }
            if style.border_width != 0 && node.kind != ComponentKind::Root {
                self.push(DisplayCommand {
                    owner: id,
                    bounds: node.rect,
                    primitive: VisualPrimitive::Border {
                        color: style.border,
                        width: style.border_width,
                    },
                });
            }
            match node.content {
                Content::Text(resource) => self.push(DisplayCommand {
                    owner: id,
                    bounds: text_bounds(node.rect, node.kind),
                    primitive: VisualPrimitive::Text {
                        resource,
                        color: style.foreground,
                        font: FontSpec {
                            size: style.font_size,
                            bold: style.bold,
                            italic: false,
                            monospace: false,
                        },
                    },
                }),
                Content::Resource(resource) => self.push(DisplayCommand {
                    owner: id,
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
                        bounds: inset(node.rect, 2, 2),
                        primitive: VisualPrimitive::Fraction {
                            color: theme.palette.accent,
                            value: value.min(1000),
                        },
                    });
                }
                _ => {}
            }
            if matches!(
                node.kind,
                ComponentKind::CheckBox | ComponentKind::RadioButton | ComponentKind::Switch
            ) && node.state.contains(NodeState::CHECKED)
            {
                self.push(DisplayCommand {
                    owner: id,
                    bounds: selection_bounds(node.rect, node.kind),
                    primitive: VisualPrimitive::SelectionMark {
                        color: theme.palette.accent,
                    },
                });
            }
            if node.state.contains(NodeState::FOCUSED) {
                self.push(DisplayCommand {
                    owner: id,
                    bounds: node.rect,
                    primitive: VisualPrimitive::Border {
                        color: style.focus,
                        width: 2,
                    },
                });
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
                let visible = command.bounds.intersection(clip);
                if visible.is_empty() {
                    continue;
                }
                executed = executed.saturating_add(1);
                pixels = pixels.saturating_add(visible.area());
                match command.primitive {
                    VisualPrimitive::Fill { color } => backend.fill(command.bounds, color, clip),
                    VisualPrimitive::Border { color, width } => {
                        backend.border(command.bounds, color, width, clip)
                    }
                    VisualPrimitive::Text {
                        resource,
                        color,
                        font,
                    } => backend.text(command.bounds, resource, color, font, clip),
                    VisualPrimitive::Image { resource, tint } => {
                        backend.image(command.bounds, resource, tint, clip)
                    }
                    VisualPrimitive::Fraction { color, value } => {
                        let inner = Rect::new(
                            command.bounds.x,
                            command.bounds.y,
                            command.bounds.width.saturating_mul(u32::from(value)) / 1000,
                            command.bounds.height,
                        );
                        backend.fill(inner, color, clip);
                    }
                    VisualPrimitive::SelectionMark { color } => {
                        backend.fill(command.bounds, color, clip)
                    }
                }
            }
        }
        (executed, pixels)
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

fn selection_bounds(rect: Rect, kind: ComponentKind) -> Rect {
    match kind {
        ComponentKind::Switch => Rect::new(
            rect.x.saturating_add(rect.width.saturating_sub(22) as i32),
            rect.y.saturating_add(4),
            16,
            rect.height.saturating_sub(8),
        ),
        _ => Rect::new(rect.x.saturating_add(5), rect.y.saturating_add(5), 12, 12),
    }
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
}

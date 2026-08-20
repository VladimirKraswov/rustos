//! Целочисленный layout без FPU и аллокаций.

use rustos_video::Rect;

use crate::{ComponentKind, NodeId, Tree};

/// Размер по одной оси.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Length {
    /// Размер определяется содержимым или minimum.
    Auto,
    /// Фиксированное число логических пикселей.
    Px(u16),
    /// Доля доступного пространства в тысячных (1000 = 100%).
    Percent(u16),
    /// Доля оставшегося пространства. Ноль трактуется как вес 1.
    Fill(u16),
}

impl Default for Length {
    fn default() -> Self {
        Self::Auto
    }
}

/// Отступы в логических пикселях.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Edges {
    /// Слева.
    pub left: u16,
    /// Сверху.
    pub top: u16,
    /// Справа.
    pub right: u16,
    /// Снизу.
    pub bottom: u16,
}

impl Edges {
    /// Одинаковый отступ со всех сторон.
    pub const fn all(value: u16) -> Self {
        Self {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    /// Отдельные горизонтальный и вертикальный отступы.
    pub const fn symmetric(horizontal: u16, vertical: u16) -> Self {
        Self {
            left: horizontal,
            top: vertical,
            right: horizontal,
            bottom: vertical,
        }
    }
}

/// Выравнивание дочернего элемента по поперечной оси.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Align {
    /// Начало оси.
    Start = 0,
    /// Центр.
    Center = 1,
    /// Конец оси.
    End = 2,
    /// Растянуть до доступного размера.
    #[default]
    Stretch = 3,
}

/// Layout-свойства узла. Все значения bounded и пригодны для UI IR.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutSpec {
    /// Ширина.
    pub width: Length,
    /// Высота.
    pub height: Length,
    /// Минимальная ширина.
    pub min_width: u16,
    /// Минимальная высота.
    pub min_height: u16,
    /// Максимальная ширина; ноль означает отсутствие ограничения.
    pub max_width: u16,
    /// Максимальная высота; ноль означает отсутствие ограничения.
    pub max_height: u16,
    /// Внутренние отступы контейнера.
    pub padding: Edges,
    /// Интервал между дочерними элементами.
    pub gap: u16,
    /// Поперечное выравнивание.
    pub align: Align,
    /// Число колонок Grid; минимум 1.
    pub grid_columns: u8,
    /// Если ненулевой, Row становится Column при меньшей ширине контейнера.
    pub container_breakpoint: u16,
}

impl Default for LayoutSpec {
    fn default() -> Self {
        Self {
            width: Length::Auto,
            height: Length::Auto,
            min_width: 0,
            min_height: 0,
            max_width: 0,
            max_height: 0,
            padding: Edges::default(),
            gap: 0,
            align: Align::Stretch,
            grid_columns: 1,
            container_breakpoint: 0,
        }
    }
}

impl LayoutSpec {
    /// Заполнить всё доступное место.
    pub const fn fill() -> Self {
        Self {
            width: Length::Fill(1),
            height: Length::Fill(1),
            min_width: 0,
            min_height: 0,
            max_width: 0,
            max_height: 0,
            padding: Edges {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            gap: 0,
            align: Align::Stretch,
            grid_columns: 1,
            container_breakpoint: 0,
        }
    }
}

pub(crate) fn perform<const N: usize, F>(tree: &mut Tree<N>, viewport: Rect, mut damaged: F)
where
    F: FnMut(Rect),
{
    let root = tree.root();
    set_rect(tree, root, viewport, &mut damaged);
    layout_children(tree, root, &mut damaged);
    tree.clear_layout_dirty();
}

fn layout_children<const N: usize, F>(tree: &mut Tree<N>, parent: NodeId, damaged: &mut F)
where
    F: FnMut(Rect),
{
    let Some(parent_node) = tree.get(parent).copied() else {
        return;
    };
    let content = content_rect(parent_node.rect, parent_node.layout.padding);
    let kind = if parent_node.kind == ComponentKind::Row
        && parent_node.layout.container_breakpoint != 0
        && content.width < u32::from(parent_node.layout.container_breakpoint)
    {
        ComponentKind::Column
    } else {
        parent_node.kind
    };

    match kind {
        ComponentKind::Row => linear(tree, parent, content, true, parent_node.layout.gap, damaged),
        ComponentKind::Column
        | ComponentKind::Panel
        | ComponentKind::ScrollView
        | ComponentKind::ListView => linear(
            tree,
            parent,
            content,
            false,
            parent_node.layout.gap,
            damaged,
        ),
        ComponentKind::Grid => grid(tree, parent, content, parent_node.layout, damaged),
        _ => stack(tree, parent, content, damaged),
    }
}

fn linear<const N: usize, F>(
    tree: &mut Tree<N>,
    parent: NodeId,
    content: Rect,
    horizontal: bool,
    gap: u16,
    damaged: &mut F,
) where
    F: FnMut(Rect),
{
    let count = tree.child_count(parent);
    if count == 0 {
        return;
    }
    let main_available = if horizontal {
        content.width
    } else {
        content.height
    };
    let cross_available = if horizontal {
        content.height
    } else {
        content.width
    };
    let gaps = u32::from(gap).saturating_mul(count.saturating_sub(1) as u32);
    let mut fixed = gaps;
    let mut fill_weight = 0u32;
    let mut child = tree.first_child(parent);
    while let Some(id) = child {
        let node = *tree.get(id).expect("child ID belongs to tree");
        let main = if horizontal {
            node.layout.width
        } else {
            node.layout.height
        };
        match main {
            Length::Fill(weight) => {
                fill_weight = fill_weight.saturating_add(u32::from(weight.max(1)))
            }
            _ => {
                fixed =
                    fixed.saturating_add(resolve(main, main_available, minimum(node, horizontal)))
            }
        }
        child = tree.next_sibling(id);
    }
    let free = main_available.saturating_sub(fixed);
    let mut cursor = if horizontal { content.x } else { content.y };
    child = tree.first_child(parent);
    while let Some(id) = child {
        let node = *tree.get(id).expect("child ID belongs to tree");
        let main_length = if horizontal {
            node.layout.width
        } else {
            node.layout.height
        };
        let mut main = match main_length {
            Length::Fill(weight) if fill_weight != 0 => {
                free.saturating_mul(u32::from(weight.max(1))) / fill_weight
            }
            _ => resolve(main_length, main_available, minimum(node, horizontal)),
        };
        main = constrain(main, node, horizontal);
        let cross_length = if horizontal {
            node.layout.height
        } else {
            node.layout.width
        };
        let mut cross = if node.layout.align == Align::Stretch
            && matches!(cross_length, Length::Auto | Length::Fill(_))
        {
            cross_available
        } else {
            resolve(cross_length, cross_available, minimum(node, !horizontal))
        };
        cross = constrain(cross, node, !horizontal).min(cross_available);
        let cross_origin = align_origin(
            if horizontal { content.y } else { content.x },
            cross_available,
            cross,
            node.layout.align,
        );
        let rect = if horizontal {
            Rect::new(cursor, cross_origin, main, cross)
        } else {
            Rect::new(cross_origin, cursor, cross, main)
        };
        set_rect(tree, id, rect, damaged);
        layout_children(tree, id, damaged);
        cursor = cursor.saturating_add(main.min(i32::MAX as u32) as i32 + i32::from(gap));
        child = tree.next_sibling(id);
    }
}

fn stack<const N: usize, F>(tree: &mut Tree<N>, parent: NodeId, content: Rect, damaged: &mut F)
where
    F: FnMut(Rect),
{
    let mut child = tree.first_child(parent);
    while let Some(id) = child {
        let node = *tree.get(id).expect("child ID belongs to tree");
        let width = constrain(
            resolve(node.layout.width, content.width, node.layout.min_width),
            node,
            true,
        )
        .min(content.width);
        let height = constrain(
            resolve(node.layout.height, content.height, node.layout.min_height),
            node,
            false,
        )
        .min(content.height);
        let rect = Rect::new(
            align_origin(content.x, content.width, width, node.layout.align),
            align_origin(content.y, content.height, height, node.layout.align),
            width,
            height,
        );
        set_rect(tree, id, rect, damaged);
        layout_children(tree, id, damaged);
        child = tree.next_sibling(id);
    }
}

fn grid<const N: usize, F>(
    tree: &mut Tree<N>,
    parent: NodeId,
    content: Rect,
    spec: LayoutSpec,
    damaged: &mut F,
) where
    F: FnMut(Rect),
{
    let columns = u32::from(spec.grid_columns.max(1));
    let gap = u32::from(spec.gap);
    let cell_width = content
        .width
        .saturating_sub(gap.saturating_mul(columns.saturating_sub(1)))
        / columns;
    let mut index = 0u32;
    let mut child = tree.first_child(parent);
    while let Some(id) = child {
        let node = *tree.get(id).expect("child ID belongs to tree");
        let column = index % columns;
        let row = index / columns;
        let height = constrain(
            resolve(
                node.layout.height,
                content.height,
                node.layout.min_height.max(36),
            ),
            node,
            false,
        );
        let x = content
            .x
            .saturating_add((column * (cell_width + gap)) as i32);
        let y = content.y.saturating_add((row * (height + gap)) as i32);
        let rect = Rect::new(x, y, cell_width, height);
        set_rect(tree, id, rect, damaged);
        layout_children(tree, id, damaged);
        index = index.saturating_add(1);
        child = tree.next_sibling(id);
    }
}

fn set_rect<const N: usize, F>(tree: &mut Tree<N>, id: NodeId, rect: Rect, damaged: &mut F)
where
    F: FnMut(Rect),
{
    if let Some(node) = tree.get_mut_internal(id) {
        if node.rect != rect {
            damaged(node.rect);
            damaged(rect);
            node.rect = rect;
            node.dirty.insert(crate::DirtyFlags::PAINT);
        }
    }
}

fn content_rect(rect: Rect, padding: Edges) -> Rect {
    let horizontal = u32::from(padding.left) + u32::from(padding.right);
    let vertical = u32::from(padding.top) + u32::from(padding.bottom);
    Rect::new(
        rect.x.saturating_add(i32::from(padding.left)),
        rect.y.saturating_add(i32::from(padding.top)),
        rect.width.saturating_sub(horizontal),
        rect.height.saturating_sub(vertical),
    )
}

fn resolve(length: Length, available: u32, intrinsic: u16) -> u32 {
    match length {
        Length::Auto => u32::from(intrinsic),
        Length::Px(value) => u32::from(value),
        Length::Percent(value) => available.saturating_mul(u32::from(value.min(1000))) / 1000,
        Length::Fill(_) => available,
    }
}

fn minimum(node: crate::Node, horizontal: bool) -> u16 {
    if horizontal {
        node.layout.min_width
    } else {
        node.layout.min_height
    }
}

fn constrain(value: u32, node: crate::Node, horizontal: bool) -> u32 {
    let (minimum, maximum) = if horizontal {
        (node.layout.min_width, node.layout.max_width)
    } else {
        (node.layout.min_height, node.layout.max_height)
    };
    let value = value.max(u32::from(minimum));
    if maximum == 0 {
        value
    } else {
        value.min(u32::from(maximum))
    }
}

fn align_origin(origin: i32, available: u32, size: u32, align: Align) -> i32 {
    let remaining = available.saturating_sub(size).min(i32::MAX as u32) as i32;
    origin.saturating_add(match align {
        Align::Start | Align::Stretch => 0,
        Align::Center => remaining / 2,
        Align::End => remaining,
    })
}

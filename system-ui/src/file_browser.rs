//! Переиспользуемый составной обозреватель иерархических ресурсов.
//!
//! Composite не знает VFS, путей и прав доступа. Приложение передаёт только
//! ResourceId, CommandId и состояние элементов; тот же виджет можно применять
//! для файлов, пакетов, устройств или project tree редактора.

use crate::{
    style_class, Align, CommandId, ComponentKind, Content, Edges, LayoutSpec, Length, NodeId,
    NodeSpec, NodeState, ResourceId, ScrollBarLayout, ScrollBarPolicy, ScrollConfig, SemanticRole,
    TreeError, UiBuilder,
};

/// Представление правой части обозревателя.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FileBrowserView {
    /// Крупные плитки.
    #[default]
    Grid = 0,
    /// Компактный список.
    List = 1,
    /// Таблица с отдельными колонками.
    Details = 2,
}

/// Один пункт дерева навигации.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileBrowserTreeItem {
    /// Отображаемая строка.
    pub label: ResourceId,
    /// Иконка resource pack.
    pub icon: ResourceId,
    /// Команда перехода.
    pub command: CommandId,
    /// Уровень вложенности, начиная с нуля.
    pub depth: u8,
    /// Выбран ли текущий путь.
    pub selected: bool,
    /// Доступен ли переход.
    pub disabled: bool,
}

/// Один объект основного представления.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileBrowserItem {
    /// Имя объекта.
    pub label: ResourceId,
    /// Текст колонки «Тип».
    pub kind_label: ResourceId,
    /// Текст колонки «Размер».
    pub size_label: ResourceId,
    /// Иконка объекта.
    pub icon: ResourceId,
    /// Команда активации/выбора.
    pub command: CommandId,
    /// Выделение.
    pub selected: bool,
    /// Вместо Label отображается поле inline-rename.
    pub editing: bool,
}

/// Ресурсы и геометрическая политика составного обозревателя.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileBrowserSpec {
    /// Размер всего SplitView.
    pub layout: LayoutSpec,
    /// Ширина дерева навигации.
    pub navigation_width: u16,
    /// Заголовок дерева.
    pub navigation_heading: ResourceId,
    /// Текст пустого представления.
    pub empty_text: ResourceId,
    /// Заголовок колонки имени.
    pub name_heading: ResourceId,
    /// Заголовок колонки типа.
    pub kind_heading: ResourceId,
    /// Заголовок колонки размера.
    pub size_heading: ResourceId,
    /// Текущее представление.
    pub view: FileBrowserView,
    /// Число колонок GridView.
    pub grid_columns: u8,
}

/// Главные узлы composite для inspector и программного управления.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileBrowserNodes {
    /// Корневой SplitView.
    pub root: NodeId,
    /// Прокручиваемое дерево.
    pub navigation: NodeId,
    /// Прокручиваемое представление объектов.
    pub collection: NodeId,
}

/// Строит дерево слева и переключаемое Grid/List/Details представление справа.
/// `item_nodes` заполняется NodeId интерактивных объектов для hit-test model;
/// лишние slots получают `NodeId::NONE`.
pub fn build_file_browser<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    spec: FileBrowserSpec,
    tree_items: &[FileBrowserTreeItem],
    items: &[FileBrowserItem],
    item_nodes: &mut [NodeId],
) -> Result<FileBrowserNodes, TreeError> {
    item_nodes.fill(NodeId::NONE);
    let root = ui.split_view(parent, spec.layout)?;
    let navigation_panel = panel(
        ui,
        root,
        LayoutSpec {
            width: Length::Px(spec.navigation_width),
            height: Length::Fill(1),
            padding: Edges::symmetric(8, 10),
            gap: 7,
            ..LayoutSpec::default()
        },
    )?;
    heading(ui, navigation_panel, spec.navigation_heading, 28)?;
    let navigation = ui.tree_view(
        navigation_panel,
        visible_scroll(),
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            padding: Edges::symmetric(3, 4),
            gap: 3,
            ..LayoutSpec::default()
        },
    )?;
    for item in tree_items {
        build_tree_item(ui, navigation, *item)?;
    }

    let content = panel(
        ui,
        root,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            padding: Edges::all(8),
            gap: 6,
            ..LayoutSpec::default()
        },
    )?;
    let collection = if items.is_empty() {
        ui.text(
            content,
            spec.empty_text,
            LayoutSpec {
                width: Length::Fill(1),
                height: Length::Fill(1),
                align: Align::Center,
                ..LayoutSpec::default()
            },
        )?
    } else {
        match spec.view {
            FileBrowserView::Grid => build_grid(ui, content, spec, items, item_nodes)?,
            FileBrowserView::List => build_list(ui, content, items, item_nodes)?,
            FileBrowserView::Details => build_table(ui, content, spec, items, item_nodes)?,
        }
    };
    Ok(FileBrowserNodes {
        root,
        navigation,
        collection,
    })
}

fn build_tree_item<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    item: FileBrowserTreeItem,
) -> Result<NodeId, TreeError> {
    let mut button = NodeSpec::new(ComponentKind::Button);
    button.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(38),
        ..LayoutSpec::default()
    };
    button.command = item.command;
    button.role = SemanticRole::TreeItem;
    button.accessible_name = item.label;
    button.style = style_class::GHOST;
    if item.selected {
        button.state.insert(NodeState::SELECTED);
        button.style = style_class::SUBTLE;
    }
    if item.disabled {
        button.state.insert(NodeState::DISABLED);
    }
    let button = ui.component(parent, button)?;
    let row = ui.row(
        button,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            padding: Edges {
                left: 5u16.saturating_add(u16::from(item.depth).saturating_mul(17)),
                top: 5,
                right: 5,
                bottom: 5,
            },
            gap: 7,
            align: Align::Center,
            ..LayoutSpec::default()
        },
    )?;
    ui.image(
        row,
        item.icon,
        ResourceId(0),
        LayoutSpec {
            width: Length::Px(24),
            height: Length::Px(24),
            ..LayoutSpec::default()
        },
    )?;
    ui.text(
        row,
        item.label,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            ..LayoutSpec::default()
        },
    )?;
    Ok(button)
}

fn build_grid<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    spec: FileBrowserSpec,
    items: &[FileBrowserItem],
    item_nodes: &mut [NodeId],
) -> Result<NodeId, TreeError> {
    let scroll = ui.scroll_view(parent, visible_scroll(), LayoutSpec::fill())?;
    let columns = usize::from(spec.grid_columns.max(1));
    let rows = items.len().div_ceil(columns).max(1);
    let grid = ui.grid_view(
        scroll,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(
                u16::try_from(rows.saturating_mul(112).saturating_add(8)).unwrap_or(u16::MAX),
            ),
            padding: Edges::all(6),
            gap: 8,
            grid_columns: spec.grid_columns.max(1),
            ..LayoutSpec::default()
        },
    )?;
    for (index, item) in items.iter().copied().enumerate() {
        let node = item_button(ui, grid, item, 104, SemanticRole::GridCell)?;
        if let Some(slot) = item_nodes.get_mut(index) {
            *slot = node;
        }
        ui.image(
            node,
            item.icon,
            item.label,
            LayoutSpec {
                width: Length::Px(48),
                height: Length::Px(48),
                align: Align::Center,
                ..LayoutSpec::default()
            },
        )?;
        item_label(ui, node, item, 30, Align::End)?;
    }
    Ok(scroll)
}

fn build_list<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    items: &[FileBrowserItem],
    item_nodes: &mut [NodeId],
) -> Result<NodeId, TreeError> {
    // ListView получает ту же предсказуемую полосу прокрутки, что GridView и
    // TableView. Приложению не приходится заново настраивать scrollbar при
    // переключении представления одного и того же каталога.
    let mut list = NodeSpec::new(ComponentKind::ListView);
    list.layout = LayoutSpec::fill();
    list.scroll = visible_scroll();
    list.role = SemanticRole::List;
    let list = ui.component(parent, list)?;
    for (index, item) in items.iter().copied().enumerate() {
        let node = item_button(ui, list, item, 42, SemanticRole::ListItem)?;
        if let Some(slot) = item_nodes.get_mut(index) {
            *slot = node;
        }
        let row = item_row(ui, node)?;
        item_icon(ui, row, item)?;
        item_label(ui, row, item, 34, Align::Stretch)?;
    }
    Ok(list)
}

fn build_table<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    spec: FileBrowserSpec,
    items: &[FileBrowserItem],
    item_nodes: &mut [NodeId],
) -> Result<NodeId, TreeError> {
    let header = ui.row(
        parent,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(34),
            padding: Edges::symmetric(9, 5),
            gap: 8,
            ..LayoutSpec::default()
        },
    )?;
    table_text(ui, header, spec.name_heading, 5, style_class::CAPTION)?;
    table_text(ui, header, spec.kind_heading, 2, style_class::CAPTION)?;
    table_text(ui, header, spec.size_heading, 2, style_class::CAPTION)?;
    let table = ui.table_view(parent, visible_scroll(), LayoutSpec::fill())?;
    for (index, item) in items.iter().copied().enumerate() {
        let node = item_button(ui, table, item, 42, SemanticRole::Row)?;
        if let Some(slot) = item_nodes.get_mut(index) {
            *slot = node;
        }
        let row = item_row(ui, node)?;
        let name = ui.row(
            row,
            LayoutSpec {
                width: Length::Fill(5),
                height: Length::Fill(1),
                gap: 7,
                align: Align::Center,
                ..LayoutSpec::default()
            },
        )?;
        item_icon(ui, name, item)?;
        item_label(ui, name, item, 34, Align::Stretch)?;
        table_text(ui, row, item.kind_label, 2, style_class::DEFAULT)?;
        table_text(ui, row, item.size_label, 2, style_class::CAPTION)?;
    }
    Ok(table)
}

fn item_button<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    item: FileBrowserItem,
    height: u16,
    role: SemanticRole,
) -> Result<NodeId, TreeError> {
    let mut button = NodeSpec::new(ComponentKind::Button);
    button.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(height),
        ..LayoutSpec::default()
    };
    button.command = item.command;
    button.role = role;
    button.accessible_name = item.label;
    button.style = style_class::GHOST;
    if item.selected {
        button.state.insert(NodeState::SELECTED);
        button.style = style_class::SUBTLE;
    }
    ui.component(parent, button)
}

fn item_row<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
) -> Result<NodeId, TreeError> {
    ui.row(
        parent,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            padding: Edges::symmetric(8, 5),
            gap: 8,
            align: Align::Center,
            ..LayoutSpec::default()
        },
    )
}

fn item_icon<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    item: FileBrowserItem,
) -> Result<NodeId, TreeError> {
    ui.image(
        parent,
        item.icon,
        ResourceId(0),
        LayoutSpec {
            width: Length::Px(26),
            height: Length::Px(26),
            ..LayoutSpec::default()
        },
    )
}

fn item_label<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    item: FileBrowserItem,
    height: u16,
    align: Align,
) -> Result<NodeId, TreeError> {
    let mut label = NodeSpec::new(if item.editing {
        ComponentKind::TextField
    } else {
        ComponentKind::Text
    });
    label.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(height),
        align,
        ..LayoutSpec::default()
    };
    label.content = Content::Text(item.label);
    label.accessible_name = item.label;
    label.role = if item.editing {
        SemanticRole::TextField
    } else {
        SemanticRole::Text
    };
    if item.editing {
        label.state.insert(NodeState::FOCUSED);
    }
    ui.component(parent, label)
}

fn table_text<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    resource: ResourceId,
    weight: u16,
    style: u16,
) -> Result<NodeId, TreeError> {
    let mut text = NodeSpec::new(ComponentKind::Text);
    text.layout = LayoutSpec {
        width: Length::Fill(weight),
        height: Length::Fill(1),
        ..LayoutSpec::default()
    };
    text.content = Content::Text(resource);
    text.role = SemanticRole::Text;
    text.style = style;
    ui.component(parent, text)
}

fn panel<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    layout: LayoutSpec,
) -> Result<NodeId, TreeError> {
    let mut panel = NodeSpec::new(ComponentKind::Panel);
    panel.layout = layout;
    panel.role = SemanticRole::Group;
    ui.component(parent, panel)
}

fn heading<const N: usize>(
    ui: &mut UiBuilder<'_, N>,
    parent: NodeId,
    resource: ResourceId,
    height: u16,
) -> Result<NodeId, TreeError> {
    let mut text = NodeSpec::new(ComponentKind::Text);
    text.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(height),
        ..LayoutSpec::default()
    };
    text.content = Content::Text(resource);
    text.role = SemanticRole::Heading;
    text.style = style_class::HEADING;
    ui.component(parent, text)
}

const fn visible_scroll() -> ScrollConfig {
    ScrollConfig {
        vertical: ScrollBarPolicy::Always,
        horizontal: ScrollBarPolicy::Hidden,
        bar_layout: ScrollBarLayout::Inset,
        ..ScrollConfig::VERTICAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tree;

    #[test]
    fn file_browser_builds_shared_tree_and_table_components() {
        let mut tree = Tree::<48>::new();
        let root = tree.root();
        let mut ui = UiBuilder::new(&mut tree);
        let navigation = [FileBrowserTreeItem {
            label: ResourceId(1),
            icon: ResourceId(2),
            command: CommandId(3),
            depth: 0,
            selected: true,
            disabled: false,
        }];
        let items = [FileBrowserItem {
            label: ResourceId(4),
            kind_label: ResourceId(5),
            size_label: ResourceId(6),
            icon: ResourceId(7),
            command: CommandId(8),
            selected: false,
            editing: false,
        }];
        let mut nodes = [NodeId::NONE; 1];
        let built = build_file_browser(
            &mut ui,
            root,
            FileBrowserSpec {
                layout: LayoutSpec::fill(),
                navigation_width: 220,
                navigation_heading: ResourceId(9),
                empty_text: ResourceId(10),
                name_heading: ResourceId(11),
                kind_heading: ResourceId(12),
                size_heading: ResourceId(13),
                view: FileBrowserView::Details,
                grid_columns: 4,
            },
            &navigation,
            &items,
            &mut nodes,
        )
        .unwrap();
        assert_eq!(tree.get(built.root).unwrap().kind, ComponentKind::SplitView);
        assert_eq!(
            tree.get(built.navigation).unwrap().kind,
            ComponentKind::TreeView
        );
        assert_eq!(
            tree.get(built.collection).unwrap().kind,
            ComponentKind::TableView
        );
        assert!(!nodes[0].is_none());
    }
}

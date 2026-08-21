//! Отдельное accessibility-дерево, не зависящее от визуального backend.

use rustos_video::Rect;

use crate::{NodeId, NodeState, ResourceId, Tree};

/// Семантическая роль компонента.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SemanticRole {
    /// Чисто декоративный узел.
    #[default]
    None = 0,
    /// Логическая группа.
    Group = 1,
    /// Текст.
    Text = 2,
    /// Заголовок.
    Heading = 3,
    /// Кнопка.
    Button = 4,
    /// Checkbox.
    CheckBox = 5,
    /// Radio button.
    RadioButton = 6,
    /// Switch.
    Switch = 7,
    /// Поле ввода.
    TextField = 8,
    /// Список.
    List = 9,
    /// Элемент списка.
    ListItem = 10,
    /// Menu.
    Menu = 11,
    /// Menu item.
    MenuItem = 12,
    /// Dialog.
    Dialog = 13,
    /// Progress indicator.
    Progress = 14,
    /// Изображение со смыслом.
    Image = 15,
    /// Полоса прокрутки.
    ScrollBar = 16,
    /// Многострочный редактор текста.
    TextArea = 17,
    /// Ползунок числового значения.
    Slider = 18,
    /// Select/ComboBox.
    ComboBox = 19,
    /// Вкладка набора страниц.
    Tab = 20,
    /// Иерархическое дерево.
    Tree = 21,
    /// Узел дерева.
    TreeItem = 22,
    /// Таблица.
    Table = 23,
    /// Строка таблицы.
    Row = 24,
    /// Сетка.
    Grid = 25,
    /// Ячейка/элемент сетки.
    GridCell = 26,
    /// Панель команд.
    Toolbar = 27,
    /// Строка состояния.
    Status = 28,
}

/// Доступные assistive-действия.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SemanticAction(pub u16);

impl SemanticAction {
    /// Активировать.
    pub const ACTIVATE: Self = Self(1 << 0);
    /// Переключить checked.
    pub const TOGGLE: Self = Self(1 << 1);
    /// Установить значение.
    pub const SET_VALUE: Self = Self(1 << 2);
    /// Прокрутить вперёд.
    pub const SCROLL_FORWARD: Self = Self(1 << 3);
    /// Прокрутить назад.
    pub const SCROLL_BACKWARD: Self = Self(1 << 4);
    /// Получить фокус.
    pub const FOCUS: Self = Self(1 << 5);
}

/// Снимок семантики одного видимого узла.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticNode {
    /// Визуальный component ID.
    pub source: NodeId,
    /// Родитель в component tree.
    pub parent: NodeId,
    /// Роль.
    pub role: SemanticRole,
    /// Локализованное accessible name.
    pub name: ResourceId,
    /// Границы для magnifier/assistive hit testing.
    pub rect: Rect,
    /// Состояния focused/checked/disabled/invalid.
    pub state: NodeState,
    /// Разрешённые действия.
    pub actions: SemanticAction,
}

impl SemanticNode {
    const EMPTY: Self = Self {
        source: NodeId::NONE,
        parent: NodeId::NONE,
        role: SemanticRole::None,
        name: ResourceId(0),
        rect: Rect::EMPTY,
        state: NodeState(0),
        actions: SemanticAction(0),
    };
}

/// Bounded accessibility snapshot. Он может сериализоваться отдельному
/// screen-reader service без предоставления доступа к surface приложения.
pub struct SemanticsTree<const N: usize> {
    nodes: [SemanticNode; N],
    len: usize,
}

impl<const N: usize> SemanticsTree<N> {
    /// Пустой snapshot.
    pub const fn new() -> Self {
        Self {
            nodes: [SemanticNode::EMPTY; N],
            len: 0,
        }
    }

    /// Инициализирует semantic storage на месте.
    ///
    /// # Safety
    /// `destination` указывает на валидное неинициализированное хранилище
    /// `SemanticsTree<N>`.
    pub(crate) unsafe fn initialize_in_place(destination: *mut Self) {
        // SAFETY: вызывающий предоставил storage всего объекта.
        let nodes = unsafe { core::ptr::addr_of_mut!((*destination).nodes).cast::<SemanticNode>() };
        for index in 0..N {
            // SAFETY: каждый slot массива пишется ровно один раз.
            unsafe { nodes.add(index).write(SemanticNode::EMPTY) };
        }
        // SAFETY: scalar field принадлежит тому же storage.
        unsafe { core::ptr::addr_of_mut!((*destination).len).write(0) };
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    /// Перестраивает только компактные semantic records.
    pub fn rebuild(&mut self, tree: &Tree<N>) {
        self.len = 0;
        for id in tree.ids() {
            let node = tree.get(id).expect("iterator yields live nodes");
            if node.role == SemanticRole::None
                || node.state.contains(NodeState::HIDDEN)
                || self.len == N
            {
                continue;
            }
            self.nodes[self.len] = SemanticNode {
                source: id,
                parent: node.parent,
                role: node.role,
                name: node.accessible_name,
                rect: node.rect,
                state: node.state,
                actions: actions_for(node.role, node.state),
            };
            self.len += 1;
        }
    }

    /// Готовые semantic records.
    pub fn as_slice(&self) -> &[SemanticNode] {
        &self.nodes[..self.len]
    }

    /// Число semantic records.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Проверка пустоты.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for SemanticsTree<N> {
    fn default() -> Self {
        Self::new()
    }
}

fn actions_for(role: SemanticRole, state: NodeState) -> SemanticAction {
    if state.contains(NodeState::DISABLED) {
        return SemanticAction(0);
    }
    let mut actions = SemanticAction(SemanticAction::FOCUS.0);
    match role {
        SemanticRole::Button | SemanticRole::MenuItem => actions.0 |= SemanticAction::ACTIVATE.0,
        SemanticRole::CheckBox | SemanticRole::RadioButton | SemanticRole::Switch => {
            actions.0 |= SemanticAction::TOGGLE.0
        }
        SemanticRole::TextField
        | SemanticRole::TextArea
        | SemanticRole::Slider
        | SemanticRole::ComboBox => actions.0 |= SemanticAction::SET_VALUE.0,
        SemanticRole::Tab => actions.0 |= SemanticAction::ACTIVATE.0,
        SemanticRole::List
        | SemanticRole::Tree
        | SemanticRole::Table
        | SemanticRole::Grid
        | SemanticRole::ScrollBar => {
            actions.0 |= SemanticAction::SCROLL_FORWARD.0 | SemanticAction::SCROLL_BACKWARD.0
        }
        _ => {}
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ComponentKind, NodeSpec};

    #[test]
    fn disabled_control_exposes_no_actions() {
        let mut tree = Tree::<4>::new();
        let mut spec = NodeSpec::new(ComponentKind::Button);
        spec.role = SemanticRole::Button;
        spec.state = NodeState::DISABLED;
        tree.create(tree.root(), spec).unwrap();
        let mut semantics = SemanticsTree::new();
        semantics.rebuild(&tree);
        assert_eq!(semantics.as_slice()[0].actions, SemanticAction(0));
    }
}

//! Bounded component tree with stable generation-checked identities.

use rustos_video::Rect;

use crate::{collections::ListViewState, LayoutSpec, ScrollConfig, ScrollState, SemanticRole};

/// Непрозрачный идентификатор узла: старшие 16 бит — generation, младшие —
/// индекс. Удалённый ID не начинает указывать на новый компонент того же slot.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NodeId(pub u32);

impl NodeId {
    /// Отсутствующий узел.
    pub const NONE: Self = Self(u32::MAX);

    pub(crate) const fn from_parts(index: u16, generation: u16) -> Self {
        Self(((generation as u32) << 16) | index as u32)
    }

    pub(crate) const fn index(self) -> usize {
        (self.0 & 0xffff) as usize
    }

    pub(crate) const fn generation(self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// Проверяет специальное значение отсутствующего узла.
    pub const fn is_none(self) -> bool {
        self.0 == Self::NONE.0
    }
}

/// ID строки, изображения или иконки в ресурсах RUNE package.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceId(pub u32);

/// ID общесистемной команды приложения.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommandId(pub u32);

/// Тип визуального/логического компонента.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ComponentKind {
    /// Корень UI-сессии.
    #[default]
    Root = 0,
    /// Универсальная панель.
    Panel = 1,
    /// Горизонтальный контейнер.
    Row = 2,
    /// Вертикальный контейнер.
    Column = 3,
    /// Наложение дочерних элементов.
    Stack = 4,
    /// Равномерная сетка.
    Grid = 5,
    /// Текст.
    Text = 6,
    /// Растровый ресурс.
    Image = 7,
    /// Иконка.
    Icon = 8,
    /// Кнопка-команда.
    Button = 9,
    /// Checkbox.
    CheckBox = 10,
    /// Radio button.
    RadioButton = 11,
    /// Переключатель.
    Switch = 12,
    /// Однострочное поле ввода.
    TextField = 13,
    /// Многострочный текст.
    TextArea = 14,
    /// Ползунок.
    Slider = 15,
    /// Select/ComboBox.
    Select = 16,
    /// Прокручиваемая область.
    ScrollView = 17,
    /// Виртуализируемый список.
    ListView = 18,
    /// Разделитель.
    Divider = 19,
    /// Линейный прогресс.
    ProgressBar = 20,
    /// Вкладки.
    TabView = 21,
    /// Menu/popup surface.
    Menu = 22,
    /// Dialog overlay.
    Dialog = 23,
    /// Самостоятельная полоса прокрутки, связанная с внешней моделью.
    ScrollBar = 24,
    /// Иерархическое дерево с вертикальной прокруткой.
    TreeView = 25,
    /// Таблица с фиксированными строками и вертикальной прокруткой.
    TableView = 26,
    /// Сетка элементов. Для прокрутки помещается внутрь ScrollView.
    GridView = 27,
    /// Панель команд приложения.
    Toolbar = 28,
    /// Нижняя строка состояния.
    StatusBar = 29,
    /// Две изменяемые панели в одной строке.
    SplitView = 30,
}

impl ComponentKind {
    /// Может ли компонент получить клавиатурный фокус по умолчанию.
    pub const fn focusable(self) -> bool {
        matches!(
            self,
            Self::Button
                | Self::CheckBox
                | Self::RadioButton
                | Self::Switch
                | Self::TextField
                | Self::TextArea
                | Self::Slider
                | Self::Select
                | Self::ScrollBar
                | Self::ListView
                | Self::TabView
                | Self::Menu
                | Self::TreeView
                | Self::TableView
                | Self::GridView
        )
    }
}

/// Небольшой payload компонента. Строки и изображения лежат в package
/// resources; дерево хранит только ID и не копирует приватные данные в cache.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Content {
    /// Нет содержимого.
    #[default]
    None,
    /// Текстовый ресурс.
    Text(ResourceId),
    /// Графический ресурс.
    Resource(ResourceId),
    /// Число в диапазоне 0..=1000 (progress/slider).
    Value(u16),
}

/// Состояния компонента, независимо комбинируемые системой стилей.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NodeState(pub u16);

impl NodeState {
    /// Указатель над компонентом.
    pub const HOVERED: Self = Self(1 << 0);
    /// Основная кнопка указателя нажата.
    pub const PRESSED: Self = Self(1 << 1);
    /// Клавиатурный фокус.
    pub const FOCUSED: Self = Self(1 << 2);
    /// Выбранный элемент коллекции.
    pub const SELECTED: Self = Self(1 << 3);
    /// Отмеченный checkbox/switch.
    pub const CHECKED: Self = Self(1 << 4);
    /// Компонент недоступен.
    pub const DISABLED: Self = Self(1 << 5);
    /// Асинхронная операция выполняется.
    pub const LOADING: Self = Self(1 << 6);
    /// Значение не прошло validation.
    pub const INVALID: Self = Self(1 << 7);
    /// Значение доступно только для чтения.
    pub const READ_ONLY: Self = Self(1 << 8);
    /// Focus получен клавиатурой и должен иметь видимый focus ring.
    pub const FOCUS_VISIBLE: Self = Self(1 << 9);
    /// Узел исключён из layout, hit-test и accessibility.
    pub const HIDDEN: Self = Self(1 << 10);

    /// Проверяет все указанные биты.
    pub const fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }

    /// Добавляет флаги.
    pub fn insert(&mut self, flags: Self) {
        self.0 |= flags.0;
    }

    /// Удаляет флаги.
    pub fn remove(&mut self, flags: Self) {
        self.0 &= !flags.0;
    }
}

/// Причины инкрементального обновления узла.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DirtyFlags(pub u8);

impl DirtyFlags {
    /// Нужен layout.
    pub const LAYOUT: Self = Self(1 << 0);
    /// Нужно перестроить визуальные команды.
    pub const PAINT: Self = Self(1 << 1);
    /// Нужно обновить семантическое дерево.
    pub const SEMANTICS: Self = Self(1 << 2);
    /// Все стадии.
    pub const ALL: Self = Self(Self::LAYOUT.0 | Self::PAINT.0 | Self::SEMANTICS.0);

    /// Проверяет наличие флага.
    pub const fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }

    /// Добавляет флаги.
    pub fn insert(&mut self, flags: Self) {
        self.0 |= flags.0;
    }

    /// Удаляет флаги.
    pub fn remove(&mut self, flags: Self) {
        self.0 &= !flags.0;
    }
}

/// Типизированное описание нового узла. Rust builder и `.rui` decoder
/// создают именно `NodeSpec`, поэтому не образуют две разные UI-системы.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeSpec {
    /// Тип компонента.
    pub kind: ComponentKind,
    /// Layout.
    pub layout: LayoutSpec,
    /// ID style class в schema темы.
    pub style: u16,
    /// Первоначальное состояние.
    pub state: NodeState,
    /// Ресурс/значение.
    pub content: Content,
    /// Команда при активации.
    pub command: CommandId,
    /// Семантическая роль.
    pub role: SemanticRole,
    /// Доступное имя в resource table.
    pub accessible_name: ResourceId,
    /// Явный Tab-order; отрицательное значение исключает из Tab traversal.
    pub tab_index: i16,
    /// Политика прокрутки. Обычные controls получают `ScrollConfig::NONE`.
    pub scroll: ScrollConfig,
}

impl NodeSpec {
    /// Описание одного компонента с разумными defaults.
    pub const fn new(kind: ComponentKind) -> Self {
        Self {
            kind,
            layout: LayoutSpec::fill(),
            style: 0,
            state: NodeState(0),
            content: Content::None,
            command: CommandId(0),
            role: SemanticRole::None,
            accessible_name: ResourceId(0),
            tab_index: if kind.focusable() { 0 } else { -1 },
            scroll: if matches!(
                kind,
                ComponentKind::ScrollView
                    | ComponentKind::ListView
                    | ComponentKind::TreeView
                    | ComponentKind::TableView
            ) {
                ScrollConfig::VERTICAL
            } else {
                ScrollConfig::NONE
            },
        }
    }
}

impl Default for NodeSpec {
    fn default() -> Self {
        Self::new(ComponentKind::Root)
    }
}

/// Один узел runtime. Поля дерева закрыты от приложений и меняются через API,
/// чтобы runtime не потерял invalidation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Node {
    pub(crate) generation: u16,
    pub(crate) live: bool,
    /// Тип компонента.
    pub kind: ComponentKind,
    pub(crate) parent: NodeId,
    pub(crate) first_child: NodeId,
    pub(crate) next_sibling: NodeId,
    /// Layout-свойства.
    pub layout: LayoutSpec,
    /// Вычисленные границы.
    pub rect: Rect,
    /// ID style class.
    pub style: u16,
    /// Текущее интерактивное состояние.
    pub state: NodeState,
    /// Содержимое.
    pub content: Content,
    /// Связанная команда.
    pub command: CommandId,
    /// Семантическая роль.
    pub role: SemanticRole,
    /// Accessible name resource.
    pub accessible_name: ResourceId,
    /// Порядок Tab.
    pub tab_index: i16,
    /// Прокрутка принадлежит компоненту, а не визуальной полосе.
    pub scroll: ScrollState,
    /// Logical list state переживает recycling видимых delegates.
    pub(crate) list: ListViewState,
    /// Source модели standalone ScrollBar; `NONE` для встроенной полосы.
    pub(crate) scroll_target: NodeId,
    /// Ось standalone ScrollBar.
    pub(crate) scroll_axis: crate::ScrollAxis,
    pub(crate) dirty: DirtyFlags,
}

impl Node {
    const EMPTY: Self = Self {
        generation: 0,
        live: false,
        kind: ComponentKind::Root,
        parent: NodeId::NONE,
        first_child: NodeId::NONE,
        next_sibling: NodeId::NONE,
        layout: LayoutSpec::fill(),
        rect: Rect::EMPTY,
        style: 0,
        state: NodeState(0),
        content: Content::None,
        command: CommandId(0),
        role: SemanticRole::None,
        accessible_name: ResourceId(0),
        tab_index: -1,
        scroll: ScrollState::disabled(),
        list: ListViewState::disabled(),
        scroll_target: NodeId::NONE,
        scroll_axis: crate::ScrollAxis::Vertical,
        dirty: DirtyFlags::ALL,
    };

    fn from_spec(generation: u16, spec: NodeSpec) -> Self {
        Self {
            generation,
            live: true,
            kind: spec.kind,
            parent: NodeId::NONE,
            first_child: NodeId::NONE,
            next_sibling: NodeId::NONE,
            layout: spec.layout,
            rect: Rect::EMPTY,
            style: spec.style,
            state: spec.state,
            content: spec.content,
            command: spec.command,
            role: spec.role,
            accessible_name: spec.accessible_name,
            tab_index: spec.tab_index,
            scroll: ScrollState::new(spec.scroll),
            list: ListViewState::disabled(),
            scroll_target: NodeId::NONE,
            scroll_axis: crate::ScrollAxis::Vertical,
            dirty: DirtyFlags::ALL,
        }
    }
}

/// Ошибки component tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeError {
    /// Достигнут заданный владельцем лимит узлов.
    Capacity,
    /// ID отсутствует, устарел или относится к другому tree.
    InvalidNode,
    /// Root нельзя удалить или добавить самому себе.
    InvalidHierarchy,
    /// Операция неприменима к типу компонента.
    InvalidComponent,
}

/// Дерево фиксированной ёмкости.
pub struct Tree<const N: usize> {
    nodes: [Node; N],
    len: usize,
    root: NodeId,
}

impl<const N: usize> Tree<N> {
    /// Создаёт дерево с единственным Root. `N` обязан быть 1..=65535.
    pub fn new() -> Self {
        assert!(N > 0 && N <= u16::MAX as usize);
        let mut nodes = [Node::EMPTY; N];
        nodes[0] = Node::from_spec(1, NodeSpec::new(ComponentKind::Root));
        let root = NodeId::from_parts(0, 1);
        Self {
            nodes,
            len: 1,
            root,
        }
    }

    /// Инициализирует большое дерево прямо по адресу владельца.
    ///
    /// # Safety
    /// `destination` должен быть выровнен, доступен для записи и указывать на
    /// неинициализированное хранилище размером `Tree<N>`.
    pub(crate) unsafe fn initialize_in_place(destination: *mut Self) {
        assert!(N > 0 && N <= u16::MAX as usize);
        // SAFETY: вызывающий предоставил storage всего Tree; массив nodes
        // занимает его первый именованный field и заполняется поэлементно.
        let nodes = unsafe { core::ptr::addr_of_mut!((*destination).nodes).cast::<Node>() };
        for index in 0..N {
            // SAFETY: index ограничен ёмкостью массива, каждый slot пишется
            // ровно один раз до публикации объекта.
            unsafe { nodes.add(index).write(Node::EMPTY) };
        }
        // SAFETY: первый slot существует, потому что N > 0.
        unsafe {
            nodes.write(Node::from_spec(1, NodeSpec::new(ComponentKind::Root)));
            core::ptr::addr_of_mut!((*destination).len).write(1);
            core::ptr::addr_of_mut!((*destination).root).write(NodeId::from_parts(0, 1));
        }
    }

    /// Очищает дерево на месте, сохраняя storage и поколения NodeId.
    /// Нужен большим runtime-ам: пересоздание всего `[Node; N]` временным
    /// значением способно необоснованно занять сотни KiB kernel stack.
    pub(crate) fn reset(&mut self) {
        for node in &mut self.nodes {
            let generation = node.generation;
            *node = Node::EMPTY;
            node.generation = generation;
        }
        let generation = self.nodes[0].generation.wrapping_add(1).max(1);
        self.nodes[0] = Node::from_spec(generation, NodeSpec::new(ComponentKind::Root));
        self.len = 1;
        self.root = NodeId::from_parts(0, generation);
    }

    /// Корень дерева.
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Число живых узлов.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Проверяет отсутствие пользовательских компонентов.
    pub const fn is_empty(&self) -> bool {
        self.len == 1
    }

    /// Возвращает узел по generation-checked ID.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        let node = self.nodes.get(id.index())?;
        (node.live && node.generation == id.generation()).then_some(node)
    }

    pub(crate) fn get_mut_internal(&mut self, id: NodeId) -> Option<&mut Node> {
        let node = self.nodes.get_mut(id.index())?;
        (node.live && node.generation == id.generation()).then_some(node)
    }

    /// Создаёт дочерний компонент.
    pub fn create(&mut self, parent: NodeId, spec: NodeSpec) -> Result<NodeId, TreeError> {
        if self.get(parent).is_none() {
            return Err(TreeError::InvalidNode);
        }
        let index = self
            .nodes
            .iter()
            .position(|node| !node.live)
            .ok_or(TreeError::Capacity)?;
        let generation = self.nodes[index].generation.wrapping_add(1).max(1);
        self.nodes[index] = Node::from_spec(generation, spec);
        let id = NodeId::from_parts(index as u16, generation);
        self.nodes[index].parent = parent;
        if self.nodes[parent.index()].first_child.is_none() {
            self.nodes[parent.index()].first_child = id;
        } else {
            let mut tail = self.nodes[parent.index()].first_child;
            loop {
                let next = self.nodes[tail.index()].next_sibling;
                if next.is_none() {
                    self.nodes[tail.index()].next_sibling = id;
                    break;
                }
                tail = next;
            }
        }
        self.len += 1;
        self.mark_ancestors(parent, DirtyFlags::LAYOUT);
        Ok(id)
    }

    /// Удаляет subtree. Старые NodeId становятся невалидными.
    pub fn remove(&mut self, id: NodeId) -> Result<(), TreeError> {
        if id == self.root {
            return Err(TreeError::InvalidHierarchy);
        }
        let parent = self.get(id).ok_or(TreeError::InvalidNode)?.parent;
        self.detach(parent, id);
        self.remove_subtree(id);
        self.mark_ancestors(parent, DirtyFlags::LAYOUT);
        Ok(())
    }

    /// Меняет layout и корректно поднимает invalidation к контейнерам.
    pub fn set_layout(&mut self, id: NodeId, layout: LayoutSpec) -> Result<(), TreeError> {
        let parent = {
            let node = self.get_mut_internal(id).ok_or(TreeError::InvalidNode)?;
            if node.layout == layout {
                return Ok(());
            }
            node.layout = layout;
            node.dirty.insert(DirtyFlags::LAYOUT);
            node.parent
        };
        self.mark_ancestors(parent, DirtyFlags::LAYOUT);
        Ok(())
    }

    /// Меняет интерактивное состояние. Layout не пересчитывается.
    pub fn set_state(&mut self, id: NodeId, state: NodeState) -> Result<Rect, TreeError> {
        let node = self.get_mut_internal(id).ok_or(TreeError::InvalidNode)?;
        if node.state != state {
            node.state = state;
            node.dirty.insert(DirtyFlags::PAINT);
            node.dirty.insert(DirtyFlags::SEMANTICS);
        }
        Ok(node.rect)
    }

    /// Меняет ресурс/значение и помечает только соответствующий узел.
    pub fn set_content(&mut self, id: NodeId, content: Content) -> Result<Rect, TreeError> {
        let node = self.get_mut_internal(id).ok_or(TreeError::InvalidNode)?;
        if node.content != content {
            node.content = content;
            node.dirty.insert(DirtyFlags::PAINT);
            node.dirty.insert(DirtyFlags::SEMANTICS);
        }
        Ok(node.rect)
    }

    /// Меняет scroll policy и сбрасывает offset отключённых осей.
    pub fn set_scroll_config(
        &mut self,
        id: NodeId,
        config: ScrollConfig,
    ) -> Result<Rect, TreeError> {
        let node = self.get_mut_internal(id).ok_or(TreeError::InvalidNode)?;
        if node.scroll.config == config {
            return Ok(node.rect);
        }
        node.scroll.config = config;
        if config.horizontal == crate::ScrollBarPolicy::Hidden {
            node.scroll.horizontal.scroll_to(0);
        }
        if config.vertical == crate::ScrollBarPolicy::Hidden {
            node.scroll.vertical.scroll_to(0);
        }
        node.dirty.insert(DirtyFlags::LAYOUT);
        node.dirty.insert(DirtyFlags::PAINT);
        Ok(node.rect)
    }

    /// Настраивает collection source ListView. Metadata имеет O(1) размер и
    /// не создаёт визуальный узел на каждый item.
    pub fn configure_list_view(
        &mut self,
        id: NodeId,
        item_count: u32,
        item_extent: u32,
        selection: crate::SelectionMode,
    ) -> Result<Rect, TreeError> {
        let node = self.get_mut_internal(id).ok_or(TreeError::InvalidNode)?;
        if node.kind != ComponentKind::ListView {
            return Err(TreeError::InvalidComponent);
        }
        node.list.configure(item_count, item_extent, selection);
        node.scroll
            .vertical
            .set_extents(node.rect.height, node.list.content_extent());
        node.dirty.insert(DirtyFlags::LAYOUT);
        node.dirty.insert(DirtyFlags::PAINT);
        node.dirty.insert(DirtyFlags::SEMANTICS);
        Ok(node.rect)
    }

    /// Связывает самостоятельный ScrollBar с моделью другого scrollable
    /// компонента. Владелец модели остаётся target.
    pub fn bind_scroll_bar(
        &mut self,
        bar: NodeId,
        target: NodeId,
        axis: crate::ScrollAxis,
    ) -> Result<Rect, TreeError> {
        if self.get(target).is_none() {
            return Err(TreeError::InvalidNode);
        }
        let node = self.get_mut_internal(bar).ok_or(TreeError::InvalidNode)?;
        if node.kind != ComponentKind::ScrollBar {
            return Err(TreeError::InvalidComponent);
        }
        node.scroll_target = target;
        node.scroll_axis = axis;
        node.dirty.insert(DirtyFlags::PAINT);
        node.dirty.insert(DirtyFlags::SEMANTICS);
        Ok(node.rect)
    }

    pub(crate) fn set_scroll_extents_internal(
        &mut self,
        id: NodeId,
        viewport_width: u32,
        content_width: u64,
        viewport_height: u32,
        content_height: u64,
    ) {
        let Some(node) = self.get_mut_internal(id) else {
            return;
        };
        node.scroll
            .horizontal
            .set_extents(viewport_width, content_width);
        let vertical_content = if node.list.is_configured() {
            node.list.content_extent().max(content_height)
        } else {
            content_height
        };
        node.scroll
            .vertical
            .set_extents(viewport_height, vertical_content);
    }

    /// Clip узла учитывает все scrollable-предки, но не обрезает рамку
    /// самого ScrollView.
    pub(crate) fn paint_clip(&self, id: NodeId) -> Rect {
        let Some(node) = self.get(id) else {
            return Rect::EMPTY;
        };
        let mut clip = self.get(self.root).map_or(Rect::EMPTY, |root| root.rect);
        let mut parent = node.parent;
        while let Some(ancestor) = self.get(parent) {
            if matches!(
                ancestor.kind,
                ComponentKind::ScrollView | ComponentKind::ListView
            ) {
                clip = clip.intersection(ancestor.rect);
            }
            parent = ancestor.parent;
        }
        clip
    }

    /// Первый дочерний элемент.
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.get(id)
            .and_then(|node| (!node.first_child.is_none()).then_some(node.first_child))
    }

    /// Следующий сосед.
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.get(id)
            .and_then(|node| (!node.next_sibling.is_none()).then_some(node.next_sibling))
    }

    /// Число непосредственных детей.
    pub fn child_count(&self, parent: NodeId) -> usize {
        let mut count = 0;
        let mut child = self.first_child(parent);
        while let Some(id) = child {
            count += 1;
            child = self.next_sibling(id);
        }
        count
    }

    /// Итератор живых IDs в стабильном paint order.
    pub fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.iter().enumerate().filter_map(|(index, node)| {
            node.live
                .then_some(NodeId::from_parts(index as u16, node.generation))
        })
    }

    pub(crate) fn has_dirty(&self, flags: DirtyFlags) -> bool {
        self.nodes
            .iter()
            .any(|node| node.live && node.dirty.contains(flags))
    }

    pub(crate) fn clear_layout_dirty(&mut self) {
        for node in &mut self.nodes {
            node.dirty.remove(DirtyFlags::LAYOUT);
        }
    }

    pub(crate) fn clear_paint_dirty(&mut self) {
        for node in &mut self.nodes {
            node.dirty.remove(DirtyFlags::PAINT);
        }
    }

    pub(crate) fn clear_semantics_dirty(&mut self) {
        for node in &mut self.nodes {
            node.dirty.remove(DirtyFlags::SEMANTICS);
        }
    }

    fn mark_ancestors(&mut self, mut id: NodeId, flags: DirtyFlags) {
        while !id.is_none() {
            let Some(node) = self.get_mut_internal(id) else {
                break;
            };
            node.dirty.insert(flags);
            id = node.parent;
        }
    }

    fn detach(&mut self, parent: NodeId, target: NodeId) {
        let mut previous = NodeId::NONE;
        let mut current = self.nodes[parent.index()].first_child;
        while !current.is_none() {
            if current == target {
                let next = self.nodes[current.index()].next_sibling;
                if previous.is_none() {
                    self.nodes[parent.index()].first_child = next;
                } else {
                    self.nodes[previous.index()].next_sibling = next;
                }
                return;
            }
            previous = current;
            current = self.nodes[current.index()].next_sibling;
        }
    }

    fn remove_subtree(&mut self, id: NodeId) {
        let mut child = self.nodes[id.index()].first_child;
        while !child.is_none() {
            let next = self.nodes[child.index()].next_sibling;
            self.remove_subtree(child);
            child = next;
        }
        let generation = self.nodes[id.index()].generation;
        self.nodes[id.index()] = Node::EMPTY;
        self.nodes[id.index()].generation = generation;
        self.len -= 1;
    }
}

impl<const N: usize> Default for Tree<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_identity_never_aliases_reused_slot() {
        let mut tree = Tree::<4>::new();
        let old = tree
            .create(tree.root(), NodeSpec::new(ComponentKind::Button))
            .unwrap();
        tree.remove(old).unwrap();
        let new = tree
            .create(tree.root(), NodeSpec::new(ComponentKind::Text))
            .unwrap();
        assert_ne!(old, new);
        assert!(tree.get(old).is_none());
        assert_eq!(tree.get(new).unwrap().kind, ComponentKind::Text);
    }

    #[test]
    fn capacity_failure_keeps_tree_intact() {
        let mut tree = Tree::<2>::new();
        tree.create(tree.root(), NodeSpec::new(ComponentKind::Panel))
            .unwrap();
        assert_eq!(
            tree.create(tree.root(), NodeSpec::new(ComponentKind::Panel)),
            Err(TreeError::Capacity)
        );
        assert_eq!(tree.len(), 2);
    }
}

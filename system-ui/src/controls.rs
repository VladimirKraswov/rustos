//! Типизированный Rust builder стандартных компонентов.

use crate::{
    CommandId, ComponentKind, Content, LayoutSpec, NodeId, NodeSpec, ResourceId, SemanticRole,
    Tree, TreeError,
};

/// Удобный façade над единым component tree. Builder ничего не рисует и не
/// создаёт параллельную object model: его результат идентичен загруженному IR.
pub struct UiBuilder<'a, const N: usize> {
    tree: &'a mut Tree<N>,
}

impl<'a, const N: usize> UiBuilder<'a, N> {
    /// Создаёт builder для существующего runtime tree.
    pub fn new(tree: &'a mut Tree<N>) -> Self {
        Self { tree }
    }

    /// Корневой узел.
    pub const fn root(&self) -> NodeId {
        self.tree.root()
    }

    /// Низкоуровневое создание из полного типизированного описания.
    pub fn component(&mut self, parent: NodeId, spec: NodeSpec) -> Result<NodeId, TreeError> {
        self.tree.create(parent, spec)
    }

    /// Контейнер и его дочернее содержимое. Ошибка дочернего builder не
    /// скрывается; caller может удалить неполный subtree или прекратить setup.
    pub fn container<F>(
        &mut self,
        parent: NodeId,
        spec: NodeSpec,
        children: F,
    ) -> Result<NodeId, TreeError>
    where
        F: FnOnce(&mut Self, NodeId) -> Result<(), TreeError>,
    {
        let id = self.component(parent, spec)?;
        children(self, id)?;
        Ok(id)
    }

    /// Panel.
    pub fn panel(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        self.simple(parent, ComponentKind::Panel, layout)
    }

    /// Row.
    pub fn row(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        self.simple(parent, ComponentKind::Row, layout)
    }

    /// Column.
    pub fn column(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        self.simple(parent, ComponentKind::Column, layout)
    }

    /// Stack.
    pub fn stack(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        self.simple(parent, ComponentKind::Stack, layout)
    }

    /// Grid.
    pub fn grid(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        self.simple(parent, ComponentKind::Grid, layout)
    }

    /// Горизонтальный SplitView. Ширины дочерних панелей задаются обычными
    /// `Length`, поэтому composite не знает о конкретном приложении.
    pub fn split_view(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::SplitView);
        spec.layout = layout;
        spec.role = SemanticRole::Group;
        self.component(parent, spec)
    }

    /// Toolbar — семантическая строка связанных команд.
    pub fn toolbar(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Toolbar);
        spec.layout = layout;
        spec.role = SemanticRole::Toolbar;
        spec.tab_index = -1;
        self.component(parent, spec)
    }

    /// StatusBar — нижняя строка краткого состояния документа/представления.
    pub fn status_bar(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::StatusBar);
        spec.layout = layout;
        spec.role = SemanticRole::Status;
        spec.tab_index = -1;
        self.component(parent, spec)
    }

    /// Text/Label.
    pub fn text(
        &mut self,
        parent: NodeId,
        resource: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Text);
        spec.layout = layout;
        spec.content = Content::Text(resource);
        spec.role = SemanticRole::Text;
        self.component(parent, spec)
    }

    /// Image из таблицы ресурсов приложения или системного resource pack.
    /// Декоративное изображение передаёт `ResourceId(0)` как accessible name;
    /// смысловая иллюстрация получает роль Image и локализованное имя.
    pub fn image(
        &mut self,
        parent: NodeId,
        resource: ResourceId,
        accessible_name: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Image);
        spec.layout = layout;
        spec.content = Content::Resource(resource);
        spec.accessible_name = accessible_name;
        if accessible_name != ResourceId(0) {
            spec.role = SemanticRole::Image;
        }
        self.component(parent, spec)
    }

    /// Icon из системного или прикладного resource pack. В отличие от Image,
    /// renderer может перекрашивать монохромную пиктограмму цветом темы.
    pub fn icon(
        &mut self,
        parent: NodeId,
        resource: ResourceId,
        accessible_name: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Icon);
        spec.layout = layout;
        spec.content = Content::Resource(resource);
        spec.accessible_name = accessible_name;
        if accessible_name != ResourceId(0) {
            spec.role = SemanticRole::Image;
        }
        self.component(parent, spec)
    }

    /// Всплывающая Menu surface. Пункты остаются обычными Button-компонентами
    /// с `SemanticRole::MenuItem`, поэтому мышь, Tab и Enter используют один
    /// dispatcher, а не отдельный набор координат shell'а.
    pub fn menu(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Menu);
        spec.layout = layout;
        spec.role = SemanticRole::Menu;
        // Menu является focus scope, а не отдельным пунктом Tab-порядка.
        // Фокус сразу переходит на первый дочерний control.
        spec.tab_index = -1;
        self.component(parent, spec)
    }

    /// Button, привязанный к объекту команды.
    pub fn button(
        &mut self,
        parent: NodeId,
        label: ResourceId,
        command: CommandId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        self.control(
            parent,
            ComponentKind::Button,
            SemanticRole::Button,
            label,
            command,
            layout,
        )
    }

    /// Пункт Menu с визуальным поведением Button и отдельной семантической
    /// ролью. Это позволяет screen reader отличить action в popup от обычной
    /// кнопки формы, не создавая второй input implementation.
    pub fn menu_item(
        &mut self,
        parent: NodeId,
        label: ResourceId,
        command: CommandId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        self.control(
            parent,
            ComponentKind::Button,
            SemanticRole::MenuItem,
            label,
            command,
            layout,
        )
    }

    /// Checkbox.
    pub fn checkbox(
        &mut self,
        parent: NodeId,
        label: ResourceId,
        command: CommandId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        self.control(
            parent,
            ComponentKind::CheckBox,
            SemanticRole::CheckBox,
            label,
            command,
            layout,
        )
    }

    /// RadioButton. Объединение взаимоисключающих вариантов остаётся
    /// политикой приложения и выражается общей командой/моделью выбора.
    pub fn radio_button(
        &mut self,
        parent: NodeId,
        label: ResourceId,
        command: CommandId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        self.control(
            parent,
            ComponentKind::RadioButton,
            SemanticRole::RadioButton,
            label,
            command,
            layout,
        )
    }

    /// Switch.
    pub fn switch(
        &mut self,
        parent: NodeId,
        label: ResourceId,
        command: CommandId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        self.control(
            parent,
            ComponentKind::Switch,
            SemanticRole::Switch,
            label,
            command,
            layout,
        )
    }

    /// TextField.
    pub fn text_field(
        &mut self,
        parent: NodeId,
        value: ResourceId,
        accessible_name: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::TextField);
        spec.layout = layout;
        spec.content = Content::Text(value);
        spec.role = SemanticRole::TextField;
        spec.accessible_name = accessible_name;
        self.component(parent, spec)
    }

    /// Многострочный TextArea. Текст хранится в resource adapter приложения,
    /// поэтому большой документ не копируется внутрь component tree.
    pub fn text_area(
        &mut self,
        parent: NodeId,
        value: ResourceId,
        accessible_name: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::TextArea);
        spec.layout = layout;
        spec.content = Content::Text(value);
        spec.role = SemanticRole::TextArea;
        spec.accessible_name = accessible_name;
        self.component(parent, spec)
    }

    /// Slider со значением 0..=1000.
    pub fn slider(
        &mut self,
        parent: NodeId,
        value: u16,
        accessible_name: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Slider);
        spec.layout = layout;
        spec.content = Content::Value(value.min(1000));
        spec.role = SemanticRole::Slider;
        spec.accessible_name = accessible_name;
        self.component(parent, spec)
    }

    /// Select/ComboBox. `value` адресует текущую отображаемую строку;
    /// варианты предоставляет bounded model приложения.
    pub fn select(
        &mut self,
        parent: NodeId,
        value: ResourceId,
        accessible_name: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Select);
        spec.layout = layout;
        spec.content = Content::Text(value);
        spec.role = SemanticRole::ComboBox;
        spec.accessible_name = accessible_name;
        self.component(parent, spec)
    }

    /// Универсальный ScrollView. Конфигурация принадлежит viewport и не
    /// зависит от наличия визуальной полосы прокрутки.
    pub fn scroll_view(
        &mut self,
        parent: NodeId,
        config: crate::ScrollConfig,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::ScrollView);
        spec.layout = layout;
        spec.scroll = config;
        spec.role = SemanticRole::Group;
        self.component(parent, spec)
    }

    /// Самостоятельное scrollbar-представление. Приложение связывает его с
    /// `ScrollModel`; полоса не владеет content/offset.
    pub fn scroll_bar(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::ScrollBar);
        spec.layout = layout;
        spec.role = SemanticRole::ScrollBar;
        self.component(parent, spec)
    }

    /// Виртуализируемый ListView container. Item creation должен ограничивать
    /// caller видимым диапазоном, который runtime сообщает через ABI event.
    pub fn list_view(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::ListView);
        spec.layout = layout;
        spec.role = SemanticRole::List;
        self.component(parent, spec)
    }

    /// TreeView с общей scroll-моделью и accessibility-ролью дерева.
    pub fn tree_view(
        &mut self,
        parent: NodeId,
        config: crate::ScrollConfig,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::TreeView);
        spec.layout = layout;
        spec.scroll = config;
        spec.role = SemanticRole::Tree;
        self.component(parent, spec)
    }

    /// Один интерактивный узел TreeView. `depth` преобразуется в indentation;
    /// файловые пути и expand-policy остаются у model приложения.
    pub fn tree_item(
        &mut self,
        parent: NodeId,
        label: ResourceId,
        command: CommandId,
        depth: u8,
        selected: bool,
        mut layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        layout.padding.left = layout
            .padding
            .left
            .saturating_add(u16::from(depth).saturating_mul(18));
        let mut spec = NodeSpec::new(ComponentKind::Button);
        spec.layout = layout;
        spec.content = Content::Text(label);
        spec.command = command;
        spec.role = SemanticRole::TreeItem;
        spec.accessible_name = label;
        if selected {
            spec.state.insert(crate::NodeState::SELECTED);
        }
        self.component(parent, spec)
    }

    /// TableView с фиксированными строками. Колонки являются Row-дочерними
    /// компонентами и используют обычные Fill weights.
    pub fn table_view(
        &mut self,
        parent: NodeId,
        config: crate::ScrollConfig,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::TableView);
        spec.layout = layout;
        spec.scroll = config;
        spec.role = SemanticRole::Table;
        self.component(parent, spec)
    }

    /// GridView layout. Для больших наборов composite помещает его внутрь
    /// ScrollView и задаёт измеренную высоту строк.
    pub fn grid_view(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::GridView);
        spec.layout = layout;
        spec.role = SemanticRole::Grid;
        self.component(parent, spec)
    }

    /// Визуальный разделитель групп.
    pub fn divider(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        self.simple(parent, ComponentKind::Divider, layout)
    }

    /// Отдельная вкладка. Связанная страница управляется приложением по
    /// `CommandId`, а выбранное состояние живёт в общем `NodeState`.
    pub fn tab(
        &mut self,
        parent: NodeId,
        label: ResourceId,
        command: CommandId,
        selected: bool,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::TabView);
        spec.layout = layout;
        spec.content = Content::Text(label);
        spec.command = command;
        spec.role = SemanticRole::Tab;
        spec.accessible_name = label;
        if selected {
            spec.state.insert(crate::NodeState::SELECTED);
        }
        self.component(parent, spec)
    }

    /// Dialog surface с отдельной accessibility-границей.
    pub fn dialog(
        &mut self,
        parent: NodeId,
        accessible_name: ResourceId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::Dialog);
        spec.layout = layout;
        spec.role = SemanticRole::Dialog;
        spec.accessible_name = accessible_name;
        spec.tab_index = -1;
        self.component(parent, spec)
    }

    /// ProgressBar, `value` в диапазоне 0..=1000.
    pub fn progress(
        &mut self,
        parent: NodeId,
        value: u16,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::ProgressBar);
        spec.layout = layout;
        spec.content = Content::Value(value.min(1000));
        spec.role = SemanticRole::Progress;
        self.component(parent, spec)
    }

    fn simple(
        &mut self,
        parent: NodeId,
        kind: ComponentKind,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(kind);
        spec.layout = layout;
        self.component(parent, spec)
    }

    fn control(
        &mut self,
        parent: NodeId,
        kind: ComponentKind,
        role: SemanticRole,
        label: ResourceId,
        command: CommandId,
        layout: LayoutSpec,
    ) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(kind);
        spec.layout = layout;
        spec.content = Content::Text(label);
        spec.command = command;
        spec.role = role;
        spec.accessible_name = label;
        self.component(parent, spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_and_menu_keep_resource_and_accessibility_contract() {
        let mut tree = Tree::<8>::new();
        let root = tree.root();
        let mut ui = UiBuilder::new(&mut tree);
        let menu = ui.menu(root, LayoutSpec::fill()).unwrap();
        let item = ui
            .menu_item(menu, ResourceId(7), CommandId(8), LayoutSpec::fill())
            .unwrap();
        let image = ui
            .image(menu, ResourceId(41), ResourceId(42), LayoutSpec::fill())
            .unwrap();
        assert_eq!(tree.get(menu).unwrap().role, SemanticRole::Menu);
        assert_eq!(tree.get(item).unwrap().role, SemanticRole::MenuItem);
        let image = tree.get(image).unwrap();
        assert_eq!(image.kind, ComponentKind::Image);
        assert_eq!(image.content, Content::Resource(ResourceId(41)));
        assert_eq!(image.accessible_name, ResourceId(42));
        assert_eq!(image.role, SemanticRole::Image);
    }

    #[test]
    fn rich_controls_keep_kind_value_and_semantic_role() {
        let mut tree = Tree::<16>::new();
        let root = tree.root();
        let mut ui = UiBuilder::new(&mut tree);
        let radio = ui
            .radio_button(root, ResourceId(1), CommandId(2), LayoutSpec::fill())
            .unwrap();
        let area = ui
            .text_area(root, ResourceId(3), ResourceId(4), LayoutSpec::fill())
            .unwrap();
        let slider = ui
            .slider(root, 2_000, ResourceId(5), LayoutSpec::fill())
            .unwrap();
        let select = ui
            .select(root, ResourceId(6), ResourceId(7), LayoutSpec::fill())
            .unwrap();
        let tab = ui
            .tab(root, ResourceId(8), CommandId(9), true, LayoutSpec::fill())
            .unwrap();

        assert_eq!(tree.get(radio).unwrap().role, SemanticRole::RadioButton);
        assert_eq!(tree.get(area).unwrap().role, SemanticRole::TextArea);
        assert_eq!(tree.get(slider).unwrap().content, Content::Value(1000));
        assert_eq!(tree.get(select).unwrap().role, SemanticRole::ComboBox);
        assert!(tree
            .get(tab)
            .unwrap()
            .state
            .contains(crate::NodeState::SELECTED));
    }
}

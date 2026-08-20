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

    /// Виртуализируемый ListView container. Item creation должен ограничивать
    /// caller видимым диапазоном, который runtime сообщает через ABI event.
    pub fn list_view(&mut self, parent: NodeId, layout: LayoutSpec) -> Result<NodeId, TreeError> {
        let mut spec = NodeSpec::new(ComponentKind::ListView);
        spec.layout = layout;
        spec.role = SemanticRole::List;
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
}

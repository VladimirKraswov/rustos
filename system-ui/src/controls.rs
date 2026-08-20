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

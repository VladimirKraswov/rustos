//! Оркестрация state → layout → display list → backend.

use rustos_video::{DamageRegion, Rect};

use crate::{
    display_list::RenderBackend, event, Content, DirtyFlags, DispatchResult, DisplayList,
    InputEvent, NodeId, NodeState, SemanticsTree, Theme, Tree, TreeError, UiBuilder,
};

/// Диагностические счётчики, доступные inspector'у без включения логирования
/// в hot path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerformanceCounters {
    /// Успешно отрисованные frames.
    pub frames: u64,
    /// Layout passes.
    pub layout_passes: u64,
    /// Перестроения display list.
    pub display_list_builds: u64,
    /// Выполненные backend-команды с учётом damage clips.
    pub rendered_commands: u64,
    /// Суммарная площадь пересечений command/damage; удобный repaint budget.
    pub rasterized_pixels: u64,
    /// Последнее число component nodes.
    pub nodes: u32,
    /// Последнее число display commands.
    pub display_commands: u32,
}

/// Damage и counters одного кадра. Rectangle-ы копируются до очистки
/// runtime tracker, чтобы compositor мог передать ровно их scanout driver'у.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameResult<const D: usize> {
    rects: [Rect; D],
    len: usize,
    /// Число реально выполненных display commands.
    pub commands: u32,
    /// Оценка затронутых пикселей.
    pub pixels: u64,
}

impl<const D: usize> FrameResult<D> {
    const EMPTY: Self = Self {
        rects: [Rect::EMPTY; D],
        len: 0,
        commands: 0,
        pixels: 0,
    };

    /// Пустой результат для безопасного отказа backend/application adapter.
    pub const fn empty() -> Self {
        Self::EMPTY
    }

    /// Damage rectangles кадра.
    pub fn damage(&self) -> &[Rect] {
        &self.rects[..self.len]
    }

    /// Был ли raster фактически нужен.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Runtime-level ошибки.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    /// Ошибка дерева/ID.
    Tree(TreeError),
    /// Display list capacity недостаточна. Частичный frame не показан.
    DisplayListCapacity,
}

impl From<TreeError> for RuntimeError {
    fn from(value: TreeError) -> Self {
        Self::Tree(value)
    }
}

/// Одна UI-сессия. `N`, `C`, `D` являются явными memory/performance budgets:
/// components, display commands и damage rectangles соответственно.
pub struct Runtime<const N: usize, const C: usize, const D: usize> {
    tree: Tree<N>,
    display_list: DisplayList<C>,
    semantics: SemanticsTree<N>,
    damage: DamageRegion<D>,
    input: event::InputState,
    viewport: Rect,
    theme: Theme,
    display_valid: bool,
    counters: PerformanceCounters,
}

impl<const N: usize, const C: usize, const D: usize> Runtime<N, C, D> {
    /// Создаёт runtime и помечает весь viewport повреждённым для первого кадра.
    pub fn new(viewport: Rect, theme: Theme) -> Self {
        assert!(C > 0 && D > 0);
        let mut damage = DamageRegion::new(viewport);
        damage.add(viewport);
        Self {
            tree: Tree::new(),
            display_list: DisplayList::new(),
            semantics: SemanticsTree::new(),
            damage,
            input: event::InputState::new(),
            viewport,
            theme,
            display_valid: false,
            counters: PerformanceCounters::default(),
        }
    }

    /// Read-only component tree для inspector/tests.
    pub const fn tree(&self) -> &Tree<N> {
        &self.tree
    }

    /// Rust builder, создающий ту же внутреннюю модель, что `.rui` loader.
    pub fn builder(&mut self) -> UiBuilder<'_, N> {
        UiBuilder::new(&mut self.tree)
    }

    /// Семантический snapshot последнего frame.
    pub const fn semantics(&self) -> &SemanticsTree<N> {
        &self.semantics
    }

    /// Текущая тема.
    pub const fn theme(&self) -> Theme {
        self.theme
    }

    /// Счётчики inspector/performance budgets.
    pub const fn counters(&self) -> PerformanceCounters {
        self.counters
    }

    /// Меняет viewport после window resize/container query.
    pub fn resize(&mut self, viewport: Rect) {
        if viewport == self.viewport {
            return;
        }
        let old = self.viewport;
        self.viewport = viewport;
        self.damage = DamageRegion::new(viewport);
        self.damage.add(old);
        self.damage.add(viewport);
        if let Some(root) = self.tree.get_mut_internal(self.tree.root()) {
            root.dirty.insert(DirtyFlags::LAYOUT);
        }
        self.display_valid = false;
    }

    /// Меняет системную тему без изменения поведения компонентов.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        self.display_valid = false;
        self.damage.add(self.viewport);
    }

    /// Помечает весь surface повреждённым (например, после восстановления
    /// окна, mode-set или потери GPU backing store).
    pub fn invalidate_all(&mut self) {
        self.damage.add(self.viewport);
    }

    /// Изменяет state с локальным damage вместо полного repaint.
    pub fn set_state(&mut self, id: NodeId, state: NodeState) -> Result<(), RuntimeError> {
        let rect = self.tree.set_state(id, state)?;
        self.damage.add(rect);
        Ok(())
    }

    /// Изменяет ресурс/значение с локальным damage.
    pub fn set_content(&mut self, id: NodeId, content: Content) -> Result<(), RuntimeError> {
        let rect = self.tree.set_content(id, content)?;
        self.damage.add(rect);
        Ok(())
    }

    /// Повреждает bounds узла, содержимое внешнего ресурса которого изменилось
    /// без смены `ResourceId`. Это штатный путь для terminal lines, часов и
    /// других динамических provider'ов: display list остаётся пригодным, layout
    /// не запускается, а backend повторно читает ресурс только внутри damage.
    /// Если новая версия меняет геометрию, caller обязан отдельно обновить
    /// layout/content через соответствующий typed setter.
    pub fn invalidate_content(&mut self, id: NodeId) -> Result<(), RuntimeError> {
        let rect = self
            .tree
            .get(id)
            .ok_or(RuntimeError::Tree(TreeError::InvalidNode))?
            .rect;
        self.damage.add(rect);
        Ok(())
    }

    /// Единый dispatch мыши/клавиатуры. Pointer capture гарантирует, что Up
    /// попадёт тому же component даже после выхода указателя за его bounds.
    pub fn dispatch(&mut self, input: InputEvent) -> DispatchResult {
        let (tree, state, damage) = (&mut self.tree, &mut self.input, &mut self.damage);
        event::dispatch(tree, state, input, |rect| damage.add(rect))
    }

    /// Выполняет только необходимые стадии и raster только внутри damage.
    pub fn render<B: RenderBackend>(
        &mut self,
        backend: &mut B,
    ) -> Result<FrameResult<D>, RuntimeError> {
        if self.tree.has_dirty(DirtyFlags::LAYOUT) {
            let (tree, damage) = (&mut self.tree, &mut self.damage);
            crate::layout::perform(tree, self.viewport, |rect| damage.add(rect));
            self.counters.layout_passes = self.counters.layout_passes.saturating_add(1);
        }
        if !self.display_valid || self.tree.has_dirty(DirtyFlags::PAINT) {
            self.display_list.rebuild(&self.tree, self.theme);
            if self.display_list.overflowed() {
                return Err(RuntimeError::DisplayListCapacity);
            }
            self.tree.clear_paint_dirty();
            self.display_valid = true;
            self.counters.display_list_builds = self.counters.display_list_builds.saturating_add(1);
        }
        if self.tree.has_dirty(DirtyFlags::SEMANTICS) {
            self.semantics.rebuild(&self.tree);
            self.tree.clear_semantics_dirty();
        }
        if self.damage.is_empty() {
            return Ok(FrameResult::EMPTY);
        }
        let mut frame = FrameResult::EMPTY;
        for (index, rect) in self.damage.iter().copied().enumerate() {
            frame.rects[index] = rect;
            frame.len += 1;
        }
        let (commands, pixels) = self.display_list.execute(backend, &self.damage);
        frame.commands = commands;
        frame.pixels = pixels;
        self.damage.clear();
        self.counters.frames = self.counters.frames.saturating_add(1);
        self.counters.rendered_commands = self
            .counters
            .rendered_commands
            .saturating_add(u64::from(commands));
        self.counters.rasterized_pixels = self.counters.rasterized_pixels.saturating_add(pixels);
        self.counters.nodes = self.tree.len() as u32;
        self.counters.display_commands = self.display_list.len() as u32;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Color, CommandId, ComponentKind, FontSpec, LayoutSpec, Length, NodeSpec, PointerEvent,
        PointerKind, ResourceId,
    };

    #[derive(Default)]
    struct Headless {
        calls: u32,
    }
    impl RenderBackend for Headless {
        fn fill(&mut self, _: Rect, _: Color, _: Rect) {
            self.calls += 1;
        }
        fn border(&mut self, _: Rect, _: Color, _: u8, _: Rect) {
            self.calls += 1;
        }
        fn text(&mut self, _: Rect, _: ResourceId, _: Color, _: FontSpec, _: Rect) {
            self.calls += 1;
        }
        fn image(&mut self, _: Rect, _: ResourceId, _: Color, _: Rect) {
            self.calls += 1;
        }
    }

    #[test]
    fn property_change_damages_control_not_whole_window() {
        let viewport = Rect::new(0, 0, 800, 600);
        let mut runtime = Runtime::<16, 64, 8>::new(viewport, Theme::dark());
        let button = {
            let root = runtime.tree().root();
            let mut builder = runtime.builder();
            let mut layout = LayoutSpec::default();
            layout.width = Length::Px(120);
            layout.height = Length::Px(40);
            builder
                .button(root, ResourceId(1), CommandId(7), layout)
                .unwrap()
        };
        let mut backend = Headless::default();
        runtime.render(&mut backend).unwrap();
        let layout_passes = runtime.counters().layout_passes;
        runtime.set_state(button, NodeState::HOVERED).unwrap();
        let frame = runtime.render(&mut backend).unwrap();
        assert_eq!(frame.damage().len(), 1);
        assert!(frame.damage()[0].area() < viewport.area() / 4);
        assert_eq!(runtime.counters().layout_passes, layout_passes);
    }

    #[test]
    fn dynamic_resource_invalidates_only_its_existing_bounds() {
        let viewport = Rect::new(0, 0, 800, 600);
        let mut runtime = Runtime::<8, 32, 8>::new(viewport, Theme::dark());
        let text = {
            let root = runtime.tree().root();
            let mut builder = runtime.builder();
            let mut layout = LayoutSpec::default();
            layout.width = Length::Px(320);
            layout.height = Length::Px(24);
            builder.text(root, ResourceId(7), layout).unwrap()
        };
        let mut backend = Headless::default();
        runtime.render(&mut backend).unwrap();
        let expected = runtime.tree().get(text).unwrap().rect;
        let counters = runtime.counters();

        // Provider обновил bytes за тем же ResourceId: дерево и display list
        // не перестраиваются, но backend обязан перечитать только эту строку.
        runtime.invalidate_content(text).unwrap();
        let frame = runtime.render(&mut backend).unwrap();

        assert_eq!(frame.damage(), &[expected]);
        assert!(expected.area() < viewport.area() / 4);
        assert_eq!(runtime.counters().layout_passes, counters.layout_passes);
        assert_eq!(
            runtime.counters().display_list_builds,
            counters.display_list_builds
        );
        assert_eq!(
            runtime.invalidate_content(NodeId::NONE),
            Err(RuntimeError::Tree(TreeError::InvalidNode))
        );
    }

    #[test]
    fn button_activation_returns_shared_command_object() {
        let mut runtime = Runtime::<8, 32, 8>::new(Rect::new(0, 0, 320, 200), Theme::dark());
        let button = {
            let root = runtime.tree().root();
            let mut builder = runtime.builder();
            let mut spec = NodeSpec::new(ComponentKind::Button);
            spec.layout.width = Length::Px(100);
            spec.layout.height = Length::Px(40);
            spec.command = CommandId(42);
            builder.component(root, spec).unwrap()
        };
        runtime.render(&mut Headless::default()).unwrap();
        let rect = runtime.tree().get(button).unwrap().rect;
        runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Down,
            rect.x + 2,
            rect.y + 2,
        )));
        let result = runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Up,
            rect.x + 2,
            rect.y + 2,
        )));
        assert_eq!(result.command, CommandId(42));
    }

    struct Snapshot(u64);

    impl Snapshot {
        fn new() -> Self {
            Self(0xcbf2_9ce4_8422_2325)
        }

        fn word(&mut self, value: u64) {
            self.0 ^= value;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }

        fn rect(&mut self, rect: Rect) {
            for value in [
                rect.x as i64 as u64,
                rect.y as i64 as u64,
                u64::from(rect.width),
                u64::from(rect.height),
            ] {
                self.word(value);
            }
        }

        fn color(&mut self, color: Color) {
            self.word((u64::from(color.r) << 16) | (u64::from(color.g) << 8) | u64::from(color.b));
        }
    }

    impl RenderBackend for Snapshot {
        fn shadow(&mut self, rect: Rect, radius: u8, color: Color, clip: Rect) {
            self.word(7);
            self.rect(rect);
            self.word(u64::from(radius));
            self.color(color);
            self.rect(clip);
        }

        fn fill(&mut self, rect: Rect, color: Color, clip: Rect) {
            self.word(1);
            self.rect(rect);
            self.color(color);
            self.rect(clip);
        }

        fn border(&mut self, rect: Rect, color: Color, width: u8, clip: Rect) {
            self.word(2);
            self.rect(rect);
            self.color(color);
            self.word(u64::from(width));
            self.rect(clip);
        }

        fn text(
            &mut self,
            rect: Rect,
            resource: ResourceId,
            color: Color,
            font: FontSpec,
            clip: Rect,
        ) {
            self.word(3);
            self.rect(rect);
            self.word(u64::from(resource.0));
            self.color(color);
            self.word(u64::from(font.size));
            self.word(u64::from(font.bold));
            self.word(u64::from(font.italic));
            self.word(u64::from(font.monospace));
            self.word(font.align as u8 as u64);
            self.word(u64::from(font.vertical_center));
            self.rect(clip);
        }

        fn image(&mut self, rect: Rect, resource: ResourceId, color: Color, clip: Rect) {
            self.word(4);
            self.rect(rect);
            self.word(u64::from(resource.0));
            self.color(color);
            self.rect(clip);
        }

        fn rounded_fill(&mut self, rect: Rect, color: Color, radius: u8, clip: Rect) {
            self.word(5);
            self.rect(rect);
            self.color(color);
            self.word(u64::from(radius));
            self.rect(clip);
        }

        fn rounded_border(&mut self, rect: Rect, color: Color, width: u8, radius: u8, clip: Rect) {
            self.word(6);
            self.rect(rect);
            self.color(color);
            self.word(u64::from(width));
            self.word(u64::from(radius));
            self.rect(clip);
        }
    }

    #[test]
    fn headless_display_list_snapshot_is_stable() {
        let mut runtime = Runtime::<8, 32, 4>::new(Rect::new(0, 0, 120, 60), Theme::dark());
        {
            let root = runtime.tree().root();
            let mut builder = runtime.builder();
            let mut spec = NodeSpec::new(ComponentKind::Button);
            spec.layout.width = Length::Px(80);
            spec.layout.height = Length::Px(32);
            spec.content = Content::Text(ResourceId(5));
            spec.style = 1;
            builder.component(root, spec).unwrap();
        }
        let mut snapshot = Snapshot::new();
        runtime.render(&mut snapshot).unwrap();
        // Snapshot включает порядок primitives, geometry, theme tokens,
        // font contract и damage clip. Намеренное изменение любого из них
        // требует визуальной проверки и явного обновления hash.
        assert_eq!(snapshot.0, 18_073_759_078_714_162_449);
    }

    #[test]
    fn container_breakpoint_changes_row_to_column() {
        let mut runtime = Runtime::<8, 32, 8>::new(Rect::new(0, 0, 600, 200), Theme::dark());
        let (left, right) = {
            let root = runtime.tree().root();
            let mut builder = runtime.builder();
            let mut row = NodeSpec::new(ComponentKind::Row);
            row.layout.container_breakpoint = 500;
            let row = builder.component(root, row).unwrap();
            let left = builder
                .component(row, NodeSpec::new(ComponentKind::Panel))
                .unwrap();
            let right = builder
                .component(row, NodeSpec::new(ComponentKind::Panel))
                .unwrap();
            (left, right)
        };
        runtime.render(&mut Headless::default()).unwrap();
        assert_eq!(
            runtime.tree().get(left).unwrap().rect.y,
            runtime.tree().get(right).unwrap().rect.y
        );
        runtime.resize(Rect::new(0, 0, 400, 200));
        runtime.render(&mut Headless::default()).unwrap();
        assert!(
            runtime.tree().get(right).unwrap().rect.y > runtime.tree().get(left).unwrap().rect.y
        );
    }
}

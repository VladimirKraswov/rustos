//! Оркестрация state → layout → display list → backend.

use rustos_video::{DamageRegion, Rect};

use crate::{
    display_list::RenderBackend, event, Content, DirtyFlags, DispatchResult, DisplayList,
    InputEvent, ListViewState, NodeId, NodeState, PointerKind, ScrollAxis, ScrollConfig,
    SelectionMode, SemanticsTree, Theme, Tree, TreeError, UiBuilder,
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
    /// Нормализованные wheel/trackpad events, реально изменившие runtime.
    pub scroll_events: u64,
    /// Logical items во всех ListView текущего viewport с overscan.
    pub visible_items: u32,
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

    /// Инициализирует runtime непосредственно в storage владельца. Это путь
    /// для kernel/server объектов с большими compile-time budgets: массивы
    /// tree/display-list не появляются временно на небольшом kernel stack.
    ///
    /// # Safety
    /// `destination` должен быть выровнен, доступен для записи и указывать на
    /// неинициализированное хранилище размером `Runtime<N, C, D>`.
    pub unsafe fn initialize_in_place(destination: *mut Self, viewport: Rect, theme: Theme) {
        assert!(C > 0 && D > 0);
        // SAFETY: все поля destination инициализируются ровно один раз до
        // публикации Runtime; вложенные методы получают свои field storage.
        unsafe {
            Tree::initialize_in_place(core::ptr::addr_of_mut!((*destination).tree));
            DisplayList::initialize_in_place(core::ptr::addr_of_mut!((*destination).display_list));
            SemanticsTree::initialize_in_place(core::ptr::addr_of_mut!((*destination).semantics));
            let mut damage = DamageRegion::new(viewport);
            damage.add(viewport);
            core::ptr::addr_of_mut!((*destination).damage).write(damage);
            core::ptr::addr_of_mut!((*destination).input).write(event::InputState::new());
            core::ptr::addr_of_mut!((*destination).viewport).write(viewport);
            core::ptr::addr_of_mut!((*destination).theme).write(theme);
            core::ptr::addr_of_mut!((*destination).display_valid).write(false);
            core::ptr::addr_of_mut!((*destination).counters).write(PerformanceCounters::default());
        }
    }

    /// Начинает новую component-сессию в уже выделенном storage.
    /// В отличие от `*runtime = Runtime::new(...)`, метод не создаёт большой
    /// временный объект на стеке и потому пригоден для GUI микроядра.
    pub fn rebuild(&mut self, viewport: Rect, theme: Theme) {
        assert!(C > 0 && D > 0);
        self.tree.reset();
        self.display_list.clear();
        self.semantics.clear();
        self.damage = DamageRegion::new(viewport);
        self.damage.add(viewport);
        self.input = event::InputState::new();
        self.viewport = viewport;
        self.theme = theme;
        self.display_valid = false;
        self.counters = PerformanceCounters::default();
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

    /// Настраивает scroll policy публичного ScrollView/ListView.
    pub fn set_scroll_config(
        &mut self,
        id: NodeId,
        config: ScrollConfig,
    ) -> Result<(), RuntimeError> {
        let rect = self.tree.set_scroll_config(id, config)?;
        self.damage.add(rect);
        self.display_valid = false;
        Ok(())
    }

    /// Связывает ListView с logical collection source. Число живых nodes от
    /// `item_count` не зависит.
    pub fn configure_list_view(
        &mut self,
        id: NodeId,
        item_count: u32,
        item_extent: u32,
        selection: SelectionMode,
    ) -> Result<(), RuntimeError> {
        let rect = self
            .tree
            .configure_list_view(id, item_count, item_extent, selection)?;
        self.damage.add(rect);
        self.display_valid = false;
        Ok(())
    }

    /// Связывает самостоятельный ScrollBar с существующим ScrollModel.
    pub fn bind_scroll_bar(
        &mut self,
        bar: NodeId,
        target: NodeId,
        axis: ScrollAxis,
    ) -> Result<(), RuntimeError> {
        let rect = self.tree.bind_scroll_bar(bar, target, axis)?;
        self.damage.add(rect);
        self.display_valid = false;
        Ok(())
    }

    /// Snapshot logical ListView state для delegate/data binding.
    pub fn list_view_state(&self, id: NodeId) -> Option<&ListViewState> {
        self.tree
            .get(id)
            .and_then(|node| (node.kind == crate::ComponentKind::ListView).then_some(&node.list))
    }

    /// Программная мгновенная прокрутка.
    pub fn scroll_to(
        &mut self,
        id: NodeId,
        axis: ScrollAxis,
        offset: u64,
    ) -> Result<bool, RuntimeError> {
        let node = self
            .tree
            .get_mut_internal(id)
            .ok_or(RuntimeError::Tree(TreeError::InvalidNode))?;
        let changed = node.scroll.model_mut(axis).scroll_to(offset);
        let rect = node.rect;
        if changed {
            node.dirty.insert(DirtyFlags::LAYOUT);
            node.dirty.insert(DirtyFlags::PAINT);
            self.damage.add(rect);
            self.display_valid = false;
        }
        Ok(changed)
    }

    /// Минимально сдвигает viewport так, чтобы диапазон content был виден.
    pub fn ensure_visible(
        &mut self,
        id: NodeId,
        axis: ScrollAxis,
        start: u64,
        end: u64,
    ) -> Result<bool, RuntimeError> {
        let node = self
            .tree
            .get_mut_internal(id)
            .ok_or(RuntimeError::Tree(TreeError::InvalidNode))?;
        let changed = node.scroll.model_mut(axis).ensure_visible(start, end);
        let rect = node.rect;
        if changed {
            node.dirty.insert(DirtyFlags::LAYOUT);
            node.dirty.insert(DirtyFlags::PAINT);
            self.damage.add(rect);
            self.display_valid = false;
        }
        Ok(changed)
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
        let result = event::dispatch(tree, state, input, |rect| damage.add(rect));
        if matches!(input, InputEvent::Pointer(pointer) if pointer.kind == PointerKind::Scroll)
            && result.changed
        {
            self.counters.scroll_events = self.counters.scroll_events.saturating_add(1);
        }
        if result.changed {
            self.display_valid = false;
        }
        result
    }

    /// Один шаг smooth scrolling. Frame scheduler вызывает метод не чаще
    /// одного раза на frame; reduced-motion завершает переход сразу.
    pub fn advance_scroll_frame(&mut self) -> bool {
        let response = if self.theme.reduced_motion {
            1_000
        } else {
            280
        };
        let mut ids = [NodeId::NONE; N];
        let mut len = 0usize;
        for id in self.tree.ids() {
            ids[len] = id;
            len += 1;
        }
        let mut changed = false;
        for id in ids.into_iter().take(len) {
            let Some(node) = self.tree.get_mut_internal(id) else {
                continue;
            };
            let node_changed = node.scroll.horizontal.advance_frame(response)
                | node.scroll.vertical.advance_frame(response);
            if node_changed {
                node.dirty.insert(DirtyFlags::LAYOUT);
                node.dirty.insert(DirtyFlags::PAINT);
                self.damage.add(node.rect);
                changed = true;
            }
        }
        if changed {
            self.display_valid = false;
        }
        changed
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
        self.counters.visible_items = self
            .tree
            .ids()
            .filter_map(|id| {
                let node = self.tree.get(id)?;
                node.list
                    .is_configured()
                    .then_some(node.list.visible_range(node.scroll.vertical).len())
            })
            .fold(0u32, u32::saturating_add);
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
            let layout = LayoutSpec {
                width: Length::Px(120),
                height: Length::Px(40),
                ..LayoutSpec::default()
            };
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
            let layout = LayoutSpec {
                width: Length::Px(320),
                height: Length::Px(24),
                ..LayoutSpec::default()
            };
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

    #[test]
    fn wheel_chains_only_unused_delta_to_parent_scroll_view() {
        let mut runtime = Runtime::<12, 96, 12>::new(Rect::new(0, 0, 300, 300), Theme::light());
        let (outer, inner) = {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let outer = ui
                .scroll_view(root, ScrollConfig::VERTICAL, LayoutSpec::fill())
                .unwrap();
            let inner_layout = LayoutSpec {
                width: Length::Fill(1),
                height: Length::Px(100),
                ..LayoutSpec::default()
            };
            let inner = ui
                .scroll_view(outer, ScrollConfig::VERTICAL, inner_layout)
                .unwrap();
            let mut tall = NodeSpec::new(ComponentKind::Panel);
            tall.layout.width = Length::Fill(1);
            tall.layout.height = Length::Px(300);
            ui.component(inner, tall).unwrap();
            let mut tail = NodeSpec::new(ComponentKind::Panel);
            tail.layout.width = Length::Fill(1);
            tail.layout.height = Length::Px(400);
            ui.component(outer, tail).unwrap();
            (outer, inner)
        };
        runtime.render(&mut Headless::default()).unwrap();
        let inner_rect = runtime.tree().get(inner).unwrap().rect;
        let mut wheel = PointerEvent::at(PointerKind::Scroll, inner_rect.x + 10, inner_rect.y + 10);
        wheel.scroll_y = 250;
        let result = runtime.dispatch(InputEvent::Pointer(wheel));
        assert!(result.changed && result.consumed);
        assert_eq!(
            runtime.tree().get(inner).unwrap().scroll.vertical.offset(),
            200
        );
        assert_eq!(
            runtime.tree().get(outer).unwrap().scroll.vertical.offset(),
            50
        );
        assert_eq!(runtime.counters().scroll_events, 1);
    }

    #[test]
    fn horizontal_scroll_view_consumes_trackpad_delta_without_focus() {
        let mut runtime = Runtime::<6, 48, 8>::new(Rect::new(0, 0, 240, 120), Theme::light());
        let scroll = {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let scroll = ui
                .scroll_view(root, ScrollConfig::BOTH, LayoutSpec::fill())
                .unwrap();
            let mut content = NodeSpec::new(ComponentKind::Panel);
            content.layout.width = Length::Px(600);
            content.layout.height = Length::Px(80);
            ui.component(scroll, content).unwrap();
            scroll
        };
        runtime.render(&mut Headless::default()).unwrap();
        let mut wheel = PointerEvent::at(PointerKind::Scroll, 20, 20);
        wheel.scroll_x = 90;
        let result = runtime.dispatch(InputEvent::Pointer(wheel));
        assert!(result.changed);
        assert_eq!(
            runtime
                .tree()
                .get(scroll)
                .unwrap()
                .scroll
                .horizontal
                .offset(),
            90
        );
    }

    #[test]
    fn list_keyboard_navigation_selects_and_ensures_visible() {
        let mut runtime = Runtime::<6, 48, 8>::new(Rect::new(0, 0, 260, 160), Theme::light());
        let list = {
            let root = runtime.tree().root();
            runtime
                .builder()
                .list_view(root, LayoutSpec::fill())
                .unwrap()
        };
        runtime
            .configure_list_view(list, 50_000, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Down,
            10,
            10,
        )));
        let result = runtime.dispatch(InputEvent::Key(crate::KeyEvent {
            key: crate::Key::End,
            pressed: true,
            modifiers: 0,
            shift: false,
        }));
        assert!(result.changed && result.consumed);
        let state = runtime.list_view_state(list).unwrap();
        assert_eq!(state.selection().current(), Some(49_999));
        assert_eq!(
            runtime.tree().get(list).unwrap().scroll.vertical.offset(),
            runtime.tree().get(list).unwrap().scroll.vertical.maximum()
        );
        assert!(
            state
                .visible_range(runtime.tree().get(list).unwrap().scroll.vertical)
                .len()
                < 20
        );
    }

    /// Строит ListView с фиксированным пулом из `count` recycled Text-delegates.
    /// Возвращает ID списка и массив delegates в порядке создания.
    fn build_list_with_delegates<const N: usize, const C: usize, const D: usize>(
        runtime: &mut Runtime<N, C, D>,
        count: usize,
    ) -> (NodeId, [NodeId; 16]) {
        assert!(count <= 16, "delegate pool is bounded to 16 in tests");
        let root = runtime.tree().root();
        let mut ui = runtime.builder();
        let list = ui.list_view(root, LayoutSpec::fill()).unwrap();
        let mut delegates = [NodeId::NONE; 16];
        for slot in delegates.iter_mut().take(count) {
            let mut row = NodeSpec::new(ComponentKind::Text);
            row.layout = LayoutSpec {
                width: Length::Fill(1),
                height: Length::Px(24),
                ..LayoutSpec::default()
            };
            row.content = Content::Text(ResourceId(1));
            *slot = ui.component(list, row).unwrap();
        }
        (list, delegates)
    }

    #[test]
    fn list_view_wheel_over_delegate_advances_offset_and_range() {
        let mut runtime = Runtime::<20, 64, 16>::new(Rect::new(0, 0, 320, 240), Theme::light());
        let (list, _delegates) = build_list_with_delegates(&mut runtime, 16);
        runtime
            .configure_list_view(list, 50_000, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        // Wheel над дочерним delegate (y=120..144) обязан найти ListView ancestor.
        let mut wheel = PointerEvent::at(PointerKind::Scroll, 5, 130);
        wheel.scroll_y = 100;
        let result = runtime.dispatch(InputEvent::Pointer(wheel));
        assert!(result.consumed && result.changed);
        assert_eq!(result.target, list);
        let model = runtime.tree().get(list).unwrap().scroll.vertical;
        assert_eq!(model.offset(), 100);
        let range = runtime.list_view_state(list).unwrap().visible_range(model);
        assert_eq!(range.start, 2);
        assert_eq!(range.end, 17);
    }

    #[test]
    fn list_view_scroll_to_max_clamps_and_includes_last_item() {
        let mut runtime = Runtime::<20, 64, 16>::new(Rect::new(0, 0, 320, 240), Theme::light());
        let (list, _delegates) = build_list_with_delegates(&mut runtime, 16);
        runtime
            .configure_list_view(list, 50_000, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        runtime
            .scroll_to(list, ScrollAxis::Vertical, u64::MAX)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        let model = runtime.tree().get(list).unwrap().scroll.vertical;
        assert_eq!(model.offset(), 1_199_760);
        let range = runtime.list_view_state(list).unwrap().visible_range(model);
        assert_eq!(range.end, 50_000);
    }

    #[test]
    fn list_view_wheel_back_to_start() {
        let mut runtime = Runtime::<20, 64, 16>::new(Rect::new(0, 0, 320, 240), Theme::light());
        let (list, _delegates) = build_list_with_delegates(&mut runtime, 16);
        runtime
            .configure_list_view(list, 50_000, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        runtime
            .scroll_to(list, ScrollAxis::Vertical, u64::MAX)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        // Wheel назад до начала: i16 delta ограничен, поэтому несколько событий.
        while runtime.tree().get(list).unwrap().scroll.vertical.offset() > 0 {
            let mut wheel = PointerEvent::at(PointerKind::Scroll, 5, 130);
            wheel.scroll_y = -32_768;
            runtime.dispatch(InputEvent::Pointer(wheel));
        }
        runtime.render(&mut Headless::default()).unwrap();
        let model = runtime.tree().get(list).unwrap().scroll.vertical;
        assert_eq!(model.offset(), 0);
        let range = runtime.list_view_state(list).unwrap().visible_range(model);
        assert_eq!(range.start, 0);
    }

    #[test]
    fn list_view_reconfigure_and_resize_clamp_offset() {
        let mut runtime = Runtime::<20, 64, 16>::new(Rect::new(0, 0, 320, 240), Theme::light());
        let (list, _delegates) = build_list_with_delegates(&mut runtime, 16);
        runtime
            .configure_list_view(list, 50_000, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        runtime
            .scroll_to(list, ScrollAxis::Vertical, u64::MAX)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        assert_eq!(
            runtime.tree().get(list).unwrap().scroll.vertical.offset(),
            1_199_760
        );
        // Resize 240 -> 400: offset clamps к новому maximum.
        runtime.resize(Rect::new(0, 0, 320, 400));
        runtime.render(&mut Headless::default()).unwrap();
        assert_eq!(
            runtime.tree().get(list).unwrap().scroll.vertical.offset(),
            1_199_600
        );
        // Reconfigure 50_000 -> 3: offset/target clamps к 0, content = 72.
        runtime
            .configure_list_view(list, 3, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        let model = runtime.tree().get(list).unwrap().scroll.vertical;
        assert_eq!(model.offset(), 0);
        assert_eq!(model.target(), 0);
        assert_eq!(model.content_size(), 72);
    }

    #[test]
    fn list_view_extra_recycled_delegates_get_empty_bounds() {
        let mut runtime = Runtime::<20, 64, 16>::new(Rect::new(0, 0, 320, 240), Theme::light());
        let (list, delegates) = build_list_with_delegates(&mut runtime, 16);
        runtime
            .configure_list_view(list, 50_000, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        let range = runtime
            .list_view_state(list)
            .unwrap()
            .visible_range(runtime.tree().get(list).unwrap().scroll.vertical);
        let expected = range.len() as usize;
        // Первые `expected` delegates имеют non-empty bounds.
        for delegate in &delegates[..expected] {
            let rect = runtime.tree().get(*delegate).unwrap().rect;
            assert!(!rect.is_empty());
        }
        // Делегаты сверх visible_range.len() получают пустые bounds.
        for delegate in &delegates[expected..] {
            let rect = runtime.tree().get(*delegate).unwrap().rect;
            assert!(rect.is_empty());
        }
    }

    #[test]
    fn list_view_wheel_damages_only_list_and_scrollbar() {
        let mut runtime = Runtime::<20, 64, 16>::new(Rect::new(0, 0, 320, 240), Theme::light());
        let (list, _delegates, sibling) = {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let column = ui
                .component(root, NodeSpec::new(ComponentKind::Column))
                .unwrap();
            let list_layout = LayoutSpec {
                width: Length::Fill(1),
                height: Length::Px(200),
                ..LayoutSpec::default()
            };
            let list = ui.list_view(column, list_layout).unwrap();
            let mut delegates = [NodeId::NONE; 16];
            for slot in delegates.iter_mut() {
                let mut row = NodeSpec::new(ComponentKind::Text);
                row.layout = LayoutSpec {
                    width: Length::Fill(1),
                    height: Length::Px(24),
                    ..LayoutSpec::default()
                };
                row.content = Content::Text(ResourceId(1));
                *slot = ui.component(list, row).unwrap();
            }
            let sibling_layout = LayoutSpec {
                width: Length::Fill(1),
                height: Length::Px(40),
                ..LayoutSpec::default()
            };
            let sibling = ui
                .button(column, ResourceId(2), CommandId(1), sibling_layout)
                .unwrap();
            (list, delegates, sibling)
        };
        runtime
            .configure_list_view(list, 50_000, 24, SelectionMode::Extended)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        let mut wheel = PointerEvent::at(PointerKind::Scroll, 5, 130);
        wheel.scroll_y = 100;
        runtime.dispatch(InputEvent::Pointer(wheel));
        let frame = runtime.render(&mut Headless::default()).unwrap();
        // Соседний control и полный viewport не получают лишний damage.
        let sibling_rect = runtime.tree().get(sibling).unwrap().rect;
        let sibling_center_x = sibling_rect.x + sibling_rect.width as i32 / 2;
        let sibling_center_y = sibling_rect.y + sibling_rect.height as i32 / 2;
        for rect in frame.damage() {
            assert!(!rect.contains(sibling_center_x, sibling_center_y));
            assert!(rect.area() < 320 * 240);
        }
    }

    #[test]
    fn page_down_scrolls_focused_controls_scroll_view_ancestor() {
        let mut runtime = Runtime::<8, 64, 8>::new(Rect::new(0, 0, 260, 160), Theme::light());
        let scroll = {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let scroll = ui
                .scroll_view(root, ScrollConfig::VERTICAL, LayoutSpec::fill())
                .unwrap();
            let mut button = NodeSpec::new(ComponentKind::Button);
            button.layout.height = Length::Px(40);
            ui.component(scroll, button).unwrap();
            let mut content = NodeSpec::new(ComponentKind::Panel);
            content.layout.height = Length::Px(600);
            ui.component(scroll, content).unwrap();
            scroll
        };
        runtime.render(&mut Headless::default()).unwrap();
        runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Down,
            10,
            10,
        )));
        let result = runtime.dispatch(InputEvent::Key(crate::KeyEvent {
            key: crate::Key::PageDown,
            pressed: true,
            modifiers: 0,
            shift: false,
        }));
        assert!(result.changed && result.consumed);
        assert_eq!(
            runtime.tree().get(scroll).unwrap().scroll.vertical.offset(),
            160
        );
    }

    #[test]
    fn focus_ring_is_visible_for_keyboard_but_not_pointer_focus() {
        let mut runtime = Runtime::<4, 32, 8>::new(Rect::new(0, 0, 160, 80), Theme::light());
        let button = {
            let root = runtime.tree().root();
            runtime
                .builder()
                .button(root, ResourceId(1), CommandId(1), LayoutSpec::fill())
                .unwrap()
        };
        runtime.render(&mut Headless::default()).unwrap();
        runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Down,
            10,
            10,
        )));
        assert!(runtime
            .tree()
            .get(button)
            .unwrap()
            .state
            .contains(NodeState::FOCUSED));
        assert!(!runtime
            .tree()
            .get(button)
            .unwrap()
            .state
            .contains(NodeState::FOCUS_VISIBLE));
        runtime.dispatch(InputEvent::Key(crate::KeyEvent {
            key: crate::Key::Tab,
            pressed: true,
            modifiers: 0,
            shift: false,
        }));
        assert!(runtime
            .tree()
            .get(button)
            .unwrap()
            .state
            .contains(NodeState::FOCUS_VISIBLE));
    }

    #[test]
    fn standalone_scrollbar_drags_the_bound_scroll_model() {
        let mut runtime = Runtime::<8, 64, 8>::new(Rect::new(0, 0, 240, 180), Theme::light());
        let (scroll, bar) = {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let scroll = ui
                .scroll_view(root, ScrollConfig::VERTICAL, LayoutSpec::fill())
                .unwrap();
            let mut content = NodeSpec::new(ComponentKind::Panel);
            content.layout.height = Length::Px(720);
            ui.component(scroll, content).unwrap();
            let bar_layout = LayoutSpec {
                width: Length::Px(14),
                height: Length::Fill(1),
                align: crate::Align::End,
                ..LayoutSpec::default()
            };
            let bar = ui.scroll_bar(root, bar_layout).unwrap();
            (scroll, bar)
        };
        runtime
            .bind_scroll_bar(bar, scroll, ScrollAxis::Vertical)
            .unwrap();
        runtime.render(&mut Headless::default()).unwrap();
        let bar_rect = runtime.tree().get(bar).unwrap().rect;
        let model = runtime.tree().get(scroll).unwrap().scroll.vertical;
        let geometry = crate::ScrollbarGeometry::with_visibility(
            bar_rect,
            model,
            ScrollAxis::Vertical,
            bar_rect.width,
            24,
            true,
        );
        runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Down,
            geometry.thumb.x + 2,
            geometry.thumb.y + 2,
        )));
        runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Move,
            geometry.thumb.x + 2,
            geometry.thumb.y + 60,
        )));
        runtime.dispatch(InputEvent::Pointer(PointerEvent::at(
            PointerKind::Up,
            geometry.thumb.x + 2,
            geometry.thumb.y + 60,
        )));
        assert!(runtime.tree().get(scroll).unwrap().scroll.vertical.offset() > 0);
    }

    #[test]
    fn rebuild_reuses_storage_without_aliasing_old_node_ids() {
        let mut runtime = Runtime::<8, 64, 8>::new(Rect::new(0, 0, 240, 180), Theme::light());
        let old_button = {
            let root = runtime.tree().root();
            runtime
                .builder()
                .button(root, ResourceId(1), CommandId(1), LayoutSpec::fill())
                .unwrap()
        };
        runtime.render(&mut Headless::default()).unwrap();

        runtime.rebuild(Rect::new(20, 30, 320, 220), Theme::dark());
        assert_eq!(runtime.tree().len(), 1);
        assert!(runtime.tree().get(old_button).is_none());
        assert_eq!(runtime.counters(), PerformanceCounters::default());

        let new_button = {
            let root = runtime.tree().root();
            runtime
                .builder()
                .button(root, ResourceId(2), CommandId(2), LayoutSpec::fill())
                .unwrap()
        };
        assert_ne!(old_button, new_button);
    }
}

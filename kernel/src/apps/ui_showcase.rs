//! Первое приложение на новом системном UI-runtime.
//!
//! Оно не вычисляет координаты controls и не рисует framebuffer напрямую:
//! typed Rust builder создаёт component tree, а адаптер ниже является только
//! CPU backend системного renderer contract. Такой же tree сможет прийти из
//! скомпилированного `.rui` и исполняться в ring 3 через `system-ui` DLL.

use crate::{
    font,
    graphics::{Color, Framebuffer, Rect},
};
use rustos_system_ui::{
    Align, CommandId, ComponentKind, Content, DispatchResult, Edges, FontSpec, FrameResult,
    InputEvent, Key, KeyEvent, LayoutSpec, Length, NodeId, NodeSpec, NodeState, PointerEvent,
    PointerKind, RenderBackend, ResourceId, Runtime, SemanticRole, Theme, ThemeKind, VirtualList,
};

const COMMAND_THEME: CommandId = CommandId(1);
const COMMAND_COUNTER: CommandId = CommandId(2);
const COMMAND_CHECK: CommandId = CommandId(3);

/// Fixed budgets раннего системного приложения. Они видимы inspector'у и не
/// могут неожиданно исчерпать kernel memory.
type UiRuntime = Runtime<24, 64, 8>;

/// Stateful оболочка демонстрации; сами controls остаются декларативными.
pub struct UiShowcase {
    runtime: UiRuntime,
    progress: NodeId,
    counter_button: NodeId,
    theme_switch: NodeId,
    virtual_list: VirtualList,
    counter: u16,
}

impl UiShowcase {
    /// Создаёт форму, адаптирующую Row в Column при ширине меньше 720 px.
    pub fn new(viewport: Rect) -> Self {
        let mut runtime = UiRuntime::new(viewport, Theme::dark());
        let mut progress = NodeId::NONE;
        let mut counter_button = NodeId::NONE;
        let mut theme_switch = NodeId::NONE;
        {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let mut page = NodeSpec::new(ComponentKind::Column);
            page.layout = LayoutSpec {
                width: Length::Fill(1),
                height: Length::Fill(1),
                padding: Edges::all(18),
                gap: 12,
                ..LayoutSpec::default()
            };
            if let Ok(page) = ui.component(root, page) {
                let mut heading = LayoutSpec::default();
                heading.width = Length::Fill(1);
                heading.height = Length::Px(34);
                let _ = ui.text(page, ResourceId(1), heading);

                let mut subtitle = heading;
                subtitle.height = Length::Px(28);
                let _ = ui.text(page, ResourceId(2), subtitle);

                let mut body = NodeSpec::new(ComponentKind::Row);
                body.layout = LayoutSpec {
                    width: Length::Fill(1),
                    height: Length::Fill(1),
                    gap: 12,
                    container_breakpoint: 720,
                    ..LayoutSpec::default()
                };
                if let Ok(body) = ui.component(page, body) {
                    let mut card = NodeSpec::new(ComponentKind::Column);
                    card.layout = LayoutSpec {
                        width: Length::Fill(1),
                        height: Length::Fill(1),
                        min_width: 220,
                        padding: Edges::all(14),
                        gap: 10,
                        ..LayoutSpec::default()
                    };
                    card.style = 0;
                    card.role = SemanticRole::Group;
                    if let Ok(settings) = ui.component(body, card) {
                        let mut line = LayoutSpec::default();
                        line.width = Length::Fill(1);
                        line.height = Length::Px(32);
                        let _ = ui.text(settings, ResourceId(3), line);
                        theme_switch = ui
                            .switch(settings, ResourceId(4), COMMAND_THEME, line)
                            .unwrap_or(NodeId::NONE);
                        let mut checked = NodeSpec::new(ComponentKind::CheckBox);
                        checked.layout = line;
                        checked.content = Content::Text(ResourceId(5));
                        checked.command = COMMAND_CHECK;
                        checked.role = SemanticRole::CheckBox;
                        checked.accessible_name = ResourceId(5);
                        checked.state = NodeState::CHECKED;
                        let _ = ui.component(settings, checked);
                        let mut field = line;
                        field.height = Length::Px(38);
                        let _ = ui.text_field(settings, ResourceId(6), ResourceId(7), field);
                        counter_button = ui
                            .button(settings, ResourceId(8), COMMAND_COUNTER, field)
                            .unwrap_or(NodeId::NONE);
                        progress = ui.progress(settings, 350, line).unwrap_or(NodeId::NONE);
                    }

                    let mut list_card = NodeSpec::new(ComponentKind::Column);
                    list_card.layout = LayoutSpec {
                        width: Length::Fill(2),
                        height: Length::Fill(1),
                        min_width: 280,
                        padding: Edges::all(14),
                        gap: 6,
                        ..LayoutSpec::default()
                    };
                    list_card.role = SemanticRole::Group;
                    if let Ok(list_card) = ui.component(body, list_card) {
                        let mut title = LayoutSpec::default();
                        title.width = Length::Fill(1);
                        title.height = Length::Px(30);
                        let _ = ui.text(list_card, ResourceId(9), title);
                        let mut list_layout = LayoutSpec {
                            width: Length::Fill(1),
                            height: Length::Fill(1),
                            padding: Edges::all(8),
                            gap: 4,
                            ..LayoutSpec::default()
                        };
                        list_layout.align = Align::Stretch;
                        if let Ok(list) = ui.list_view(list_card, list_layout) {
                            for resource in 10..=16 {
                                let mut item = LayoutSpec::default();
                                item.width = Length::Fill(1);
                                item.height = Length::Px(28);
                                let _ = ui.text(list, ResourceId(resource), item);
                            }
                        }
                    }
                }
            }
        }
        let mut virtual_list = VirtualList::new(50_000, 28);
        virtual_list.set_viewport(viewport.height.saturating_sub(130));
        Self {
            runtime,
            progress,
            counter_button,
            theme_switch,
            virtual_list,
            counter: 350,
        }
    }

    /// Обновляет viewport после resize/mode-set.
    pub fn resize(&mut self, viewport: Rect) {
        self.runtime.resize(viewport);
        self.virtual_list
            .set_viewport(viewport.height.saturating_sub(130));
    }

    /// Синхронизирует accessibility/UI scale с desktop settings.
    pub fn set_scale(&mut self, scale_milli: u16) {
        let mut theme = self.runtime.theme();
        theme.scale_milli = scale_milli;
        self.runtime.set_theme(theme);
    }

    /// Pointer input уже нормализован window server'ом.
    pub fn pointer(&mut self, kind: PointerKind, x: i32, y: i32, scroll_y: i16) -> bool {
        if kind == PointerKind::Scroll {
            self.virtual_list.scroll_by(i64::from(scroll_y) * 28);
        }
        let mut event = PointerEvent::at(kind, x, y);
        event.scroll_y = scroll_y;
        let result = self.runtime.dispatch(InputEvent::Pointer(event));
        self.apply(result)
    }

    /// Клавиатурная навигация/активация.
    pub fn key(&mut self, key: Key, shift: bool) -> bool {
        let result = self.runtime.dispatch(InputEvent::Key(KeyEvent {
            key,
            pressed: true,
            modifiers: 0,
            shift,
        }));
        self.apply(result)
    }

    /// Полный repaint нужен только когда compositor восстановил/пересоздал
    /// поверхность. Обычные события оставляют локальный damage.
    pub fn draw(&mut self, framebuffer: &mut Framebuffer, full: bool) -> FrameResult<8> {
        if full {
            self.runtime.invalidate_all();
        }
        let mut backend = FramebufferBackend { framebuffer };
        self.runtime
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    fn apply(&mut self, result: DispatchResult) -> bool {
        match result.command {
            COMMAND_THEME => {
                let next = if self.runtime.theme().kind == ThemeKind::Dark {
                    Theme::light()
                } else {
                    Theme::dark()
                };
                self.runtime.set_theme(next);
            }
            COMMAND_COUNTER => {
                self.counter = self.counter.saturating_add(100).min(1000);
                let _ = self
                    .runtime
                    .set_content(self.progress, Content::Value(self.counter));
                let mut state = self
                    .runtime
                    .tree()
                    .get(self.counter_button)
                    .map_or(NodeState(0), |node| node.state);
                state.insert(NodeState::SELECTED);
                let _ = self.runtime.set_state(self.counter_button, state);
            }
            COMMAND_CHECK => {}
            _ => {}
        }
        // theme_switch хранится как часть состояния/inspector identity; чтение
        // предотвращает расхождение demo bindings при дальнейшем расширении.
        let _ = self.runtime.tree().get(self.theme_switch);
        result.changed || result.command != CommandId(0)
    }
}

/// Adapter к существующему CPU framebuffer. Только он знает реализацию
/// системных шрифтов; runtime остаётся renderer-agnostic.
struct FramebufferBackend<'a> {
    framebuffer: &'a mut Framebuffer,
}

impl RenderBackend for FramebufferBackend<'_> {
    fn fill(&mut self, rect: Rect, color: Color, clip: Rect) {
        self.framebuffer.fill_rect(rect.intersection(clip), color);
    }

    fn border(&mut self, rect: Rect, color: Color, width: u8, clip: Rect) {
        for inset in 0..u32::from(width.max(1)) {
            let inset_rect = Rect::new(
                rect.x.saturating_add(inset as i32),
                rect.y.saturating_add(inset as i32),
                rect.width.saturating_sub(inset * 2),
                rect.height.saturating_sub(inset * 2),
            );
            draw_clipped_border(self.framebuffer, inset_rect, color, clip);
        }
    }

    fn text(&mut self, rect: Rect, resource: ResourceId, color: Color, spec: FontSpec, _: Rect) {
        let mut style = font::FontStyle::sans(spec.size.clamp(10, 48));
        if spec.bold {
            style = style.bold();
        }
        if spec.italic {
            style = style.italic();
        }
        if spec.monospace {
            style = font::FontStyle::console(spec.size.clamp(10, 48));
        }
        font::draw_text(
            self.framebuffer,
            rect.x,
            rect.y,
            text_resource(resource),
            color,
            style,
        );
    }

    fn image(&mut self, rect: Rect, _: ResourceId, tint: Color, clip: Rect) {
        self.framebuffer.fill_rect(rect.intersection(clip), tint);
    }
}

fn draw_clipped_border(framebuffer: &mut Framebuffer, rect: Rect, color: Color, clip: Rect) {
    if rect.is_empty() {
        return;
    }
    for edge in [
        Rect::new(rect.x, rect.y, rect.width, 1),
        Rect::new(rect.x, rect.bottom().saturating_sub(1), rect.width, 1),
        Rect::new(rect.x, rect.y, 1, rect.height),
        Rect::new(rect.right().saturating_sub(1), rect.y, 1, rect.height),
    ] {
        framebuffer.fill_rect(edge.intersection(clip), color);
    }
}

fn text_resource(id: ResourceId) -> &'static str {
    match id.0 {
        1 => "SYSTEM UI · КОМПОНЕНТЫ",
        2 => "ОДНО ДЕРЕВО · RUST API И COMPILED RUI · CPU/GPU BACKENDS",
        3 => "НАСТРОЙКИ",
        4 => "СВЕТЛАЯ ТЕМА",
        5 => "ДИАГНОСТИКА ВКЛЮЧЕНА",
        6 => "RUSTOS DEVELOPER",
        7 => "ИМЯ РАЗРАБОТЧИКА",
        8 => "УВЕЛИЧИТЬ ПРОГРЕСС",
        9 => "VIRTUAL LIST · 50 000 ЭЛЕМЕНТОВ",
        10 => "00001  WINDOW + LAYOUT",
        11 => "00002  DISPLAY LIST",
        12 => "00003  DIRTY REGIONS",
        13 => "00004  POINTER CAPTURE",
        14 => "00005  KEYBOARD FOCUS",
        15 => "00006  ACCESSIBILITY TREE",
        16 => "... MATERIALIZED ТОЛЬКО ВИДИМЫЕ СТРОКИ",
        _ => "",
    }
}

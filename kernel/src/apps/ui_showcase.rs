//! Интерактивная галерея современной системной UI-платформы RustOS.
//!
//! Приложение намеренно использует только declarative component tree. Оно не
//! знает координат кнопок и не рисует controls напрямую: общий `system-ui`
//! runtime создаёт display list, а небольшой adapter исполняет его на CPU.

use crate::{
    apps::draw_system_ui_text,
    graphics::{Color, Framebuffer, Rect},
};
use rustos_system_assets::{IconKind, IconPack, AURORA_ICON_PACK};
use rustos_system_ui::{
    style_class, Align, CommandId, ComponentKind, Content, DispatchResult, Edges, FontSpec,
    FrameResult, InputEvent, Key, KeyEvent, LayoutSpec, Length, NodeId, NodeSpec, NodeState,
    PointerEvent, PointerKind, RenderBackend, ResourceId, Runtime, SelectionMode, SemanticRole,
    Theme, ThemeKind,
};

const COMMAND_THEME: CommandId = CommandId(1);
const COMMAND_COUNTER: CommandId = CommandId(2);
const COMMAND_CHECK: CommandId = CommandId(3);

const ICON_OVERVIEW: ResourceId = ResourceId(100);
const ICON_SUCCESS: ResourceId = ResourceId(101);
const ICON_INFO: ResourceId = ResourceId(102);
const ICON_SETTINGS: ResourceId = ResourceId(103);

/// Запасы памяти известны заранее и видимы inspector'у. Даже богатая demo
/// форма не может неожиданно аллоцировать heap внутри ядра.
type UiRuntime = Runtime<72, 256, 12>;

/// Stateful bindings демонстрации; внешний вид полностью остаётся в tree.
pub struct UiShowcase {
    runtime: UiRuntime,
    progress: NodeId,
    counter_button: NodeId,
    theme_switch: NodeId,
    check_box: NodeId,
    counter: u16,
}

impl UiShowcase {
    /// Строит responsive-форму: при узком окне горизонтальные группы сами
    /// переходят в Column без отдельного набора координат.
    pub fn new(viewport: Rect) -> Self {
        let mut runtime = UiRuntime::new(viewport, Theme::light());
        let mut progress = NodeId::NONE;
        let mut counter_button = NodeId::NONE;
        let mut theme_switch = NodeId::NONE;
        let mut check_box = NodeId::NONE;
        let mut list_view = NodeId::NONE;

        {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let page = add_container(
                &mut ui,
                root,
                ComponentKind::Column,
                LayoutSpec {
                    width: Length::Fill(1),
                    height: Length::Fill(1),
                    padding: Edges::all(16),
                    gap: 10,
                    ..LayoutSpec::default()
                },
                style_class::DEFAULT,
            );

            if let Some(page) = page {
                build_header(&mut ui, page);

                let top = add_container(
                    &mut ui,
                    page,
                    ComponentKind::Row,
                    LayoutSpec {
                        width: Length::Fill(1),
                        height: Length::Fill(1),
                        min_height: 245,
                        gap: 10,
                        container_breakpoint: 760,
                        ..LayoutSpec::default()
                    },
                    style_class::DEFAULT,
                );
                if let Some(top) = top {
                    let buttons = add_card(&mut ui, top, 1);
                    if let Some(card) = buttons {
                        add_heading(&mut ui, card, ResourceId(3));
                        counter_button = add_button(
                            &mut ui,
                            card,
                            ResourceId(4),
                            COMMAND_COUNTER,
                            style_class::PRIMARY,
                        );
                        let _ = add_button(
                            &mut ui,
                            card,
                            ResourceId(5),
                            CommandId(0),
                            style_class::DEFAULT,
                        );
                        let _ = add_button(
                            &mut ui,
                            card,
                            ResourceId(6),
                            CommandId(0),
                            style_class::GHOST,
                        );
                        let mut disabled = button_spec(ResourceId(7), CommandId(0));
                        disabled.state = NodeState::DISABLED;
                        let _ = ui.component(card, disabled);
                    }

                    let controls = add_card(&mut ui, top, 1);
                    if let Some(card) = controls {
                        add_heading(&mut ui, card, ResourceId(8));
                        theme_switch = ui
                            .switch(card, ResourceId(9), COMMAND_THEME, line(36))
                            .unwrap_or(NodeId::NONE);
                        let mut checked = NodeSpec::new(ComponentKind::CheckBox);
                        checked.layout = line(34);
                        checked.content = Content::Text(ResourceId(10));
                        checked.command = COMMAND_CHECK;
                        checked.role = SemanticRole::CheckBox;
                        checked.accessible_name = ResourceId(10);
                        checked.state = NodeState::CHECKED;
                        check_box = ui.component(card, checked).unwrap_or(NodeId::NONE);
                        let mut radio = NodeSpec::new(ComponentKind::RadioButton);
                        radio.layout = line(34);
                        radio.content = Content::Text(ResourceId(11));
                        radio.role = SemanticRole::RadioButton;
                        radio.accessible_name = ResourceId(11);
                        radio.state = NodeState::CHECKED;
                        let _ = ui.component(card, radio);
                    }

                    let fields = add_card(&mut ui, top, 1);
                    if let Some(card) = fields {
                        add_heading(&mut ui, card, ResourceId(12));
                        if let Some(tabs) = add_container(
                            &mut ui,
                            card,
                            ComponentKind::Row,
                            line_with_gap(34, 4),
                            style_class::DEFAULT,
                        ) {
                            for (index, resource) in
                                [ResourceId(30), ResourceId(31)].into_iter().enumerate()
                            {
                                let mut tab = NodeSpec::new(ComponentKind::TabView);
                                tab.layout = LayoutSpec {
                                    width: Length::Fill(1),
                                    height: Length::Fill(1),
                                    ..LayoutSpec::default()
                                };
                                tab.content = Content::Text(resource);
                                tab.accessible_name = resource;
                                if index == 0 {
                                    tab.state = NodeState::SELECTED;
                                }
                                let _ = ui.component(tabs, tab);
                            }
                        }
                        let _ = ui.text_field(card, ResourceId(13), ResourceId(14), line(40));
                        let mut slider = NodeSpec::new(ComponentKind::Slider);
                        slider.layout = line(16);
                        slider.content = Content::Value(620);
                        slider.accessible_name = ResourceId(15);
                        let _ = ui.component(card, slider);
                        add_caption(&mut ui, card, ResourceId(15));
                        progress = ui.progress(card, 350, line(14)).unwrap_or(NodeId::NONE);
                        add_caption(&mut ui, card, ResourceId(16));
                    }
                }

                let bottom = add_container(
                    &mut ui,
                    page,
                    ComponentKind::Row,
                    LayoutSpec {
                        width: Length::Fill(1),
                        height: Length::Fill(1),
                        min_height: 210,
                        gap: 10,
                        container_breakpoint: 760,
                        ..LayoutSpec::default()
                    },
                    style_class::DEFAULT,
                );
                if let Some(bottom) = bottom {
                    list_view = build_list_card(&mut ui, bottom);
                    build_status_card(&mut ui, bottom);
                }
            }
        }

        if !list_view.is_none() {
            let _ = runtime.configure_list_view(list_view, 50_000, 30, SelectionMode::Extended);
        }
        Self {
            runtime,
            progress,
            counter_button,
            theme_switch,
            check_box,
            counter: 350,
        }
    }

    pub fn resize(&mut self, viewport: Rect) {
        self.runtime.resize(viewport);
    }

    pub fn set_scale(&mut self, scale_milli: u16) {
        let mut theme = self.runtime.theme();
        theme.scale_milli = scale_milli;
        self.runtime.set_theme(theme);
    }

    pub fn pointer(&mut self, kind: PointerKind, x: i32, y: i32, scroll_y: i16) -> bool {
        let mut event = PointerEvent::at(kind, x, y);
        event.scroll_y = scroll_y;
        event.scroll_unit = rustos_system_ui::ScrollUnit::Line;
        let result = self.runtime.dispatch(InputEvent::Pointer(event));
        self.apply(result)
    }

    pub fn key(&mut self, key: Key, shift: bool) -> bool {
        let result = self.runtime.dispatch(InputEvent::Key(KeyEvent {
            key,
            pressed: true,
            modifiers: 0,
            shift,
        }));
        self.apply(result)
    }

    pub fn draw(&mut self, framebuffer: &mut Framebuffer, full: bool) -> FrameResult<12> {
        if full {
            self.runtime.invalidate_all();
        }
        let mut backend = FramebufferBackend {
            framebuffer,
            icons: AURORA_ICON_PACK,
        };
        self.runtime
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    fn apply(&mut self, result: DispatchResult) -> bool {
        match result.command {
            COMMAND_THEME => {
                let scale = self.runtime.theme().scale_milli;
                let mut next = if self.runtime.theme().kind == ThemeKind::Dark {
                    Theme::light()
                } else {
                    Theme::dark()
                };
                next.scale_milli = scale;
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
            COMMAND_CHECK => {
                // Dispatcher уже переключил CHECKED; binding оставляет это
                // состояние в tree и лишь подтверждает стабильный NodeId.
                let _ = self.runtime.tree().get(self.check_box);
            }
            _ => {}
        }
        let _ = self.runtime.tree().get(self.theme_switch);
        result.changed || result.command != CommandId(0)
    }
}

fn build_header<const N: usize>(ui: &mut rustos_system_ui::UiBuilder<'_, N>, page: NodeId) {
    let Some(header) = add_container(
        ui,
        page,
        ComponentKind::Row,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(54),
            gap: 12,
            align: Align::Center,
            ..LayoutSpec::default()
        },
        style_class::DEFAULT,
    ) else {
        return;
    };
    let _ = ui.image(
        header,
        ICON_OVERVIEW,
        ResourceId(1),
        LayoutSpec {
            width: Length::Px(42),
            height: Length::Px(42),
            align: Align::Center,
            ..LayoutSpec::default()
        },
    );
    let Some(texts) = add_container(
        ui,
        header,
        ComponentKind::Column,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            gap: 1,
            ..LayoutSpec::default()
        },
        style_class::DEFAULT,
    ) else {
        return;
    };
    add_heading(ui, texts, ResourceId(1));
    add_caption(ui, texts, ResourceId(2));
}

fn build_list_card<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
) -> NodeId {
    let Some(card) = add_card(ui, parent, 2) else {
        return NodeId::NONE;
    };
    add_heading(ui, card, ResourceId(17));
    let Ok(list) = ui.list_view(
        card,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            padding: Edges::all(6),
            gap: 3,
            ..LayoutSpec::default()
        },
    ) else {
        return NodeId::NONE;
    };
    // Десять recyclable delegates покрывают viewport + overscan. Logical
    // source при этом содержит 50 000 items и не создаёт 50 000 nodes.
    for index in 0..10 {
        let resource = 18 + index % 5;
        let mut row = NodeSpec::new(ComponentKind::Text);
        row.layout = line(29);
        row.content = Content::Text(ResourceId(resource));
        row.role = SemanticRole::ListItem;
        if resource == 18 {
            row.style = style_class::SUBTLE;
        }
        let _ = ui.component(list, row);
    }
    list
}

fn build_status_card<const N: usize>(ui: &mut rustos_system_ui::UiBuilder<'_, N>, parent: NodeId) {
    let Some(card) = add_card(ui, parent, 1) else {
        return;
    };
    add_heading(ui, card, ResourceId(23));
    for (icon, title, caption) in [
        (ICON_SUCCESS, ResourceId(24), ResourceId(25)),
        (ICON_INFO, ResourceId(26), ResourceId(27)),
        (ICON_SETTINGS, ResourceId(28), ResourceId(29)),
    ] {
        let Some(row) = add_container(
            ui,
            card,
            ComponentKind::Row,
            LayoutSpec {
                width: Length::Fill(1),
                height: Length::Fill(1),
                min_height: 46,
                padding: Edges::all(5),
                gap: 8,
                align: Align::Center,
                ..LayoutSpec::default()
            },
            style_class::SUBTLE,
        ) else {
            continue;
        };
        let _ = ui.image(
            row,
            icon,
            title,
            LayoutSpec {
                width: Length::Px(28),
                height: Length::Px(28),
                align: Align::Center,
                ..LayoutSpec::default()
            },
        );
        if let Some(texts) = add_container(
            ui,
            row,
            ComponentKind::Column,
            LayoutSpec {
                width: Length::Fill(1),
                height: Length::Fill(1),
                ..LayoutSpec::default()
            },
            style_class::DEFAULT,
        ) {
            add_text(ui, texts, title, 22, style_class::DEFAULT);
            add_text(ui, texts, caption, 20, style_class::CAPTION);
        }
    }
}

fn add_card<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    weight: u16,
) -> Option<NodeId> {
    add_container(
        ui,
        parent,
        ComponentKind::Panel,
        LayoutSpec {
            width: Length::Fill(weight),
            height: Length::Fill(1),
            min_width: 220,
            padding: Edges::all(14),
            gap: 8,
            ..LayoutSpec::default()
        },
        style_class::CARD,
    )
}

fn add_container<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    kind: ComponentKind,
    layout: LayoutSpec,
    style: u16,
) -> Option<NodeId> {
    let mut spec = NodeSpec::new(kind);
    spec.layout = layout;
    spec.style = style;
    spec.role = SemanticRole::Group;
    ui.component(parent, spec).ok()
}

fn add_heading<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    resource: ResourceId,
) {
    add_text(ui, parent, resource, 26, style_class::HEADING);
}

fn add_caption<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    resource: ResourceId,
) {
    add_text(ui, parent, resource, 21, style_class::CAPTION);
}

fn add_text<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    resource: ResourceId,
    height: u16,
    style: u16,
) {
    let mut spec = NodeSpec::new(ComponentKind::Text);
    spec.layout = line(height);
    spec.content = Content::Text(resource);
    spec.role = SemanticRole::Text;
    spec.style = style;
    let _ = ui.component(parent, spec);
}

fn add_button<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    label: ResourceId,
    command: CommandId,
    style: u16,
) -> NodeId {
    let mut spec = button_spec(label, command);
    spec.style = style;
    ui.component(parent, spec).unwrap_or(NodeId::NONE)
}

fn button_spec(label: ResourceId, command: CommandId) -> NodeSpec {
    let mut spec = NodeSpec::new(ComponentKind::Button);
    spec.layout = line(40);
    spec.content = Content::Text(label);
    spec.command = command;
    spec.role = SemanticRole::Button;
    spec.accessible_name = label;
    spec
}

const fn line(height: u16) -> LayoutSpec {
    LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(height),
        min_width: 0,
        min_height: 0,
        max_width: 0,
        max_height: 0,
        padding: Edges::all(0),
        gap: 0,
        align: Align::Stretch,
        grid_columns: 1,
        container_breakpoint: 0,
    }
}

const fn line_with_gap(height: u16, gap: u16) -> LayoutSpec {
    let mut layout = line(height);
    layout.gap = gap;
    layout
}

struct FramebufferBackend<'a> {
    framebuffer: &'a mut Framebuffer,
    icons: IconPack,
}

impl RenderBackend for FramebufferBackend<'_> {
    fn shadow(&mut self, rect: Rect, radius: u8, color: Color, clip: Rect) {
        self.framebuffer.surface_shadow(rect, radius, color, clip);
    }

    fn fill(&mut self, rect: Rect, color: Color, clip: Rect) {
        self.framebuffer.fill_rect(rect.intersection(clip), color);
    }

    fn border(&mut self, rect: Rect, color: Color, width: u8, clip: Rect) {
        self.framebuffer
            .rounded_border_clipped(rect, 0, width, color, clip);
    }

    fn rounded_fill(&mut self, rect: Rect, color: Color, radius: u8, clip: Rect) {
        self.framebuffer
            .fill_rounded_rect_clipped(rect, radius, color, clip);
    }

    fn rounded_border(&mut self, rect: Rect, color: Color, width: u8, radius: u8, clip: Rect) {
        self.framebuffer
            .rounded_border_clipped(rect, radius, width, color, clip);
    }

    fn text(&mut self, rect: Rect, resource: ResourceId, color: Color, spec: FontSpec, clip: Rect) {
        if !rect.intersection(clip).is_empty() {
            draw_system_ui_text(self.framebuffer, rect, text_resource(resource), color, spec);
        }
    }

    fn image(&mut self, rect: Rect, resource: ResourceId, _: Color, clip: Rect) {
        if rect.intersection(clip).is_empty() {
            return;
        }
        let kind = match resource {
            ICON_OVERVIEW => IconKind::Grid,
            ICON_SUCCESS => IconKind::Success,
            ICON_INFO => IconKind::Info,
            ICON_SETTINGS => IconKind::Settings,
            _ => return,
        };
        self.icons.draw(self.framebuffer, kind, rect);
    }
}

fn text_resource(id: ResourceId) -> &'static str {
    match id.0 {
        1 => "Библиотека компонентов",
        2 => "Единый API · светлая и тёмная темы · CPU/GPU backends",
        3 => "Кнопки",
        4 => "Увеличить прогресс",
        5 => "Вторичная кнопка",
        6 => "Прозрачная кнопка",
        7 => "Недоступно",
        8 => "Элементы выбора",
        9 => "Тёмная тема",
        10 => "Диагностика включена",
        11 => "Основной вариант",
        12 => "Поля и прогресс",
        13 => "RustOS developer",
        14 => "Имя разработчика",
        15 => "Производительность · 62%",
        16 => "Готовность платформы · 35%",
        17 => "Виртуальный список · 50 000 строк",
        18 => "Обзор компонентов",
        19 => "Сетка и адаптивный layout",
        20 => "Инкрементальная перерисовка",
        21 => "Фокус и клавиатурная навигация",
        22 => "Accessibility tree",
        23 => "Состояние системы",
        24 => "Renderer готов",
        25 => "Damage работает",
        26 => "Единый дизайн",
        27 => "Общие токены приложений",
        28 => "Пакеты ресурсов",
        29 => "Иконки можно заменять",
        30 => "Основное",
        31 => "Стиль",
        _ => "",
    }
}

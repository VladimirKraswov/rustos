//! Отдельное приложение «Свойства рабочего стола».
//!
//! Приложение владеет только component tree и выбранными controls. Реальное
//! переключение видеорежима, цветности и общесистемного масштаба выполняет
//! desktop/window service после получения типизированной команды. Поэтому
//! UI-клиент не получает прямой доступ к framebuffer или display device.

use crate::{
    apps::draw_system_ui_text,
    graphics::{Color, Framebuffer, Rect},
};
use rustos_system_assets::{wallpaper, WallpaperId};
use rustos_system_ui::{
    style_class, CommandId, ComponentKind, Content, DispatchResult, Edges, FontSpec, FrameResult,
    InputEvent, Key, KeyEvent, LayoutSpec, Length, NodeId, NodeSpec, NodeState, PointerEvent,
    PointerKind, RenderBackend, ResourceId, Runtime, ScrollUnit, SemanticRole, Theme,
};
use rustos_video::{ColorMode, DisplayMode};

const COMMAND_COLOR_24: CommandId = CommandId(4);
const COMMAND_COLOR_16: CommandId = CommandId(5);
const COMMAND_COLOR_GRAY: CommandId = CommandId(6);
const COMMAND_WALLPAPER_SPRING: CommandId = CommandId(7);
const COMMAND_WALLPAPER_AUTUMN: CommandId = CommandId(8);
const COMMAND_WALLPAPER_WINTER: CommandId = CommandId(9);
const COMMAND_SCALE_100: CommandId = CommandId(10);
const COMMAND_SCALE_125: CommandId = CommandId(11);
const COMMAND_SCALE_150: CommandId = CommandId(12);

const TEXT_TITLE: ResourceId = ResourceId(1);
const TEXT_SUBTITLE: ResourceId = ResourceId(2);
const IMAGE_WALLPAPER: ResourceId = ResourceId(3);
const TEXT_RESOLUTION: ResourceId = ResourceId(4);
const TEXT_COLOR: ResourceId = ResourceId(8);
const TEXT_COLOR_24: ResourceId = ResourceId(9);
const TEXT_COLOR_16: ResourceId = ResourceId(10);
const TEXT_COLOR_GRAY: ResourceId = ResourceId(11);
const TEXT_WALLPAPER: ResourceId = ResourceId(12);
const TEXT_SPRING: ResourceId = ResourceId(13);
const TEXT_AUTUMN: ResourceId = ResourceId(14);
const TEXT_WINTER: ResourceId = ResourceId(15);
const TEXT_SCALE: ResourceId = ResourceId(16);
const TEXT_SCALE_100: ResourceId = ResourceId(17);
const TEXT_SCALE_125: ResourceId = ResourceId(18);
const TEXT_SCALE_150: ResourceId = ResourceId(19);

const RESOLUTION_COMMAND_BASE: u32 = 100;
const RESOLUTION_TEXT_BASE: u32 = 100;
const RESOLUTION_ROW_HEIGHT: u16 = 36;
const RESOLUTION_LIST_HEIGHT: u16 = 86;

/// Стандартные режимы, которые умеет создать virtio-gpu 2D scanout. Сначала
/// идут комфортные широкоформатные варианты, чтобы они были доступны без
/// прокрутки; legacy 4:3 сохранены внизу для совместимости и тестов.
const RESOLUTION_OPTIONS: [ResolutionOption; 24] = [
    resolution(1280, 800, 0),
    resolution(1280, 720, 1),
    resolution(1366, 768, 2),
    resolution(1440, 810, 3),
    resolution(1440, 900, 4),
    resolution(1600, 900, 5),
    resolution(1680, 1050, 6),
    resolution(1920, 1080, 7),
    resolution(1920, 1200, 8),
    resolution(2048, 1152, 9),
    resolution(2560, 1080, 10),
    resolution(2560, 1440, 11),
    resolution(2560, 1600, 12),
    resolution(2880, 1800, 13),
    resolution(3200, 1800, 14),
    resolution(3440, 1440, 15),
    resolution(3840, 1600, 16),
    resolution(3840, 2160, 17),
    resolution(1152, 648, 18),
    resolution(1024, 600, 19),
    resolution(1024, 768, 20),
    resolution(1280, 1024, 21),
    resolution(800, 600, 22),
    resolution(640, 480, 23),
];
const RESOLUTION_COUNT: usize = RESOLUTION_OPTIONS.len();

#[derive(Clone, Copy)]
struct ResolutionOption {
    width: u32,
    height: u32,
    label: ResourceId,
    command: CommandId,
}

const fn resolution(width: u32, height: u32, index: u32) -> ResolutionOption {
    ResolutionOption {
        width,
        height,
        label: ResourceId(RESOLUTION_TEXT_BASE + index),
        command: CommandId(RESOLUTION_COMMAND_BASE + index),
    }
}

type SettingsRuntime = Runtime<72, 256, 12>;

/// Снимок состояния, которым desktop service синхронизирует controls после
/// успешной операции или честного отказа display driver'а.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopSettingsSnapshot {
    pub width: u32,
    pub height: u32,
    pub color: ColorMode,
    pub wallpaper: WallpaperId,
    pub ui_scale_milli: u16,
}

/// Команды приложения. Они не содержат указателей и позже напрямую станут
/// payload capability IPC между settings client и desktop/display services.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopSettingsAction {
    None,
    SetResolution { width: u32, height: u32 },
    SetColor(ColorMode),
    SetWallpaper(WallpaperId),
    SetUiScale(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopSettingsInput {
    pub action: DesktopSettingsAction,
    pub changed: bool,
    pub consumed: bool,
}

pub struct DesktopSettings {
    runtime: SettingsRuntime,
    snapshot: DesktopSettingsSnapshot,
    resolution: [NodeId; RESOLUTION_COUNT],
    colors: [NodeId; 3],
    wallpapers: [NodeId; 3],
    scales: [NodeId; 3],
}

impl DesktopSettings {
    pub fn new(
        viewport: Rect,
        snapshot: DesktopSettingsSnapshot,
        available_modes: &[DisplayMode],
        recommended_mode: DisplayMode,
    ) -> Self {
        let mut runtime = SettingsRuntime::new(viewport, theme(snapshot.ui_scale_milli));
        let mut resolution = [NodeId::NONE; RESOLUTION_COUNT];
        let mut colors = [NodeId::NONE; 3];
        let mut wallpapers = [NodeId::NONE; 3];
        let mut scales = [NodeId::NONE; 3];
        build_tree(
            &mut runtime,
            &mut resolution,
            &mut colors,
            &mut wallpapers,
            &mut scales,
            available_modes,
            recommended_mode,
        );
        let mut result = Self {
            runtime,
            snapshot,
            resolution,
            colors,
            wallpapers,
            scales,
        };
        result.sync(snapshot);
        result
    }

    pub fn resize(&mut self, viewport: Rect) {
        self.runtime.resize(viewport);
    }

    pub fn sync(&mut self, snapshot: DesktopSettingsSnapshot) {
        let repaint_preview = self.snapshot.wallpaper != snapshot.wallpaper;
        self.snapshot = snapshot;
        let mut current_theme = self.runtime.theme();
        if current_theme.scale_milli != snapshot.ui_scale_milli {
            current_theme.scale_milli = snapshot.ui_scale_milli;
            self.runtime.set_theme(current_theme);
        }
        let mut selected_resolution = [false; RESOLUTION_COUNT];
        for (selected, option) in selected_resolution.iter_mut().zip(RESOLUTION_OPTIONS) {
            *selected = snapshot.width == option.width && snapshot.height == option.height;
        }
        select_group(&mut self.runtime, self.resolution, selected_resolution);
        select_group(
            &mut self.runtime,
            self.colors,
            [
                snapshot.color == ColorMode::TrueColor24,
                snapshot.color == ColorMode::HighColor16,
                snapshot.color == ColorMode::Grayscale8,
            ],
        );
        select_group(
            &mut self.runtime,
            self.wallpapers,
            [
                snapshot.wallpaper == WallpaperId::SpringRiver,
                snapshot.wallpaper == WallpaperId::AutumnRiver,
                snapshot.wallpaper == WallpaperId::WinterField,
            ],
        );
        select_group(
            &mut self.runtime,
            self.scales,
            [
                snapshot.ui_scale_milli == 1_000,
                snapshot.ui_scale_milli == 1_250,
                snapshot.ui_scale_milli == 1_500,
            ],
        );
        if repaint_preview {
            self.runtime.invalidate_all();
        }
    }

    pub fn pointer(&mut self, kind: PointerKind, x: i32, y: i32) -> DesktopSettingsInput {
        self.dispatch(InputEvent::Pointer(PointerEvent::at(kind, x, y)))
    }

    /// Прокрутка приходит в логических координатах окна и проходит тот же
    /// nested-scroll dispatcher, что ListView Проводника и галереи.
    pub fn scroll(&mut self, x: i32, y: i32, wheel_x: i16, wheel_y: i16) -> bool {
        let mut pointer = PointerEvent::at(PointerKind::Scroll, x, y);
        pointer.scroll_x = wheel_x;
        pointer.scroll_y = wheel_y;
        pointer.scroll_unit = ScrollUnit::Line;
        self.runtime.dispatch(InputEvent::Pointer(pointer)).changed
    }

    pub fn key(&mut self, key: Key, shift: bool) -> DesktopSettingsInput {
        self.dispatch(InputEvent::Key(KeyEvent {
            key,
            pressed: true,
            modifiers: 0,
            shift,
        }))
    }

    pub fn draw(&mut self, framebuffer: &mut Framebuffer, full: bool) -> FrameResult<12> {
        if full {
            self.runtime.invalidate_all();
        }
        let mut backend = SettingsBackend {
            framebuffer,
            wallpaper: self.snapshot.wallpaper,
        };
        self.runtime
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    fn dispatch(&mut self, input: InputEvent) -> DesktopSettingsInput {
        let result = self.runtime.dispatch(input);
        DesktopSettingsInput {
            action: action_for(result),
            changed: result.changed,
            consumed: result.consumed,
        }
    }
}

fn build_tree(
    runtime: &mut SettingsRuntime,
    resolution: &mut [NodeId; RESOLUTION_COUNT],
    colors: &mut [NodeId; 3],
    wallpapers: &mut [NodeId; 3],
    scales: &mut [NodeId; 3],
    available_modes: &[DisplayMode],
    recommended_mode: DisplayMode,
) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let mut page = NodeSpec::new(ComponentKind::Column);
    page.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        padding: Edges::all(18),
        gap: 8,
        ..LayoutSpec::default()
    };
    let Ok(page) = ui.component(root, page) else {
        return;
    };
    add_text(&mut ui, page, TEXT_TITLE, 34, style_class::HEADING);
    add_text(&mut ui, page, TEXT_SUBTITLE, 24, style_class::CAPTION);
    let _ = ui.image(
        page,
        IMAGE_WALLPAPER,
        TEXT_WALLPAPER,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(60),
            ..LayoutSpec::default()
        },
    );
    add_text(&mut ui, page, TEXT_RESOLUTION, 24, style_class::HEADING);
    *resolution = add_resolution_choices(&mut ui, page, available_modes, recommended_mode);
    add_text(&mut ui, page, TEXT_COLOR, 24, style_class::HEADING);
    *colors = add_choices(
        &mut ui,
        page,
        [
            (TEXT_COLOR_24, COMMAND_COLOR_24),
            (TEXT_COLOR_16, COMMAND_COLOR_16),
            (TEXT_COLOR_GRAY, COMMAND_COLOR_GRAY),
        ],
    );
    add_text(&mut ui, page, TEXT_WALLPAPER, 24, style_class::HEADING);
    *wallpapers = add_choices(
        &mut ui,
        page,
        [
            (TEXT_SPRING, COMMAND_WALLPAPER_SPRING),
            (TEXT_AUTUMN, COMMAND_WALLPAPER_AUTUMN),
            (TEXT_WINTER, COMMAND_WALLPAPER_WINTER),
        ],
    );
    add_text(&mut ui, page, TEXT_SCALE, 24, style_class::HEADING);
    *scales = add_choices(
        &mut ui,
        page,
        [
            (TEXT_SCALE_100, COMMAND_SCALE_100),
            (TEXT_SCALE_125, COMMAND_SCALE_125),
            (TEXT_SCALE_150, COMMAND_SCALE_150),
        ],
    );
}

fn add_resolution_choices<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    available_modes: &[DisplayMode],
    recommended_mode: DisplayMode,
) -> [NodeId; RESOLUTION_COUNT] {
    let Ok(list) = ui.list_view(
        parent,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(RESOLUTION_LIST_HEIGHT),
            padding: Edges::symmetric(4, 3),
            gap: 4,
            ..LayoutSpec::default()
        },
    ) else {
        return [NodeId::NONE; RESOLUTION_COUNT];
    };

    let mut result = [NodeId::NONE; RESOLUTION_COUNT];
    let recommended_index = RESOLUTION_OPTIONS.iter().position(|option| {
        option.width == recommended_mode.width && option.height == recommended_mode.height
    });
    for order in 0..RESOLUTION_COUNT {
        // Рекомендованный EDID/startup mode всегда первый. NodeId при этом
        // сохраняется в slot исходного option, поэтому sync не зависит от
        // визуальной сортировки.
        let index = match recommended_index {
            Some(recommended) if order == 0 => recommended,
            Some(recommended) if order <= recommended => order - 1,
            _ => order,
        };
        let option = RESOLUTION_OPTIONS[index];
        let mut spec = NodeSpec::new(ComponentKind::Button);
        spec.layout = LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(RESOLUTION_ROW_HEIGHT),
            ..LayoutSpec::default()
        };
        spec.content = Content::Text(option.label);
        spec.command = option.command;
        spec.role = SemanticRole::ListItem;
        spec.accessible_name = option.label;
        if Some(index) == recommended_index {
            spec.style = style_class::PRIMARY;
        }
        if !mode_available(option, available_modes) {
            spec.state.insert(NodeState::DISABLED);
        }
        result[index] = ui.component(list, spec).unwrap_or(NodeId::NONE);
    }
    result
}

fn mode_available(option: ResolutionOption, available_modes: &[DisplayMode]) -> bool {
    available_modes
        .iter()
        .any(|mode| mode.width == option.width && mode.height == option.height)
}

fn add_text<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    resource: ResourceId,
    height: u16,
    style: u16,
) {
    let mut spec = NodeSpec::new(ComponentKind::Text);
    spec.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(height),
        ..LayoutSpec::default()
    };
    spec.content = Content::Text(resource);
    spec.role = if style == style_class::HEADING {
        SemanticRole::Heading
    } else {
        SemanticRole::Text
    };
    spec.style = style;
    let _ = ui.component(parent, spec);
}

fn add_choices<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    choices: [(ResourceId, CommandId); 3],
) -> [NodeId; 3] {
    let mut row = NodeSpec::new(ComponentKind::Row);
    row.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(46),
        gap: 8,
        ..LayoutSpec::default()
    };
    let Ok(row) = ui.component(parent, row) else {
        return [NodeId::NONE; 3];
    };
    let mut result = [NodeId::NONE; 3];
    for (index, (label, command)) in choices.into_iter().enumerate() {
        result[index] = ui
            .button(
                row,
                label,
                command,
                LayoutSpec {
                    width: Length::Fill(1),
                    height: Length::Fill(1),
                    ..LayoutSpec::default()
                },
            )
            .unwrap_or(NodeId::NONE);
    }
    result
}

fn select_group<const N: usize>(
    runtime: &mut SettingsRuntime,
    nodes: [NodeId; N],
    selected: [bool; N],
) {
    for (node, active) in nodes.into_iter().zip(selected) {
        let Some(current) = runtime.tree().get(node).map(|value| value.state) else {
            continue;
        };
        let mut next = current;
        if active {
            next.insert(NodeState::SELECTED);
        } else {
            next.remove(NodeState::SELECTED);
        }
        if next != current {
            let _ = runtime.set_state(node, next);
        }
    }
}

fn action_for(result: DispatchResult) -> DesktopSettingsAction {
    if let Some(option) = RESOLUTION_OPTIONS
        .iter()
        .find(|option| option.command == result.command)
    {
        return DesktopSettingsAction::SetResolution {
            width: option.width,
            height: option.height,
        };
    }
    match result.command {
        COMMAND_COLOR_24 => DesktopSettingsAction::SetColor(ColorMode::TrueColor24),
        COMMAND_COLOR_16 => DesktopSettingsAction::SetColor(ColorMode::HighColor16),
        COMMAND_COLOR_GRAY => DesktopSettingsAction::SetColor(ColorMode::Grayscale8),
        COMMAND_WALLPAPER_SPRING => DesktopSettingsAction::SetWallpaper(WallpaperId::SpringRiver),
        COMMAND_WALLPAPER_AUTUMN => DesktopSettingsAction::SetWallpaper(WallpaperId::AutumnRiver),
        COMMAND_WALLPAPER_WINTER => DesktopSettingsAction::SetWallpaper(WallpaperId::WinterField),
        COMMAND_SCALE_100 => DesktopSettingsAction::SetUiScale(1_000),
        COMMAND_SCALE_125 => DesktopSettingsAction::SetUiScale(1_250),
        COMMAND_SCALE_150 => DesktopSettingsAction::SetUiScale(1_500),
        _ => DesktopSettingsAction::None,
    }
}

fn theme(scale_milli: u16) -> Theme {
    let mut theme = Theme::light();
    theme.scale_milli = scale_milli;
    theme
}

struct SettingsBackend<'a> {
    framebuffer: &'a mut Framebuffer,
    wallpaper: WallpaperId,
}

impl RenderBackend for SettingsBackend<'_> {
    fn shadow(&mut self, rect: Rect, radius: u8, color: Color, clip: Rect) {
        self.framebuffer.surface_shadow(rect, radius, color, clip);
    }

    fn fill(&mut self, rect: Rect, color: Color, clip: Rect) {
        self.framebuffer.fill_rect(rect.intersection(clip), color);
    }

    fn border(&mut self, rect: Rect, color: Color, width: u8, clip: Rect) {
        for inset in 0..u32::from(width.max(1)) {
            let current = Rect::new(
                rect.x.saturating_add(inset as i32),
                rect.y.saturating_add(inset as i32),
                rect.width.saturating_sub(inset * 2),
                rect.height.saturating_sub(inset * 2),
            );
            draw_border(self.framebuffer, current, color, clip);
        }
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
        if rect.intersection(clip).is_empty() {
            return;
        }
        draw_system_ui_text(
            self.framebuffer,
            rect,
            text_resource(resource),
            color,
            spec,
            clip,
        );
    }

    fn image(&mut self, rect: Rect, resource: ResourceId, _: Color, clip: Rect) {
        if resource == IMAGE_WALLPAPER && !rect.intersection(clip).is_empty() {
            self.framebuffer
                .draw_wallpaper(rect, wallpaper(self.wallpaper));
            self.framebuffer
                .mask_rounded_corners(rect, 12, Color::rgb(244, 247, 252));
            self.framebuffer
                .rounded_border(rect, 12, 1, Color::rgb(205, 216, 232));
        }
    }
}

fn draw_border(framebuffer: &mut Framebuffer, rect: Rect, color: Color, clip: Rect) {
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

fn text_resource(resource: ResourceId) -> &'static str {
    match resource {
        TEXT_TITLE => "Параметры рабочего стола",
        TEXT_SUBTITLE => "Первый режим рекомендован и выбирается автоматически",
        TEXT_RESOLUTION => "Разрешение экрана",
        ResourceId(100) => "1280 × 800 · 16:10",
        ResourceId(101) => "1280 × 720 · 16:9",
        ResourceId(102) => "1366 × 768 · 16:9",
        ResourceId(103) => "1440 × 810 · 16:9",
        ResourceId(104) => "1440 × 900 · 16:10",
        ResourceId(105) => "1600 × 900 · 16:9",
        ResourceId(106) => "1680 × 1050 · 16:10",
        ResourceId(107) => "1920 × 1080 · Full HD",
        ResourceId(108) => "1920 × 1200 · 16:10",
        ResourceId(109) => "2048 × 1152 · 16:9",
        ResourceId(110) => "2560 × 1080 · UltraWide",
        ResourceId(111) => "2560 × 1440 · QHD",
        ResourceId(112) => "2560 × 1600 · 16:10",
        ResourceId(113) => "2880 × 1800 · Retina 16:10",
        ResourceId(114) => "3200 × 1800 · QHD+",
        ResourceId(115) => "3440 × 1440 · UltraWide",
        ResourceId(116) => "3840 × 1600 · UltraWide",
        ResourceId(117) => "3840 × 2160 · 4K UHD",
        ResourceId(118) => "1152 × 648 · 16:9",
        ResourceId(119) => "1024 × 600 · Wide",
        ResourceId(120) => "1024 × 768 · 4:3",
        ResourceId(121) => "1280 × 1024 · 5:4",
        ResourceId(122) => "800 × 600 · 4:3",
        ResourceId(123) => "640 × 480 · 4:3",
        TEXT_COLOR => "Глубина цвета",
        TEXT_COLOR_24 => "24 бита",
        TEXT_COLOR_16 => "16 бит",
        TEXT_COLOR_GRAY => "Серый · 8 бит",
        TEXT_WALLPAPER => "Обои",
        TEXT_SPRING => "Весна",
        TEXT_AUTUMN => "Осень",
        TEXT_WINTER => "Зима",
        TEXT_SCALE => "Размер системного шрифта",
        TEXT_SCALE_100 => "100%",
        TEXT_SCALE_125 => "125%",
        TEXT_SCALE_150 => "150%",
        _ => "",
    }
}

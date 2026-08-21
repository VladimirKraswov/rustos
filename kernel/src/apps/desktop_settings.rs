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
    PointerKind, RenderBackend, ResourceId, Runtime, SemanticRole, Theme,
};
use rustos_video::ColorMode;

const COMMAND_RESOLUTION_720: CommandId = CommandId(1);
const COMMAND_RESOLUTION_800: CommandId = CommandId(2);
const COMMAND_RESOLUTION_900: CommandId = CommandId(3);
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
const TEXT_1280_720: ResourceId = ResourceId(5);
const TEXT_1280_800: ResourceId = ResourceId(6);
const TEXT_1600_900: ResourceId = ResourceId(7);
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

type SettingsRuntime = Runtime<40, 112, 12>;

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
    resolution: [NodeId; 3],
    colors: [NodeId; 3],
    wallpapers: [NodeId; 3],
    scales: [NodeId; 3],
}

impl DesktopSettings {
    pub fn new(viewport: Rect, snapshot: DesktopSettingsSnapshot) -> Self {
        let mut runtime = SettingsRuntime::new(viewport, theme(snapshot.ui_scale_milli));
        let mut resolution = [NodeId::NONE; 3];
        let mut colors = [NodeId::NONE; 3];
        let mut wallpapers = [NodeId::NONE; 3];
        let mut scales = [NodeId::NONE; 3];
        build_tree(
            &mut runtime,
            &mut resolution,
            &mut colors,
            &mut wallpapers,
            &mut scales,
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
        select_group(
            &mut self.runtime,
            self.resolution,
            [
                snapshot.width == 1280 && snapshot.height == 720,
                snapshot.width == 1280 && snapshot.height == 800,
                snapshot.width == 1600 && snapshot.height == 900,
            ],
        );
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
    resolution: &mut [NodeId; 3],
    colors: &mut [NodeId; 3],
    wallpapers: &mut [NodeId; 3],
    scales: &mut [NodeId; 3],
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
            height: Length::Px(100),
            ..LayoutSpec::default()
        },
    );
    add_text(&mut ui, page, TEXT_RESOLUTION, 24, style_class::HEADING);
    *resolution = add_choices(
        &mut ui,
        page,
        [
            (TEXT_1280_720, COMMAND_RESOLUTION_720),
            (TEXT_1280_800, COMMAND_RESOLUTION_800),
            (TEXT_1600_900, COMMAND_RESOLUTION_900),
        ],
    );
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

fn select_group(runtime: &mut SettingsRuntime, nodes: [NodeId; 3], selected: [bool; 3]) {
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
    match result.command {
        COMMAND_RESOLUTION_720 => DesktopSettingsAction::SetResolution {
            width: 1280,
            height: 720,
        },
        COMMAND_RESOLUTION_800 => DesktopSettingsAction::SetResolution {
            width: 1280,
            height: 800,
        },
        COMMAND_RESOLUTION_900 => DesktopSettingsAction::SetResolution {
            width: 1600,
            height: 900,
        },
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
        TEXT_SUBTITLE => "Изменения применяются сразу",
        TEXT_RESOLUTION => "Разрешение экрана",
        TEXT_1280_720 => "1280 × 720",
        TEXT_1280_800 => "1280 × 800",
        TEXT_1600_900 => "1600 × 900",
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

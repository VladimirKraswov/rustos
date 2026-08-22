//! Компонентный интерфейс desktop shell: Start, меню и календарные часы.
//!
//! Здесь нет ручного hit-test по координатам пунктов. `system-ui` строит
//! Panel/Menu/Button/Image/Label, владеет hover, pointer capture, focus и
//! возвращает только типизированные команды оконному серверу.

use crate::{
    apps::draw_system_ui_text,
    graphics::{Color, Framebuffer, Rect},
    time::SystemClock,
};
use rustos_system_assets::{IconKind, IconPack, IconTarget, CLASSIC_ICON_PACK};
use rustos_system_ui::{
    style_class, Align, CommandId, ComponentKind, Content, DispatchResult, Edges, FontSpec,
    FrameResult, InputEvent, Key, KeyEvent, LayoutSpec, Length, NodeSpec, PointerEvent,
    PointerKind, RenderBackend, ResourceId, Runtime, SemanticRole, Theme,
};

const COMMAND_START: CommandId = CommandId(1);
const COMMAND_TERMINAL: CommandId = CommandId(2);
const COMMAND_GALLERY: CommandId = CommandId(3);
const COMMAND_SHUTDOWN: CommandId = CommandId(4);
const COMMAND_ARRANGE_DESKTOP: CommandId = CommandId(5);
const COMMAND_DESKTOP_PROPERTIES: CommandId = CommandId(6);
const COMMAND_EXPLORER: CommandId = CommandId(7);
const COMMAND_PROGRAMS: CommandId = CommandId(8);
const COMMAND_GPU_DEMO: CommandId = CommandId(9);
const COMMAND_SETTINGS: CommandId = CommandId(10);

const TEXT_START: ResourceId = ResourceId(1);
const IMAGE_RUSTOS: ResourceId = ResourceId(2);
const TEXT_RUSTOS: ResourceId = ResourceId(3);
const TEXT_TERMINAL: ResourceId = ResourceId(4);
const IMAGE_TERMINAL: ResourceId = ResourceId(5);
const TEXT_GALLERY: ResourceId = ResourceId(6);
const IMAGE_GALLERY: ResourceId = ResourceId(7);
const TEXT_SHUTDOWN: ResourceId = ResourceId(8);
const IMAGE_POWER: ResourceId = ResourceId(9);
const TEXT_TIME: ResourceId = ResourceId(10);
const TEXT_DATE: ResourceId = ResourceId(11);
const TEXT_APPLICATIONS: ResourceId = ResourceId(12);
const TEXT_ARRANGE_DESKTOP: ResourceId = ResourceId(13);
const TEXT_DESKTOP_PROPERTIES: ResourceId = ResourceId(14);
const IMAGE_ARRANGE: ResourceId = ResourceId(15);
const IMAGE_SETTINGS: ResourceId = ResourceId(16);
const TEXT_EXPLORER: ResourceId = ResourceId(17);
const IMAGE_EXPLORER: ResourceId = ResourceId(18);
const TEXT_PROGRAMS: ResourceId = ResourceId(19);
const IMAGE_PROGRAMS: ResourceId = ResourceId(20);
const TEXT_GPU_DEMO: ResourceId = ResourceId(21);
const IMAGE_GPU_DEMO: ResourceId = ResourceId(22);
const TEXT_SETTINGS: ResourceId = ResourceId(23);

type LauncherRuntime = Runtime<5, 16, 4>;
type MenuRuntime = Runtime<14, 40, 8>;
type ProgramsMenuRuntime = Runtime<18, 52, 8>;
type ClockRuntime = Runtime<5, 12, 4>;
type DesktopMenuRuntime = Runtime<9, 28, 6>;

/// Команда shell, независимая от конкретной реализации window manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellAction {
    None,
    ToggleStart,
    OpenTerminal,
    OpenFileExplorer,
    OpenGallery,
    OpenGpuDemo,
    OpenDesktopSettings,
    ArrangeDesktop,
    OpenDesktopProperties,
    Shutdown,
}

/// Итог одного input event. `consumed` не даёт клику одновременно активировать
/// кнопку меню и лежащее под ним окно.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellInputResult {
    pub action: ShellAction,
    pub changed: bool,
    pub consumed: bool,
}

impl ShellInputResult {
    const NONE: Self = Self {
        action: ShellAction::None,
        changed: false,
        consumed: false,
    };
}

/// Три небольших component runtime вместо особого «нарисованного» Start.
/// Раздельные trees позволяют инкрементально обновить часы, не инвалидируя
/// меню или полный desktop.
pub struct ShellUi {
    launcher: LauncherRuntime,
    menu: MenuRuntime,
    programs_menu: ProgramsMenuRuntime,
    clock_view: ClockRuntime,
    desktop_menu: DesktopMenuRuntime,
    clock: SystemClock,
    taskbar_height: u32,
    screen_width: u32,
    screen_height: u32,
    open: bool,
    programs_open: bool,
    desktop_menu_open: bool,
    desktop_menu_rect: Rect,
}

impl ShellUi {
    pub fn new(screen_width: u32, screen_height: u32, taskbar_height: u32, now_ms: u64) -> Self {
        let launcher_rect = launcher_rect(screen_height, taskbar_height);
        let menu_rect = menu_rect(screen_height, taskbar_height);
        let clock_rect = clock_rect(screen_width, screen_height, taskbar_height);
        let mut launcher = LauncherRuntime::new(launcher_rect, shell_theme());
        let mut menu = MenuRuntime::new(menu_rect, shell_theme());
        let mut programs_menu = ProgramsMenuRuntime::new(
            programs_menu_rect(screen_width, screen_height, taskbar_height),
            shell_theme(),
        );
        let mut clock_view = ClockRuntime::new(clock_rect, taskbar_theme());
        let desktop_menu_rect =
            desktop_popup_rect(8, 8, screen_width, screen_height, taskbar_height);
        let mut desktop_menu = DesktopMenuRuntime::new(desktop_menu_rect, shell_theme());
        build_launcher(&mut launcher);
        build_menu(&mut menu);
        build_programs_menu(&mut programs_menu);
        build_clock(&mut clock_view);
        build_desktop_menu(&mut desktop_menu);
        Self {
            launcher,
            menu,
            programs_menu,
            clock_view,
            desktop_menu,
            clock: SystemClock::new(now_ms),
            taskbar_height,
            screen_width,
            screen_height,
            open: false,
            programs_open: false,
            desktop_menu_open: false,
            desktop_menu_rect,
        }
    }

    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn set_open(&mut self, open: bool) {
        if self.open == open {
            return;
        }
        self.open = open;
        if !open {
            self.programs_open = false;
        }
        if open {
            self.desktop_menu_open = false;
        }
        self.launcher.invalidate_all();
        if open {
            self.menu.invalidate_all();
        }
    }

    pub fn toggle(&mut self) {
        self.set_open(!self.open);
    }

    pub const fn desktop_menu_is_open(&self) -> bool {
        self.desktop_menu_open
    }

    pub const fn has_popup(&self) -> bool {
        self.open || self.desktop_menu_open
    }

    /// Открывает desktop-owned context menu рядом с указателем и удерживает
    /// его целиком внутри рабочей области, включая малые видеорежимы.
    pub fn open_desktop_menu(&mut self, x: i32, y: i32) {
        self.open = false;
        self.programs_open = false;
        self.desktop_menu_open = true;
        self.desktop_menu_rect = desktop_popup_rect(
            x,
            y,
            self.screen_width,
            self.screen_height,
            self.taskbar_height,
        );
        self.desktop_menu.resize(self.desktop_menu_rect);
        self.desktop_menu.invalidate_all();
        self.launcher.invalidate_all();
    }

    pub fn close_popups(&mut self) {
        if !self.has_popup() {
            return;
        }
        self.open = false;
        self.programs_open = false;
        self.desktop_menu_open = false;
        self.launcher.invalidate_all();
    }

    pub fn resize(&mut self, screen_width: u32, screen_height: u32) {
        self.screen_width = screen_width;
        self.screen_height = screen_height;
        self.launcher
            .resize(launcher_rect(screen_height, self.taskbar_height));
        self.menu
            .resize(menu_rect(screen_height, self.taskbar_height));
        self.programs_menu.resize(programs_menu_rect(
            screen_width,
            screen_height,
            self.taskbar_height,
        ));
        self.clock_view
            .resize(clock_rect(screen_width, screen_height, self.taskbar_height));
        self.desktop_menu_rect = desktop_popup_rect(
            self.desktop_menu_rect.x,
            self.desktop_menu_rect.y,
            screen_width,
            screen_height,
            self.taskbar_height,
        );
        self.desktop_menu.resize(self.desktop_menu_rect);
    }

    pub fn pointer(&mut self, kind: PointerKind, x: i32, y: i32) -> ShellInputResult {
        let event = InputEvent::Pointer(PointerEvent::at(kind, x, y));
        let launcher = self.launcher.dispatch(event);
        let mut menu = if self.open {
            self.menu.dispatch(event)
        } else {
            DispatchResult {
                target: rustos_system_ui::NodeId::NONE,
                command: CommandId(0),
                changed: false,
                consumed: false,
            }
        };
        if menu.command == COMMAND_PROGRAMS {
            self.programs_open = !self.programs_open;
            self.programs_menu.invalidate_all();
            self.menu.invalidate_all();
            menu.command = CommandId(0);
            menu.changed = true;
        }
        let programs = if self.open && self.programs_open {
            self.programs_menu.dispatch(event)
        } else {
            empty_dispatch()
        };
        let desktop = if self.desktop_menu_open {
            self.desktop_menu.dispatch(event)
        } else {
            empty_dispatch()
        };
        combine(launcher, menu, programs, desktop)
    }

    pub fn key(&mut self, key: Key, shift: bool) -> ShellInputResult {
        if !self.has_popup() {
            return ShellInputResult::NONE;
        }
        let event = InputEvent::Key(KeyEvent {
            key,
            pressed: true,
            modifiers: 0,
            shift,
        });
        let result = if self.desktop_menu_open {
            self.desktop_menu.dispatch(event)
        } else if self.programs_open {
            self.programs_menu.dispatch(event)
        } else {
            self.menu.dispatch(event)
        };
        from_dispatch(result)
    }

    /// Применяет общий масштаб ко всем shell trees. Layout controls остаётся
    /// стабильным, а theme вычисляет увеличенный em-size для Text/Button.
    pub fn set_scale(&mut self, scale_milli: u16) {
        self.launcher.set_theme(shell_theme_scaled(scale_milli));
        self.menu.set_theme(shell_theme_scaled(scale_milli));
        self.programs_menu
            .set_theme(shell_theme_scaled(scale_milli));
        self.desktop_menu.set_theme(shell_theme_scaled(scale_milli));
        self.clock_view.set_theme(taskbar_theme_scaled(scale_milli));
    }

    /// Читает wall clock с bounded частотой. `true` означает, что видимая
    /// строка действительно изменилась и taskbar надо опубликовать снова.
    pub fn update_clock(&mut self, now_ms: u64) -> bool {
        if !self.clock.poll(now_ms) {
            return false;
        }
        self.clock_view.invalidate_all();
        if self.open {
            self.menu.invalidate_all();
        }
        true
    }

    pub fn draw_launcher(
        &mut self,
        framebuffer: &mut Framebuffer,
        icon_pack: IconPack,
        full: bool,
    ) -> FrameResult<4> {
        if full {
            self.launcher.invalidate_all();
        }
        let resources = ShellResources {
            clock: &self.clock,
            icon_pack,
        };
        let mut backend = ShellBackend {
            framebuffer,
            resources: &resources,
        };
        self.launcher
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    pub fn draw_clock(
        &mut self,
        framebuffer: &mut Framebuffer,
        icon_pack: IconPack,
        full: bool,
    ) -> FrameResult<4> {
        if full {
            self.clock_view.invalidate_all();
        }
        let resources = ShellResources {
            clock: &self.clock,
            icon_pack,
        };
        let mut backend = ShellBackend {
            framebuffer,
            resources: &resources,
        };
        self.clock_view
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    pub fn draw_menu(
        &mut self,
        framebuffer: &mut Framebuffer,
        icon_pack: IconPack,
        full: bool,
    ) -> FrameResult<8> {
        if full {
            self.menu.invalidate_all();
        }
        let resources = ShellResources {
            clock: &self.clock,
            icon_pack,
        };
        let mut backend = ShellBackend {
            framebuffer,
            resources: &resources,
        };
        self.menu
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    /// Рисует вложенное меню «Программы». Оно является отдельным focus scope,
    /// поэтому hover и keyboard navigation не смешиваются с основным Start.
    pub fn draw_programs_menu(
        &mut self,
        framebuffer: &mut Framebuffer,
        icon_pack: IconPack,
        full: bool,
    ) -> FrameResult<8> {
        if full {
            self.programs_menu.invalidate_all();
        }
        let resources = ShellResources {
            clock: &self.clock,
            icon_pack,
        };
        let mut backend = ShellBackend {
            framebuffer,
            resources: &resources,
        };
        self.programs_menu
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    pub fn draw_desktop_menu(
        &mut self,
        framebuffer: &mut Framebuffer,
        icon_pack: IconPack,
        full: bool,
    ) -> FrameResult<6> {
        if full {
            self.desktop_menu.invalidate_all();
        }
        let resources = ShellResources {
            clock: &self.clock,
            icon_pack,
        };
        let mut backend = ShellBackend {
            framebuffer,
            resources: &resources,
        };
        self.desktop_menu
            .render(&mut backend)
            .unwrap_or_else(|_| FrameResult::empty())
    }

    pub fn launcher_rect(&self) -> Rect {
        launcher_rect(self.screen_height, self.taskbar_height)
    }

    pub fn menu_rect(&self) -> Rect {
        menu_rect(self.screen_height, self.taskbar_height)
    }

    pub fn programs_menu_is_open(&self) -> bool {
        self.open && self.programs_open
    }

    pub fn programs_menu_rect(&self) -> Rect {
        programs_menu_rect(self.screen_width, self.screen_height, self.taskbar_height)
    }

    pub fn interactive_at(&self, x: i32, y: i32) -> bool {
        self.launcher_rect().contains(x, y)
            || self.open && self.menu_rect().contains(x, y)
            || self.programs_menu_is_open() && self.programs_menu_rect().contains(x, y)
            || self.desktop_menu_open && self.desktop_menu_rect.contains(x, y)
    }

    pub fn clock_source(&self) -> &'static str {
        self.clock.source_name()
    }

    pub fn clock_time(&self) -> &str {
        self.clock.time_text()
    }

    pub fn clock_date(&self) -> &str {
        self.clock.date_text()
    }
}

fn build_launcher(runtime: &mut LauncherRuntime) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let mut spec = NodeSpec::new(ComponentKind::Button);
    spec.layout = LayoutSpec::fill();
    spec.content = Content::Text(TEXT_START);
    spec.accessible_name = TEXT_START;
    spec.command = COMMAND_START;
    spec.role = SemanticRole::Button;
    spec.style = style_class::PRIMARY;
    let button = ui
        .component(root, spec)
        .unwrap_or(rustos_system_ui::NodeId::NONE);
    if !button.is_none() {
        let image = LayoutSpec {
            width: Length::Px(28),
            height: Length::Px(28),
            align: Align::End,
            ..LayoutSpec::default()
        };
        let _ = ui.image(button, IMAGE_RUSTOS, TEXT_RUSTOS, image);
    }
}

fn build_menu(runtime: &mut MenuRuntime) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let surface = ui
        .menu(root, LayoutSpec::fill())
        .unwrap_or(rustos_system_ui::NodeId::NONE);
    if surface.is_none() {
        return;
    }
    let mut column = NodeSpec::new(ComponentKind::Column);
    column.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        padding: Edges::all(12),
        gap: 8,
        ..LayoutSpec::default()
    };
    let Ok(column) = ui.component(surface, column) else {
        return;
    };

    let mut header = NodeSpec::new(ComponentKind::Row);
    header.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(44),
        gap: 10,
        align: Align::Center,
        padding: Edges::symmetric(8, 4),
        ..LayoutSpec::default()
    };
    header.style = style_class::SUBTLE;
    if let Ok(header) = ui.component(column, header) {
        let icon = LayoutSpec {
            width: Length::Px(34),
            height: Length::Px(34),
            align: Align::Center,
            ..LayoutSpec::default()
        };
        let _ = ui.image(header, IMAGE_RUSTOS, TEXT_RUSTOS, icon);
        let label = LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(34),
            align: Align::Center,
            ..LayoutSpec::default()
        };
        let _ = ui.text(header, TEXT_RUSTOS, label);
    }

    add_menu_button(
        &mut ui,
        column,
        TEXT_PROGRAMS,
        IMAGE_PROGRAMS,
        COMMAND_PROGRAMS,
    );

    // Пустой прозрачный Column получает остаток высоты и прижимает clock и
    // shutdown к низу без абсолютных координат.
    let mut spacer = NodeSpec::new(ComponentKind::Column);
    spacer.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        ..LayoutSpec::default()
    };
    let _ = ui.component(column, spacer);

    let mut clock_spec = NodeSpec::new(ComponentKind::Panel);
    clock_spec.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(42),
        padding: Edges::symmetric(4, 2),
        gap: 2,
        ..LayoutSpec::default()
    };
    clock_spec.style = style_class::SUBTLE;
    let clock_panel = ui
        .component(column, clock_spec)
        .unwrap_or(rustos_system_ui::NodeId::NONE);
    if !clock_panel.is_none() {
        for resource in [TEXT_TIME, TEXT_DATE] {
            let _ = ui.text(
                clock_panel,
                resource,
                LayoutSpec {
                    width: Length::Fill(1),
                    height: Length::Fill(1),
                    ..LayoutSpec::default()
                },
            );
        }
    }
    add_menu_button(
        &mut ui,
        column,
        TEXT_SHUTDOWN,
        IMAGE_POWER,
        COMMAND_SHUTDOWN,
    );
}

fn build_programs_menu(runtime: &mut ProgramsMenuRuntime) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let surface = ui
        .menu(root, LayoutSpec::fill())
        .unwrap_or(rustos_system_ui::NodeId::NONE);
    if surface.is_none() {
        return;
    }
    let mut column = NodeSpec::new(ComponentKind::Column);
    column.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        padding: Edges::all(8),
        gap: 4,
        ..LayoutSpec::default()
    };
    let Ok(column) = ui.component(surface, column) else {
        return;
    };
    for (text, image, command) in [
        (TEXT_EXPLORER, IMAGE_EXPLORER, COMMAND_EXPLORER),
        (TEXT_TERMINAL, IMAGE_TERMINAL, COMMAND_TERMINAL),
        (TEXT_GALLERY, IMAGE_GALLERY, COMMAND_GALLERY),
        (TEXT_SETTINGS, IMAGE_SETTINGS, COMMAND_SETTINGS),
        (TEXT_GPU_DEMO, IMAGE_GPU_DEMO, COMMAND_GPU_DEMO),
    ] {
        add_menu_button(&mut ui, column, text, image, command);
    }
}

fn add_menu_button<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: rustos_system_ui::NodeId,
    label: ResourceId,
    image: ResourceId,
    command: CommandId,
) {
    let layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(48),
        ..LayoutSpec::default()
    };
    let button = ui
        .menu_item(parent, label, command, layout)
        .unwrap_or(rustos_system_ui::NodeId::NONE);
    if button.is_none() {
        return;
    }
    let image_layout = LayoutSpec {
        width: Length::Px(34),
        height: Length::Px(34),
        align: Align::End,
        ..LayoutSpec::default()
    };
    let _ = ui.image(button, image, label, image_layout);
}

fn build_clock(runtime: &mut ClockRuntime) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let mut column = NodeSpec::new(ComponentKind::Column);
    column.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        padding: Edges::symmetric(4, 0),
        ..LayoutSpec::default()
    };
    let Ok(column) = ui.component(root, column) else {
        return;
    };
    for resource in [TEXT_TIME, TEXT_DATE] {
        let _ = ui.text(
            column,
            resource,
            LayoutSpec {
                width: Length::Fill(1),
                height: Length::Fill(1),
                ..LayoutSpec::default()
            },
        );
    }
}

fn build_desktop_menu(runtime: &mut DesktopMenuRuntime) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let surface = ui
        .menu(root, LayoutSpec::fill())
        .unwrap_or(rustos_system_ui::NodeId::NONE);
    if surface.is_none() {
        return;
    }
    let mut column = NodeSpec::new(ComponentKind::Column);
    column.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        padding: Edges::all(6),
        gap: 6,
        ..LayoutSpec::default()
    };
    let Ok(column) = ui.component(surface, column) else {
        return;
    };
    add_menu_button(
        &mut ui,
        column,
        TEXT_ARRANGE_DESKTOP,
        IMAGE_ARRANGE,
        COMMAND_ARRANGE_DESKTOP,
    );
    add_menu_button(
        &mut ui,
        column,
        TEXT_DESKTOP_PROPERTIES,
        IMAGE_SETTINGS,
        COMMAND_DESKTOP_PROPERTIES,
    );
}

fn combine(
    launcher: DispatchResult,
    menu: DispatchResult,
    programs: DispatchResult,
    desktop: DispatchResult,
) -> ShellInputResult {
    let command = if launcher.command != CommandId(0) {
        launcher.command
    } else if menu.command != CommandId(0) {
        menu.command
    } else if programs.command != CommandId(0) {
        programs.command
    } else {
        desktop.command
    };
    ShellInputResult {
        action: action_for(command),
        changed: launcher.changed || menu.changed || programs.changed || desktop.changed,
        consumed: launcher.consumed || menu.consumed || programs.consumed || desktop.consumed,
    }
}

const fn empty_dispatch() -> DispatchResult {
    DispatchResult {
        target: rustos_system_ui::NodeId::NONE,
        command: CommandId(0),
        changed: false,
        consumed: false,
    }
}

fn from_dispatch(result: DispatchResult) -> ShellInputResult {
    ShellInputResult {
        action: action_for(result.command),
        changed: result.changed,
        consumed: result.consumed,
    }
}

const fn action_for(command: CommandId) -> ShellAction {
    match command {
        COMMAND_START => ShellAction::ToggleStart,
        COMMAND_TERMINAL => ShellAction::OpenTerminal,
        COMMAND_EXPLORER => ShellAction::OpenFileExplorer,
        COMMAND_GALLERY => ShellAction::OpenGallery,
        COMMAND_GPU_DEMO => ShellAction::OpenGpuDemo,
        COMMAND_SETTINGS => ShellAction::OpenDesktopSettings,
        COMMAND_ARRANGE_DESKTOP => ShellAction::ArrangeDesktop,
        COMMAND_DESKTOP_PROPERTIES => ShellAction::OpenDesktopProperties,
        COMMAND_SHUTDOWN => ShellAction::Shutdown,
        _ => ShellAction::None,
    }
}

fn launcher_rect(screen_height: u32, taskbar_height: u32) -> Rect {
    Rect::new(
        6,
        screen_height as i32 - taskbar_height as i32 + 4,
        112,
        taskbar_height.saturating_sub(8),
    )
}

fn menu_rect(screen_height: u32, taskbar_height: u32) -> Rect {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 392;
    Rect::new(
        6,
        (screen_height as i32 - taskbar_height as i32 - HEIGHT as i32 - 6).max(6),
        WIDTH,
        HEIGHT.min(screen_height.saturating_sub(taskbar_height + 12)),
    )
}

fn programs_menu_rect(screen_width: u32, screen_height: u32, taskbar_height: u32) -> Rect {
    const WIDTH: u32 = 292;
    const HEIGHT: u32 = 276;
    let main = menu_rect(screen_height, taskbar_height);
    let right = main.right().saturating_add(6);
    let x = if right.saturating_add(WIDTH as i32) <= screen_width as i32 {
        right
    } else {
        main.x.saturating_sub(WIDTH as i32 + 6).max(4)
    };
    Rect::new(
        x,
        main.y.saturating_add(58),
        WIDTH.min(screen_width.saturating_sub(8).max(1)),
        HEIGHT.min(screen_height.saturating_sub(taskbar_height + 12).max(1)),
    )
}

fn clock_rect(screen_width: u32, screen_height: u32, taskbar_height: u32) -> Rect {
    Rect::new(
        screen_width.saturating_sub(150) as i32,
        screen_height as i32 - taskbar_height as i32 + 4,
        144,
        taskbar_height.saturating_sub(8),
    )
}

fn desktop_popup_rect(
    requested_x: i32,
    requested_y: i32,
    screen_width: u32,
    screen_height: u32,
    taskbar_height: u32,
) -> Rect {
    let width = 244u32.min(screen_width.saturating_sub(8).max(1));
    let height = 114u32.min(
        screen_height
            .saturating_sub(taskbar_height)
            .saturating_sub(8)
            .max(1),
    );
    let max_x = screen_width.saturating_sub(width + 4) as i32;
    let max_y = screen_height.saturating_sub(taskbar_height + height + 4) as i32;
    Rect::new(
        requested_x.clamp(4, max_x.max(4)),
        requested_y.clamp(4, max_y.max(4)),
        width,
        height,
    )
}

const fn shell_theme() -> Theme {
    Theme::dark()
}

fn shell_theme_scaled(scale_milli: u16) -> Theme {
    let mut theme = Theme::dark();
    theme.scale_milli = scale_milli;
    theme
}

fn taskbar_theme() -> Theme {
    taskbar_theme_scaled(1_000)
}

fn taskbar_theme_scaled(scale_milli: u16) -> Theme {
    let mut theme = Theme::dark();
    theme.palette.window = Color::rgb(13, 19, 30);
    theme.scale_milli = scale_milli;
    theme
}

struct ShellResources<'a> {
    clock: &'a SystemClock,
    icon_pack: IconPack,
}

impl ShellResources<'_> {
    fn text(&self, resource: ResourceId) -> &str {
        match resource {
            TEXT_START => "Пуск",
            TEXT_RUSTOS => "RustOS",
            TEXT_TERMINAL => "Новый терминал",
            TEXT_EXPLORER => "Проводник",
            TEXT_GALLERY => "Компоненты UI",
            TEXT_SHUTDOWN => "Завершить работу",
            TEXT_TIME => self.clock.time_text(),
            TEXT_DATE => self.clock.date_text(),
            TEXT_APPLICATIONS => "Приложения",
            TEXT_PROGRAMS => "Программы  ›",
            TEXT_GPU_DEMO => "Aurora 3D",
            TEXT_SETTINGS => "Параметры рабочего стола",
            TEXT_ARRANGE_DESKTOP => "Упорядочить значки",
            TEXT_DESKTOP_PROPERTIES => "Параметры рабочего стола",
            _ => "",
        }
    }
}

struct ShellBackend<'framebuffer, 'resources, 'clock> {
    framebuffer: &'framebuffer mut Framebuffer,
    resources: &'resources ShellResources<'clock>,
}

impl RenderBackend for ShellBackend<'_, '_, '_> {
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
            draw_clipped_border(self.framebuffer, current, color, clip);
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
        // Две строки часов обязаны помещаться в неизменную высоту taskbar;
        // основные подписи shell масштабируются полностью.
        let size = if matches!(resource, TEXT_TIME | TEXT_DATE) {
            spec.size.min(16)
        } else {
            spec.size
        };
        let mut spec = spec;
        spec.size = size;
        draw_system_ui_text(
            self.framebuffer,
            rect,
            self.resources.text(resource),
            color,
            spec,
            clip,
        );
    }

    fn image(&mut self, rect: Rect, resource: ResourceId, _: Color, clip: Rect) {
        if rect.intersection(clip).is_empty() {
            return;
        }
        match resource {
            IMAGE_RUSTOS => draw_rustos_logo(self.framebuffer, rect),
            IMAGE_GALLERY => self
                .resources
                .icon_pack
                .draw(self.framebuffer, IconKind::Grid, rect),
            IMAGE_TERMINAL => {
                self.resources
                    .icon_pack
                    .draw(self.framebuffer, IconKind::Terminal, rect);
            }
            IMAGE_EXPLORER => {
                self.resources
                    .icon_pack
                    .draw(self.framebuffer, IconKind::Folder, rect);
            }
            IMAGE_POWER => self
                .resources
                .icon_pack
                .draw(self.framebuffer, IconKind::Power, rect),
            IMAGE_ARRANGE => {
                self.resources
                    .icon_pack
                    .draw(self.framebuffer, IconKind::Grid, rect);
            }
            IMAGE_SETTINGS => {
                self.resources
                    .icon_pack
                    .draw(self.framebuffer, IconKind::Settings, rect);
            }
            IMAGE_PROGRAMS => {
                self.resources
                    .icon_pack
                    .draw(self.framebuffer, IconKind::Grid, rect);
            }
            IMAGE_GPU_DEMO => {
                self.resources
                    .icon_pack
                    .draw(self.framebuffer, IconKind::GpuDemo, rect);
            }
            _ => {}
        }
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

fn draw_rustos_logo(framebuffer: &mut Framebuffer, rect: Rect) {
    let size = rect.width.min(rect.height).max(4);
    let cell = size.saturating_sub(2) / 2;
    let x = rect.x + (rect.width.saturating_sub(size) / 2) as i32;
    let y = rect.y + (rect.height.saturating_sub(size) / 2) as i32;
    let colors = [
        Color::rgb(80, 196, 220),
        Color::rgb(105, 220, 192),
        Color::rgb(103, 140, 238),
        Color::rgb(170, 116, 235),
    ];
    for row in 0..2u32 {
        for column in 0..2u32 {
            IconTarget::rounded_fill(
                framebuffer,
                Rect::new(
                    x + (column * (cell + 2)) as i32,
                    y + (row * (cell + 2)) as i32,
                    cell,
                    cell,
                ),
                4,
                colors[(row * 2 + column) as usize],
            );
        }
    }
}

/// Active icon pack передаётся значением: pack содержит только metadata,
/// palette и function pointer, поэтому никакой графический код не копируется.
pub fn active_icon_or_default(active: Option<IconPack>) -> IconPack {
    active.unwrap_or(CLASSIC_ICON_PACK)
}

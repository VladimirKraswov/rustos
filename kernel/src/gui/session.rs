//! Desktop session, software compositor и оконный сервер RustOS.
//!
//! Окно, состояние приложения и его время жизни здесь разделены намеренно:
//! оконный сервер владеет только geometry/Z-order, а каждый запущенный клиент —
//! собственным [`Application`]. Закрытие удаляет экземпляр приложения и
//! возвращает занятые им физические кадры. Это bootstrap transport на CPU0;
//! wire ABI уже пригоден для замены прямого вызова capability IPC ring 3.

use core::{mem::size_of, ptr};

use crate::{
    apps::{
        terminal::{
            CursorCommand, CursorThemeName, IconThemeName, MouseCommand, Terminal, TerminalAction,
        },
        ui_showcase::UiShowcase,
    },
    arch, font,
    graphics::{Color, Framebuffer, Rect},
    gui::{
        components::{self, Button, Label, Panel, Theme, Widget},
        cursor::Cursor,
    },
    input::{self, Event, Key, MouseEvent, PlatformInput},
    memory::{self, FrameBlock},
    serial,
};
use rustos_abi::{
    bootinfo::BootInitramfs,
    input::{MouseSettings, PointerCursor},
    window::{
        event as window_event, WindowCommand, WindowCreateRequest, WindowEvent, WindowId,
        WindowRect, WindowStyle,
    },
    BootInfo, PAGE_SIZE,
};
use rustos_system_assets::{
    wallpaper, IconKind, IconPack, PackId, PackRegistry, ResourcePack, WallpaperId,
    CLASSIC_ICON_PACK, MIDNIGHT_ICON_PACK, MONO_ICON_PACK,
};
use rustos_system_ui::{Key as UiKey, PointerKind as UiPointerKind};
use rustos_video::{
    hit_test_resize, resize_from_edges, DamageRegion, DisplayDriver, DisplayMode, ManagedWindow,
    PixelFormat, ResizeEdges, Scanout, WindowEventQueue,
};

const TASKBAR_HEIGHT: u32 = 46;
const TITLE_HEIGHT: u32 = 34;
const RESIZE_BORDER: u32 = 6;
const WINDOW_SHADOW_RIGHT: u32 = 7;
const WINDOW_SHADOW_BOTTOM: u32 = 8;
const TERMINAL_MIN_WIDTH: u32 = 480;
const TERMINAL_MIN_HEIGHT: u32 = 300;
const GALLERY_MIN_WIDTH: u32 = 560;
const GALLERY_MIN_HEIGHT: u32 = 360;

/// Bounded registry защищает ядро от исчерпания памяти одним GUI-клиентом.
/// Состояние приложений при этом выделяется динамически из frame allocator,
/// поэтому лимит можно менять независимо от размера kernel stack.
const MAX_WINDOWS: usize = 16;
const WINDOW_EVENT_CAPACITY: usize = 128;

pub fn run(info: &BootInfo) -> ! {
    let Some(framebuffer) = Framebuffer::from_boot(info) else {
        serial::put_str("[gui] no supported framebuffer/back buffer; system halted\n");
        loop {
            arch::halt();
        }
    };
    serial::put_str("[gui] back buffer @ 0x");
    serial::put_hex(framebuffer.backbuffer_phys());
    serial::put_str(" size=");
    serial::put_u32((framebuffer.backbuffer_bytes() / 1024) as u32);
    serial::put_str(" KiB background-cache=");
    serial::put_str(if framebuffer.has_background_cache() {
        "yes"
    } else {
        "no"
    });
    serial::put_str("\n");
    let mode = framebuffer.mode();
    let capabilities = framebuffer.capabilities();
    serial::put_str("[video] scanout=");
    serial::put_str(framebuffer.driver_name());
    serial::put_str(" mode=");
    serial::put_u32(mode.width);
    serial::put_str("x");
    serial::put_u32(mode.height);
    serial::put_str(" format=");
    serial::put_str(match mode.format {
        PixelFormat::Rgb888 => "rgb888",
        PixelFormat::Bgr888 => "bgr888",
        PixelFormat::Argb8888 => "argb8888",
        PixelFormat::Rgb565 => "rgb565",
        PixelFormat::Grayscale8 => "gray8",
    });
    serial::put_str(" present=immediate page-flip=");
    serial::put_str(if capabilities.page_flip { "yes" } else { "no" });
    serial::put_str("\n");
    serial::put_str(
        "[font] families=console,sans scripts=latin,cyrillic styles=regular,bold,italic sizes=10..48\n",
    );
    serial::put_str("[ui] constructing independent application sessions\n");
    let mut session = DesktopSession::new(
        framebuffer,
        info.total_usable_ram() / (1024 * 1024),
        info.initramfs,
    );
    let _ = session.spawn_application(ApplicationKind::Terminal);
    serial::put_str("[ui] window server ready capacity=");
    serial::put_u32(MAX_WINDOWS as u32);
    serial::put_str("\n");
    session.render_all();
    serial::put_str("[gui] GUI_READY desktop=1 terminal=1 multiwindow=1 mouse=");
    serial::put_str(input::backend_name());
    serial::put_str("\n");
    session.event_loop()
}

#[derive(Clone, Copy)]
enum WindowInteraction {
    None,
    Move {
        window: WindowId,
        offset_x: i32,
        offset_y: i32,
    },
    Resize {
        window: WindowId,
        edges: ResizeEdges,
        start_mouse_x: i32,
        start_mouse_y: i32,
        start: WindowRect,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationKind {
    Terminal,
    UiShowcase,
}

impl ApplicationKind {
    const fn title(self) -> &'static str {
        match self {
            Self::Terminal => "RUSTOS · ТЕРМИНАЛ",
            Self::UiShowcase => "RUSTOS · SYSTEM UI GALLERY",
        }
    }

    const fn task_label(self) -> &'static str {
        match self {
            Self::Terminal => "TERMINAL",
            Self::UiShowcase => "UI GALLERY",
        }
    }

    const fn minimum_size(self) -> (u32, u32) {
        match self {
            Self::Terminal => (TERMINAL_MIN_WIDTH, TERMINAL_MIN_HEIGHT),
            Self::UiShowcase => (GALLERY_MIN_WIDTH, GALLERY_MIN_HEIGHT),
        }
    }
}

enum Application {
    Terminal(Terminal),
    UiShowcase(UiShowcase),
}

impl Application {
    const fn kind(&self) -> ApplicationKind {
        match self {
            Self::Terminal(_) => ApplicationKind::Terminal,
            Self::UiShowcase(_) => ApplicationKind::UiShowcase,
        }
    }
}

/// Heap ядру не нужен: объект приложения размещается в непрерывных физических
/// кадрах и уничтожается через Drop. Именно этот владелец превращает `close`
/// из визуального флага в настоящий lifecycle transition с освобождением RAM.
struct ApplicationMemory {
    pointer: *mut Application,
    block: FrameBlock,
}

impl ApplicationMemory {
    fn new(application: Application) -> Option<Self> {
        let bytes = u64::try_from(size_of::<Application>()).ok()?;
        let frames = bytes.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE;
        let block = memory::allocate(frames.max(1), 1).ok()?;
        let pointer = block.phys as *mut Application;
        // SAFETY: frame allocator вернул уникальный identity-mapped диапазон,
        // выровненный минимум на 4 KiB и достаточный для Application.
        unsafe { pointer.write(application) };
        Some(Self { pointer, block })
    }

    fn get(&self) -> &Application {
        // SAFETY: pointer инициализирован в new и живёт до Drop владельца.
        unsafe { &*self.pointer }
    }

    fn get_mut(&mut self) -> &mut Application {
        // SAFETY: &mut self гарантирует единственный mutable access.
        unsafe { &mut *self.pointer }
    }

    const fn frames(&self) -> u64 {
        self.block.frames
    }
}

impl Drop for ApplicationMemory {
    fn drop(&mut self) {
        if self.pointer.is_null() || self.block.frames == 0 {
            return;
        }
        // SAFETY: значение было записано ровно один раз и ещё не уничтожено.
        unsafe { ptr::drop_in_place(self.pointer) };
        let _ = memory::free(self.block);
        self.pointer = ptr::null_mut();
        self.block = FrameBlock { phys: 0, frames: 0 };
    }
}

struct WindowSlot {
    model: ManagedWindow,
    application: ApplicationMemory,
}

impl WindowSlot {
    fn kind(&self) -> ApplicationKind {
        self.application.get().kind()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ClickKind {
    Single,
    Double,
}

struct ClickTracker {
    last_press_ms: u64,
    last_x: i32,
    last_y: i32,
}

impl ClickTracker {
    const fn new() -> Self {
        Self {
            last_press_ms: 0,
            last_x: 0,
            last_y: 0,
        }
    }

    fn pressed(&mut self, now_ms: u64, x: i32, y: i32, settings: MouseSettings) -> ClickKind {
        let elapsed = now_ms.saturating_sub(self.last_press_ms);
        let movement = (x - self.last_x).abs().max((y - self.last_y).abs());
        let double = self.last_press_ms != 0
            && elapsed >= u64::from(settings.click_debounce_ms)
            && elapsed <= u64::from(settings.double_click_ms)
            && movement <= i32::from(settings.drag_threshold_px);
        if double {
            self.last_press_ms = 0;
            ClickKind::Double
        } else {
            self.last_press_ms = now_ms;
            self.last_x = x;
            self.last_y = y;
            ClickKind::Single
        }
    }
}

struct DesktopSession {
    framebuffer: Framebuffer,
    input: PlatformInput,
    usable_ram_mib: u64,
    initramfs: BootInitramfs,
    windows: [Option<WindowSlot>; MAX_WINDOWS],
    /// От нижнего окна к верхнему. ID стабилен при любом reorder.
    z_order: [WindowId; MAX_WINDOWS],
    /// Порядок кнопок taskbar не меняется при смене фокуса.
    task_order: [WindowId; MAX_WINDOWS],
    window_count: usize,
    focused: Option<WindowId>,
    next_window_id: u64,
    cascade: u32,
    window_events: WindowEventQueue<WINDOW_EVENT_CAPACITY>,
    interaction: WindowInteraction,
    cursor: Cursor,
    icon_packs: PackRegistry<IconPack, 8>,
    wallpaper: WallpaperId,
    mouse_x: i32,
    mouse_y: i32,
    previous_left: bool,
    start_open: bool,
    drag_frames: u32,
    drag_packets: u32,
    drag_present_pixels: u64,
    drag_preview_visible: bool,
    desktop_icon_selected: bool,
    desktop_icon_pressed: bool,
    click_tracker: ClickTracker,
}

impl DesktopSession {
    fn new(framebuffer: Framebuffer, usable_ram_mib: u64, initramfs: BootInitramfs) -> Self {
        let screen_width = framebuffer.width();
        let screen_height = framebuffer.height();
        let mut icon_packs = PackRegistry::new();
        let _ = icon_packs.install(CLASSIC_ICON_PACK);
        let _ = icon_packs.install(MIDNIGHT_ICON_PACK);
        let _ = icon_packs.install(MONO_ICON_PACK);
        Self {
            framebuffer,
            input: PlatformInput::new(),
            usable_ram_mib,
            initramfs,
            windows: [const { None }; MAX_WINDOWS],
            z_order: [WindowId::new(0); MAX_WINDOWS],
            task_order: [WindowId::new(0); MAX_WINDOWS],
            window_count: 0,
            focused: None,
            next_window_id: 1,
            cascade: 0,
            window_events: WindowEventQueue::new(),
            interaction: WindowInteraction::None,
            cursor: Cursor::new(),
            icon_packs,
            wallpaper: WallpaperId::SpringRiver,
            mouse_x: (screen_width / 2) as i32,
            mouse_y: (screen_height / 2) as i32,
            previous_left: false,
            start_open: false,
            drag_frames: 0,
            drag_packets: 0,
            drag_present_pixels: 0,
            drag_preview_visible: false,
            desktop_icon_selected: false,
            desktop_icon_pressed: false,
            click_tracker: ClickTracker::new(),
        }
    }

    fn event_loop(&mut self) -> ! {
        loop {
            if let Some(event) = self.input.poll() {
                let old_cursor = self.cursor.rect();
                self.cursor.restore(&mut self.framebuffer);
                let redraw = match event {
                    Event::Key(key) => self.handle_key(key),
                    Event::Mouse(mouse) => {
                        let redraw = self.handle_mouse(mouse);
                        self.update_cursor_hint();
                        redraw
                    }
                };

                let mut terminal_line = None;
                let mut drag_cached = false;
                match redraw {
                    Redraw::Scene => self.render_scene(),
                    Redraw::TerminalLine => {
                        terminal_line = self.draw_focused_terminal_line();
                    }
                    Redraw::DragMove {
                        window,
                        previous,
                        first,
                    } => {
                        drag_cached = self.render_drag_preview(window, previous, first);
                    }
                    Redraw::DragEnd {
                        window,
                        preview,
                        visible,
                        ..
                    } => {
                        drag_cached = self.render_drag_end(window, preview, visible);
                    }
                    Redraw::None => {}
                }

                self.cursor
                    .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                match redraw {
                    Redraw::None => {
                        self.framebuffer.present_rect(old_cursor);
                        self.framebuffer.present_rect(self.cursor.rect());
                    }
                    Redraw::TerminalLine => {
                        self.framebuffer.present_rect(old_cursor);
                        if let Some(line) = terminal_line {
                            self.framebuffer.present_rect(line);
                        }
                        self.framebuffer.present_rect(self.cursor.rect());
                    }
                    Redraw::DragMove {
                        window,
                        previous,
                        first,
                    } => {
                        if drag_cached {
                            if first {
                                self.present_drag_rect(window_damage(previous));
                            } else {
                                self.present_preview(previous);
                            }
                            if let Some(current) = self.window_rect(window) {
                                self.present_preview(current);
                            }
                        } else {
                            self.present_drag_full();
                        }
                        self.framebuffer.present_rect(old_cursor);
                        self.framebuffer.present_rect(self.cursor.rect());
                    }
                    Redraw::DragEnd {
                        window,
                        visible,
                        resized,
                        ..
                    } => {
                        if visible {
                            if drag_cached {
                                if let Some(current) = self.window_rect(window) {
                                    self.present_drag_rect(window_damage(current));
                                }
                            } else {
                                self.present_drag_full();
                            }
                        } else {
                            self.framebuffer.present_rect(old_cursor);
                            self.framebuffer.present_rect(self.cursor.rect());
                        }
                        self.log_drag_finished(window, resized);
                    }
                    Redraw::Scene => self.framebuffer.present(),
                }
                self.dispatch_window_events();
            } else if self.cursor.animate(arch::monotonic_milliseconds()) {
                let old_cursor = self.cursor.rect();
                self.cursor.restore(&mut self.framebuffer);
                self.cursor
                    .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                self.framebuffer.present_rect(old_cursor);
                self.framebuffer.present_rect(self.cursor.rect());
            } else {
                core::hint::spin_loop();
            }
        }
    }

    fn handle_key(&mut self, key: Key) -> Redraw {
        if matches!(key, Key::Escape) && self.start_open {
            self.start_open = false;
            return Redraw::Scene;
        }
        let Some(id) = self.focused else {
            return Redraw::None;
        };
        if !self.window_is_visible(id) {
            return Redraw::None;
        }
        match self.window_kind(id) {
            Some(ApplicationKind::UiShowcase) => {
                let key = match key {
                    Key::Tab => UiKey::Tab,
                    Key::Enter => UiKey::Enter,
                    Key::Escape => UiKey::Escape,
                    Key::Character(b' ') => UiKey::Space,
                    Key::Character(byte) if byte.is_ascii() => UiKey::Character(char::from(byte)),
                    Key::Backspace | Key::Character(_) => return Redraw::None,
                };
                let changed = match self.application_mut(id) {
                    Some(Application::UiShowcase(showcase)) => showcase.key(key, false),
                    _ => false,
                };
                if changed {
                    Redraw::Scene
                } else {
                    Redraw::None
                }
            }
            Some(ApplicationKind::Terminal) => self.handle_terminal_key(id, key),
            None => Redraw::None,
        }
    }

    fn handle_terminal_key(&mut self, id: WindowId, key: Key) -> Redraw {
        let action = match self.application_mut(id) {
            Some(Application::Terminal(terminal)) => terminal.handle_key(key),
            _ => return Redraw::None,
        };
        match action {
            TerminalAction::None => Redraw::None,
            TerminalAction::RedrawInputLine => Redraw::TerminalLine,
            TerminalAction::RedrawAll => Redraw::Scene,
            TerminalAction::DisplayInfo => {
                let mode = self.framebuffer.mode();
                let connector = self.framebuffer.connector();
                let driver = self.framebuffer.driver_name();
                let color = self.framebuffer.color_mode();
                serial::put_str("[display] info driver=");
                serial::put_str(driver);
                serial::put_str(" mode=");
                serial::put_u32(mode.width);
                serial::put_str("x");
                serial::put_u32(mode.height);
                serial::put_str("\n");
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_display_info(
                        driver,
                        mode.width,
                        mode.height,
                        connector.width_mm,
                        connector.height_mm,
                        color,
                    );
                }
                Redraw::Scene
            }
            TerminalAction::DisplayModes => {
                let mut modes = [self.framebuffer.mode(); 20];
                let count = self.framebuffer.modes(&mut modes);
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_display_modes(&modes[..count]);
                }
                serial::put_str("[display] modes count=");
                serial::put_u32(count as u32);
                serial::put_str("\n");
                Redraw::Scene
            }
            TerminalAction::DisplayMode { width, height } => {
                let current = self.framebuffer.mode();
                let requested = DisplayMode {
                    width,
                    height,
                    stride_pixels: width,
                    format: current.format,
                    refresh_millihertz: current.refresh_millihertz,
                };
                let result = self.framebuffer.set_mode(requested);
                if result.is_ok() {
                    self.relayout_after_mode_set();
                }
                serial::put_str("[display] mode request=");
                serial::put_u32(width);
                serial::put_str("x");
                serial::put_u32(height);
                serial::put_str(match result {
                    Ok(_) => " result=active\n",
                    Err(rustos_video::ModeSetError::RequiresReboot) => " result=reboot-required\n",
                    Err(rustos_video::ModeSetError::UnsupportedMode) => " result=unsupported\n",
                    Err(rustos_video::ModeSetError::OutOfMemory) => " result=out-of-memory\n",
                    Err(rustos_video::ModeSetError::DeviceLost) => " result=device-lost\n",
                });
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_display_mode(width, height, result);
                }
                Redraw::Scene
            }
            TerminalAction::DisplayColor(mode) => {
                self.framebuffer.set_color_mode(mode);
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_color_mode(mode);
                }
                serial::put_str("[display] color=");
                serial::put_str(match mode {
                    rustos_video::ColorMode::TrueColor24 => "truecolor24",
                    rustos_video::ColorMode::HighColor16 => "rgb565",
                    rustos_video::ColorMode::Grayscale8 => "gray8",
                });
                serial::put_str("\n");
                Redraw::Scene
            }
            TerminalAction::OpenUiShowcase => {
                let _ = self.spawn_application(ApplicationKind::UiShowcase);
                serial::put_str("[ui] Gallery opened runtime=system-ui-v1 independent-window=1\n");
                Redraw::Scene
            }
            TerminalAction::Mouse(command) => {
                let mut settings = self.input.mouse_settings();
                let hardware_applied = match command {
                    MouseCommand::Info => None,
                    MouseCommand::Rate(value) => {
                        settings.sample_rate_hz = value;
                        Some(self.input.set_mouse_settings(settings))
                    }
                    MouseCommand::Resolution(value) => {
                        settings.resolution_level = value;
                        Some(self.input.set_mouse_settings(settings))
                    }
                    MouseCommand::Sensitivity(value) => {
                        settings.sensitivity_percent = value;
                        Some(self.input.set_mouse_settings(settings))
                    }
                    MouseCommand::Acceleration(value) => {
                        settings.acceleration_percent = value;
                        Some(self.input.set_mouse_settings(settings))
                    }
                    MouseCommand::DoubleClick(value) => {
                        settings.double_click_ms = value;
                        Some(self.input.set_mouse_settings(settings))
                    }
                    MouseCommand::Debounce(value) => {
                        settings.click_debounce_ms = value;
                        Some(self.input.set_mouse_settings(settings))
                    }
                    MouseCommand::DragThreshold(value) => {
                        settings.drag_threshold_px = value;
                        Some(self.input.set_mouse_settings(settings))
                    }
                };
                let settings = self.input.mouse_settings();
                let capabilities = self.input.mouse_capabilities();
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_mouse(settings, capabilities, hardware_applied);
                }
                serial::put_str("[input] mouse profile updated rate=");
                serial::put_u32(u32::from(settings.sample_rate_hz));
                serial::put_str(" sensitivity=");
                serial::put_u32(u32::from(settings.sensitivity_percent));
                serial::put_str("% double-ms=");
                serial::put_u32(u32::from(settings.double_click_ms));
                serial::put_str("\n");
                Redraw::Scene
            }
            TerminalAction::Cursor(command) => {
                let value = match command {
                    CursorCommand::Auto => {
                        self.cursor.set_preview(None);
                        "AUTO"
                    }
                    CursorCommand::Preview(kind) => {
                        self.cursor.set_preview(Some(kind));
                        cursor_name(kind)
                    }
                    CursorCommand::Theme(theme) => {
                        let pack = match theme {
                            CursorThemeName::Light => PackId(0x1001),
                            CursorThemeName::Midnight => PackId(0x1002),
                            CursorThemeName::Contrast => PackId(0x1003),
                        };
                        let _ = self.cursor.select_theme(pack);
                        self.cursor.theme_name()
                    }
                };
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_visual_setting("CURSOR", value);
                }
                serial::put_str("[cursor] value=");
                serial::put_str(value);
                serial::put_str(" theme=");
                serial::put_str(self.cursor.theme_name());
                serial::put_str("\n");
                Redraw::Scene
            }
            TerminalAction::Icons(theme) => {
                let pack = match theme {
                    IconThemeName::Classic => PackId(0x2001),
                    IconThemeName::Midnight => PackId(0x2002),
                    IconThemeName::Mono => PackId(0x2003),
                };
                let _ = self.icon_packs.select(pack);
                let name = self
                    .icon_packs
                    .active()
                    .map_or("none", |icons| icons.metadata().name);
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_visual_setting("ICON PACK", name);
                }
                serial::put_str("[assets] icon-pack=");
                serial::put_str(name);
                serial::put_str("\n");
                Redraw::Scene
            }
            TerminalAction::Wallpaper(selected) => {
                self.wallpaper = selected;
                if let Some(Application::Terminal(terminal)) = self.application_mut(id) {
                    terminal.report_visual_setting("WALLPAPER", wallpaper(selected).name);
                }
                serial::put_str("[desktop] wallpaper=");
                serial::put_str(wallpaper(selected).name);
                serial::put_str("\n");
                Redraw::Scene
            }
            TerminalAction::Shutdown => shutdown(),
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> Redraw {
        let _secondary_pressed = event.right || event.middle;
        self.mouse_x = (self.mouse_x + event.dx as i32)
            .clamp(0, self.framebuffer.width().saturating_sub(1) as i32);
        self.mouse_y = (self.mouse_y + event.dy as i32)
            .clamp(0, self.framebuffer.height().saturating_sub(1) as i32);

        match self.interaction {
            WindowInteraction::Move {
                window,
                offset_x,
                offset_y,
            } => {
                if !event.left {
                    return self.finish_window_gesture(window, false);
                }
                let Some(previous) = self.window_rect(window) else {
                    self.interaction = WindowInteraction::None;
                    return Redraw::Scene;
                };
                let first = !self.drag_preview_visible;
                let command = WindowCommand::move_to(
                    window,
                    self.mouse_x - offset_x,
                    self.mouse_y - offset_y,
                );
                let _ = self.apply_window_command(command);
                self.previous_left = event.left;
                self.drag_frames = self.drag_frames.saturating_add(1);
                self.drag_packets = self.drag_packets.saturating_add(u32::from(event.packets));
                return Redraw::DragMove {
                    window,
                    previous,
                    first,
                };
            }
            WindowInteraction::Resize {
                window,
                edges,
                start_mouse_x,
                start_mouse_y,
                start,
            } => {
                if !event.left {
                    return self.finish_window_gesture(window, true);
                }
                let Some(previous) = self.window_rect(window) else {
                    self.interaction = WindowInteraction::None;
                    return Redraw::Scene;
                };
                let first = !self.drag_preview_visible;
                let (minimum_width, minimum_height) = self
                    .window_kind(window)
                    .unwrap_or(ApplicationKind::Terminal)
                    .minimum_size();
                let requested = resize_from_edges(
                    start,
                    edges,
                    self.mouse_x - start_mouse_x,
                    self.mouse_y - start_mouse_y,
                    minimum_width,
                    minimum_height,
                );
                let _ = self.apply_window_command(WindowCommand::resize(window, requested));
                self.previous_left = event.left;
                self.drag_frames = self.drag_frames.saturating_add(1);
                self.drag_packets = self.drag_packets.saturating_add(u32::from(event.packets));
                return Redraw::DragMove {
                    window,
                    previous,
                    first,
                };
            }
            WindowInteraction::None => {}
        }

        let was_left = self.previous_left;
        let pressed = event.left && !was_left;
        let released = !event.left && was_left;
        self.previous_left = event.left;
        if released {
            self.desktop_icon_pressed = false;
        }

        if !pressed {
            let Some(id) = self.focused else {
                return Redraw::None;
            };
            if self.window_kind(id) == Some(ApplicationKind::UiShowcase)
                && self.window_is_visible(id)
                && (self
                    .window_content_rect(id)
                    .is_some_and(|rect| rect.contains(self.mouse_x, self.mouse_y))
                    || released)
            {
                let kind = if released {
                    UiPointerKind::Up
                } else {
                    UiPointerKind::Move
                };
                let x = self.mouse_x;
                let y = self.mouse_y;
                let changed = match self.application_mut(id) {
                    Some(Application::UiShowcase(showcase)) => showcase.pointer(kind, x, y, 0),
                    _ => false,
                };
                if changed {
                    return Redraw::Scene;
                }
            }
            return Redraw::None;
        }

        let x = self.mouse_x;
        let y = self.mouse_y;
        // Один marker на mouse-down (не на каждый movement packet) делает
        // GUI-тесты и реальные bug reports воспроизводимыми: видно координаты
        // после применённой sensitivity/acceleration и верхнее окно hit-test.
        serial::put_str("[pointer] down x=");
        serial::put_u32(x as u32);
        serial::put_str(" y=");
        serial::put_u32(y as u32);
        serial::put_str(" top=0x");
        serial::put_hex(self.top_window_at(x, y).map_or(0, |window| window.0));
        serial::put_str("\n");
        if self.start_button().contains(x, y) {
            self.start_open = !self.start_open;
            return Redraw::Scene;
        }
        if self.start_open {
            if self.start_terminal_item().contains(x, y) {
                let _ = self.spawn_application(ApplicationKind::Terminal);
                self.start_open = false;
                return Redraw::Scene;
            }
            if self.start_ui_item().contains(x, y) {
                let _ = self.spawn_application(ApplicationKind::UiShowcase);
                self.start_open = false;
                return Redraw::Scene;
            }
            if self.start_shutdown_item().contains(x, y) {
                shutdown();
            }
            self.start_open = false;
            return Redraw::Scene;
        }
        if self.desktop_terminal_icon().contains(x, y) {
            self.desktop_icon_selected = true;
            self.desktop_icon_pressed = true;
            let click = self.click_tracker.pressed(
                arch::monotonic_milliseconds(),
                x,
                y,
                self.input.mouse_settings(),
            );
            if click == ClickKind::Double {
                let _ = self.spawn_application(ApplicationKind::Terminal);
                serial::put_str("[desktop] new terminal requested by double-click\n");
            }
            return Redraw::Scene;
        }
        self.desktop_icon_selected = false;

        if let Some(id) = self.task_window_at(x, y) {
            if self.window_is_minimized(id) {
                let _ = self.apply_window_command(WindowCommand::restore(id));
                self.focus_window(id);
            } else if self.focused == Some(id) {
                let _ = self.apply_window_command(WindowCommand::minimize(id));
                self.focused = self.top_visible_window();
            } else {
                self.focus_window(id);
            }
            return Redraw::Scene;
        }

        let Some(id) = self.top_window_at(x, y) else {
            return Redraw::None;
        };
        let focus_changed = self.focus_window(id);
        let (minimize, maximize, close) = self.window_controls(id);
        if close.is_some_and(|rect| rect.contains(x, y)) {
            self.close_window(id);
            return Redraw::Scene;
        }
        if minimize.is_some_and(|rect| rect.contains(x, y)) {
            let _ = self.apply_window_command(WindowCommand::minimize(id));
            self.focused = self.top_visible_window();
            serial::put_str("[wm] window minimized id=0x");
            serial::put_hex(id.0);
            serial::put_str("\n");
            return Redraw::Scene;
        }
        if maximize.is_some_and(|rect| rect.contains(x, y)) {
            self.toggle_maximize(id);
            return Redraw::Scene;
        }

        let Some(model_rect) = self.window_model_rect(id) else {
            return Redraw::None;
        };
        let resize_edges = if self.window_style(id).is_some_and(|style| {
            style.contains(WindowStyle::RESIZABLE) && !self.window_is_maximized(id)
        }) {
            hit_test_resize(model_rect, x, y, RESIZE_BORDER)
        } else {
            ResizeEdges::NONE
        };
        if !resize_edges.is_empty() {
            self.interaction = WindowInteraction::Resize {
                window: id,
                edges: resize_edges,
                start_mouse_x: x,
                start_mouse_y: y,
                start: model_rect,
            };
            self.begin_window_gesture(id);
            serial::put_str("[wm] resize started id=0x");
            serial::put_hex(id.0);
            serial::put_str("\n");
            return Redraw::None;
        }

        if self.window_kind(id) == Some(ApplicationKind::UiShowcase)
            && self
                .window_content_rect(id)
                .is_some_and(|content| content.contains(x, y))
        {
            let changed = match self.application_mut(id) {
                Some(Application::UiShowcase(showcase)) => {
                    showcase.pointer(UiPointerKind::Down, x, y, 0)
                }
                _ => false,
            };
            return if changed || focus_changed {
                Redraw::Scene
            } else {
                Redraw::None
            };
        }

        let Some(rect) = self.window_rect(id) else {
            return Redraw::None;
        };
        let title = Rect::new(rect.x, rect.y, rect.width.saturating_sub(100), TITLE_HEIGHT);
        let movable = self.window_style(id).is_some_and(|style| {
            style.contains(WindowStyle::TITLE_BAR) && style.contains(WindowStyle::MOVABLE)
        });
        if movable && title.contains(x, y) && !self.window_is_maximized(id) {
            self.interaction = WindowInteraction::Move {
                window: id,
                offset_x: x - rect.x,
                offset_y: y - rect.y,
            };
            self.begin_window_gesture(id);
            serial::put_str("[wm] drag started id=0x");
            serial::put_hex(id.0);
            serial::put_str("\n");
            return Redraw::None;
        }
        if focus_changed {
            Redraw::Scene
        } else {
            Redraw::None
        }
    }

    fn update_cursor_hint(&mut self) {
        let x = self.mouse_x;
        let y = self.mouse_y;
        let kind = match self.interaction {
            WindowInteraction::Move { .. } => PointerCursor::Grabbing,
            WindowInteraction::Resize { edges, .. } => cursor_for_resize(edges),
            WindowInteraction::None => {
                if self.start_button().contains(x, y)
                    || self.start_open
                        && (self.start_terminal_item().contains(x, y)
                            || self.start_ui_item().contains(x, y)
                            || self.start_shutdown_item().contains(x, y))
                    || self.task_window_at(x, y).is_some()
                    || self.desktop_terminal_icon().contains(x, y)
                    || self.desktop_trash_icon().contains(x, y)
                {
                    PointerCursor::Link
                } else if let Some(id) = self.top_window_at(x, y) {
                    let model = self
                        .window_model_rect(id)
                        .unwrap_or(WindowRect::new(0, 0, 0, 0));
                    if self.window_style(id).is_some_and(|style| {
                        style.contains(WindowStyle::RESIZABLE) && !self.window_is_maximized(id)
                    }) {
                        let edges = hit_test_resize(model, x, y, RESIZE_BORDER);
                        if !edges.is_empty() {
                            self.cursor.set_automatic_kind(cursor_for_resize(edges));
                            return;
                        }
                    }
                    let (minimize, maximize, close) = self.window_controls(id);
                    if minimize.is_some_and(|rect| rect.contains(x, y))
                        || maximize.is_some_and(|rect| rect.contains(x, y))
                        || close.is_some_and(|rect| rect.contains(x, y))
                    {
                        PointerCursor::Link
                    } else if self
                        .window_content_rect(id)
                        .is_some_and(|content| content.contains(x, y))
                    {
                        if self.window_kind(id) == Some(ApplicationKind::Terminal) {
                            PointerCursor::Text
                        } else {
                            PointerCursor::Link
                        }
                    } else if self.window_style(id).is_some_and(|style| {
                        style.contains(WindowStyle::TITLE_BAR)
                            && self.window_rect(id).is_some_and(|rect| {
                                Rect::new(
                                    rect.x,
                                    rect.y,
                                    rect.width.saturating_sub(100),
                                    TITLE_HEIGHT,
                                )
                                .contains(x, y)
                            })
                    }) {
                        PointerCursor::Grab
                    } else {
                        PointerCursor::Arrow
                    }
                } else {
                    PointerCursor::Arrow
                }
            }
        };
        self.cursor.set_automatic_kind(kind);
    }

    fn spawn_application(&mut self, kind: ApplicationKind) -> Option<WindowId> {
        if self.window_count == MAX_WINDOWS {
            serial::put_str("[wm] create rejected: window registry full\n");
            return None;
        }
        let slot_index = self.windows.iter().position(Option::is_none)?;
        let id = WindowId::new(self.next_window_id.max(1));
        self.next_window_id = self.next_window_id.wrapping_add(1).max(1);
        let (minimum_width, minimum_height) = kind.minimum_size();
        let requested = self.default_window_rect(kind);
        let (model, shown) = ManagedWindow::create(
            id,
            WindowCreateRequest::standard(window_rect(requested), minimum_width, minimum_height),
            self.window_work_area(),
        )
        .ok()?;
        let content = content_rect_for(&model);
        let application = match kind {
            ApplicationKind::Terminal => {
                Application::Terminal(Terminal::new(self.usable_ram_mib, self.initramfs))
            }
            ApplicationKind::UiShowcase => Application::UiShowcase(UiShowcase::new(content)),
        };
        let memory = ApplicationMemory::new(application)?;
        let frames = memory.frames();
        self.windows[slot_index] = Some(WindowSlot {
            model,
            application: memory,
        });
        self.z_order[self.window_count] = id;
        self.task_order[self.window_count] = id;
        self.window_count += 1;
        self.focused = Some(id);
        self.cascade = self.cascade.wrapping_add(1);
        self.push_window_event(shown);
        serial::put_str("[app] spawn id=0x");
        serial::put_hex(id.0);
        serial::put_str(" kind=");
        serial::put_str(kind.task_label());
        serial::put_str(" private-frames=");
        serial::put_u32(frames as u32);
        serial::put_str(" windows=");
        serial::put_u32(self.window_count as u32);
        if let Some(rect) = self.window_model_rect(id) {
            serial::put_str(" rect=");
            serial::put_u32(rect.x.max(0) as u32);
            serial::put_str(",");
            serial::put_u32(rect.y.max(0) as u32);
            serial::put_str(",");
            serial::put_u32(rect.width);
            serial::put_str("x");
            serial::put_u32(rect.height);
        }
        serial::put_str("\n");
        Some(id)
    }

    fn close_window(&mut self, id: WindowId) {
        let request = self
            .window_slot_mut(id)
            .and_then(|slot| slot.model.request_close().ok());
        if let Some(event) = request {
            self.push_window_event(event);
        }
        let _ = self.apply_window_command(WindowCommand::close(id));
        self.destroy_window(id);
    }

    fn destroy_window(&mut self, id: WindowId) {
        let Some(slot_index) = self.window_slot_index(id) else {
            return;
        };
        let (kind, frames) = self.windows[slot_index]
            .as_ref()
            .map(|slot| (slot.kind(), slot.application.frames()))
            .unwrap_or((ApplicationKind::Terminal, 0));
        // take вызывает Drop ApplicationMemory: app state исчезает, кадры
        // возвращаются allocator'у до публикации нового focused window.
        let destroyed = self.windows[slot_index].take();
        drop(destroyed);
        remove_id(&mut self.z_order, &mut self.window_count, id);
        let mut task_count = self.window_count + 1;
        remove_id(&mut self.task_order, &mut task_count, id);
        debug_assert_eq!(task_count, self.window_count);
        if self.focused == Some(id) {
            self.focused = self.top_visible_window();
        }
        serial::put_str("[app] exit id=0x");
        serial::put_hex(id.0);
        serial::put_str(" kind=");
        serial::put_str(kind.task_label());
        serial::put_str(" released-frames=");
        serial::put_u32(frames as u32);
        serial::put_str(" windows=");
        serial::put_u32(self.window_count as u32);
        serial::put_str("\n");
    }

    fn default_window_rect(&self, kind: ApplicationKind) -> Rect {
        let work_height = self.framebuffer.height().saturating_sub(TASKBAR_HEIGHT);
        let (minimum_width, minimum_height) = kind.minimum_size();
        // Сохраняем просторную geometry прежнего desktop: терминал на
        // 1280x800 получает 1040x640. Cascade меняет только позицию новых
        // экземпляров, а не неожиданно уменьшает рабочую область приложения.
        let width = fit_window_extent(self.framebuffer.width(), 180, minimum_width, 1040);
        let height = fit_window_extent(work_height, 114, minimum_height, 650);
        let step = (self.cascade % 10) as i32 * 28;
        let work = self.window_work_area();
        let max_x = work
            .x
            .saturating_add(work.width.saturating_sub(width) as i32);
        let max_y = work
            .y
            .saturating_add(work.height.saturating_sub(height) as i32);
        let x = (120 + step).clamp(work.x, max_x.max(work.x));
        let y = (57 + step).clamp(work.y, max_y.max(work.y));
        Rect::new(x, y, width, height)
    }

    fn focus_window(&mut self, id: WindowId) -> bool {
        if self.window_slot_index(id).is_none() {
            return false;
        }
        let changed = self.focused != Some(id) || self.z_order[self.window_count - 1] != id;
        if let Some(position) = self.z_order[..self.window_count]
            .iter()
            .position(|candidate| *candidate == id)
        {
            for index in position..self.window_count - 1 {
                self.z_order[index] = self.z_order[index + 1];
            }
            self.z_order[self.window_count - 1] = id;
        }
        self.focused = Some(id);
        if changed {
            serial::put_str("[wm] focus id=0x");
            serial::put_hex(id.0);
            serial::put_str("\n");
        }
        changed
    }

    fn top_visible_window(&self) -> Option<WindowId> {
        self.z_order[..self.window_count]
            .iter()
            .rev()
            .copied()
            .find(|id| self.window_is_visible(*id))
    }

    fn top_window_at(&self, x: i32, y: i32) -> Option<WindowId> {
        self.z_order[..self.window_count]
            .iter()
            .rev()
            .copied()
            .find(|id| {
                self.window_slot(*id).is_some_and(|slot| {
                    slot.model.is_visible()
                        && (video_rect(slot.model.rect()).contains(x, y)
                            || !hit_test_resize(slot.model.rect(), x, y, RESIZE_BORDER).is_empty())
                })
            })
    }

    fn task_window_at(&self, x: i32, y: i32) -> Option<WindowId> {
        (0..self.window_count).find_map(|index| {
            let id = self.task_order[index];
            self.task_button(index, self.window_count)
                .contains(x, y)
                .then_some(id)
        })
    }

    fn window_slot_index(&self, id: WindowId) -> Option<usize> {
        self.windows
            .iter()
            .position(|candidate| candidate.as_ref().is_some_and(|slot| slot.model.id() == id))
    }

    fn window_slot(&self, id: WindowId) -> Option<&WindowSlot> {
        self.windows.get(self.window_slot_index(id)?)?.as_ref()
    }

    fn window_slot_mut(&mut self, id: WindowId) -> Option<&mut WindowSlot> {
        let index = self.window_slot_index(id)?;
        self.windows.get_mut(index)?.as_mut()
    }

    fn application_mut(&mut self, id: WindowId) -> Option<&mut Application> {
        Some(self.window_slot_mut(id)?.application.get_mut())
    }

    fn window_kind(&self, id: WindowId) -> Option<ApplicationKind> {
        Some(self.window_slot(id)?.kind())
    }

    fn window_style(&self, id: WindowId) -> Option<WindowStyle> {
        Some(self.window_slot(id)?.model.style())
    }

    fn window_model_rect(&self, id: WindowId) -> Option<WindowRect> {
        Some(self.window_slot(id)?.model.rect())
    }

    fn window_rect(&self, id: WindowId) -> Option<Rect> {
        self.window_model_rect(id).map(video_rect)
    }

    fn window_content_rect(&self, id: WindowId) -> Option<Rect> {
        Some(content_rect_for(&self.window_slot(id)?.model))
    }

    fn window_is_visible(&self, id: WindowId) -> bool {
        self.window_slot(id)
            .is_some_and(|slot| slot.model.is_visible())
    }

    fn window_is_minimized(&self, id: WindowId) -> bool {
        self.window_slot(id)
            .is_some_and(|slot| slot.model.is_minimized())
    }

    fn window_is_maximized(&self, id: WindowId) -> bool {
        self.window_slot(id)
            .is_some_and(|slot| slot.model.is_maximized())
    }

    fn window_work_area(&self) -> WindowRect {
        WindowRect::new(
            8,
            8,
            self.framebuffer.width().saturating_sub(16).max(1),
            self.framebuffer
                .height()
                .saturating_sub(TASKBAR_HEIGHT + 16)
                .max(1),
        )
    }

    fn apply_window_command(&mut self, command: WindowCommand) -> bool {
        let work_area = self.window_work_area();
        let event = self
            .window_slot_mut(command.window)
            .and_then(|slot| slot.model.apply(command, work_area).ok());
        if let Some(event) = event {
            self.push_window_event(event);
            true
        } else {
            false
        }
    }

    fn push_window_event(&mut self, event: WindowEvent) {
        if !self.window_events.push(event) {
            self.dispatch_window_events();
            let _ = self.window_events.push(event);
        }
    }

    fn dispatch_window_events(&mut self) {
        while let Some(event) = self.window_events.pop() {
            if !matches!(event.kind, window_event::MOVED | window_event::RESIZED) {
                serial::put_str("[window-event] id=0x");
                serial::put_hex(event.window.0);
                serial::put_str(" kind=");
                serial::put_u32(u32::from(event.kind));
                serial::put_str(" state=");
                serial::put_u32(u32::from(event.state));
                serial::put_str("\n");
            }
        }
    }

    fn begin_window_gesture(&mut self, window: WindowId) {
        self.drag_frames = 0;
        self.drag_packets = 0;
        self.drag_present_pixels = 0;
        self.drag_preview_visible = false;
        // Cache содержит desktop и все остальные окна. Поэтому лёгкий drag
        // preview не стирает перекрытые приложения и не требует full redraw.
        self.render_base();
        for index in 0..self.window_count {
            let id = self.z_order[index];
            if id != window && self.window_is_visible(id) {
                self.render_window(id);
            }
        }
        let _ = self.framebuffer.cache_background();
        self.render_window(window);
    }

    fn finish_window_gesture(&mut self, window: WindowId, resized: bool) -> Redraw {
        self.interaction = WindowInteraction::None;
        let preview = self.window_rect(window).unwrap_or(Rect::new(0, 0, 0, 0));
        let visible = self.drag_preview_visible;
        self.drag_preview_visible = false;
        self.previous_left = false;
        if resized {
            if let Some(content) = self.window_content_rect(window) {
                if let Some(Application::UiShowcase(showcase)) = self.application_mut(window) {
                    showcase.resize(content);
                }
            }
        }
        Redraw::DragEnd {
            window,
            preview,
            visible,
            resized,
        }
    }

    fn relayout_after_mode_set(&mut self) {
        let screen_width = self.framebuffer.width();
        let screen_height = self.framebuffer.height();
        self.cursor.invalidate();
        self.mouse_x = self.mouse_x.clamp(0, screen_width.saturating_sub(1) as i32);
        self.mouse_y = self
            .mouse_y
            .clamp(0, screen_height.saturating_sub(1) as i32);
        self.interaction = WindowInteraction::None;
        self.drag_preview_visible = false;
        let work_area = self.window_work_area();
        for index in 0..MAX_WINDOWS {
            let event = if let Some(slot) = self.windows[index].as_mut() {
                let event = slot.model.reflow(work_area);
                let content = content_rect_for(&slot.model);
                if let Application::UiShowcase(showcase) = slot.application.get_mut() {
                    showcase.resize(content);
                }
                Some(event)
            } else {
                None
            };
            if let Some(event) = event {
                self.push_window_event(event);
            }
        }
    }

    fn toggle_maximize(&mut self, id: WindowId) {
        let command = if self.window_is_maximized(id) {
            WindowCommand::restore(id)
        } else {
            WindowCommand::maximize(id)
        };
        let _ = self.apply_window_command(command);
    }

    fn render_all(&mut self) {
        self.render_scene();
        self.cursor
            .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
        self.framebuffer.present();
    }

    fn render_scene(&mut self) {
        self.render_base();
        let _ = self.framebuffer.cache_background();
        for index in 0..self.window_count {
            let id = self.z_order[index];
            if self.window_is_visible(id) {
                self.render_window(id);
            }
        }
        if self.start_open {
            self.render_start_menu();
        }
    }

    fn render_base(&mut self) {
        self.render_wallpaper();
        self.render_desktop_icons();
        self.render_taskbar();
    }

    fn render_drag_preview(&mut self, window: WindowId, previous: Rect, first: bool) -> bool {
        if !self.framebuffer.has_background_cache() {
            self.render_scene();
            return false;
        }
        if first {
            let _ = self.framebuffer.restore_background(window_damage(previous));
        } else {
            self.restore_preview(previous);
        }
        if let Some(rect) = self.window_rect(window) {
            self.draw_drag_preview(window, rect);
        }
        self.drag_preview_visible = true;
        true
    }

    fn render_drag_end(&mut self, window: WindowId, preview: Rect, visible: bool) -> bool {
        if !visible {
            return self.framebuffer.has_background_cache();
        }
        if !self.framebuffer.has_background_cache() {
            self.render_scene();
            return false;
        }
        self.restore_preview(preview);
        self.render_window(window);
        true
    }

    fn draw_drag_preview(&mut self, window: WindowId, rect: Rect) {
        let title = self
            .window_kind(window)
            .map_or("RUSTOS", ApplicationKind::title);
        self.framebuffer.fill_rect(
            Rect::new(rect.x, rect.y, rect.width, TITLE_HEIGHT),
            Color::rgb(28, 43, 62),
        );
        self.framebuffer.border(rect, Theme::ACCENT);
        font::draw_text(
            &mut self.framebuffer,
            rect.x + 12,
            rect.y + 11,
            title,
            Theme::TEXT,
            font::UI_TITLE,
        );
    }

    fn restore_preview(&mut self, rect: Rect) {
        for damage in preview_damage(rect) {
            let _ = self.framebuffer.restore_background(damage);
        }
    }

    fn present_preview(&mut self, rect: Rect) {
        let bounds = Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
        let mut region = DamageRegion::<4>::new(bounds);
        for damaged in preview_damage(rect) {
            region.add(damaged);
        }
        self.drag_present_pixels = self
            .drag_present_pixels
            .saturating_add(region.covered_pixels());
        self.framebuffer.present_damage(&region);
    }

    fn present_drag_rect(&mut self, rect: Rect) {
        self.drag_present_pixels = self.drag_present_pixels.saturating_add(clipped_area(
            rect,
            self.framebuffer.width(),
            self.framebuffer.height(),
        ));
        self.framebuffer.present_rect(rect);
    }

    fn present_drag_full(&mut self) {
        self.drag_present_pixels = self.drag_present_pixels.saturating_add(
            u64::from(self.framebuffer.width()) * u64::from(self.framebuffer.height()),
        );
        self.framebuffer.present();
    }

    fn log_drag_finished(&self, window: WindowId, resized: bool) {
        serial::put_str(if resized {
            "[wm] resize finished id=0x"
        } else {
            "[wm] drag finished id=0x"
        });
        serial::put_hex(window.0);
        serial::put_str(" frames=");
        serial::put_u32(self.drag_frames);
        serial::put_str(" packets=");
        serial::put_u32(self.drag_packets);
        serial::put_str(" present-kpx=");
        serial::put_u32((self.drag_present_pixels / 1000) as u32);
        serial::put_str(" compositor=");
        serial::put_str(if self.framebuffer.has_background_cache() {
            "layer-cache"
        } else {
            "full"
        });
        serial::put_str("\n");
    }

    fn render_wallpaper(&mut self) {
        let width = self.framebuffer.width();
        let height = self.framebuffer.height().saturating_sub(TASKBAR_HEIGHT);
        self.framebuffer
            .draw_wallpaper(Rect::new(0, 0, width, height), wallpaper(self.wallpaper));
        let branding_x = self.framebuffer.width() as i32 - 210;
        let branding_y = self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 - 28;
        font::draw_text(
            &mut self.framebuffer,
            branding_x + 1,
            branding_y + 1,
            arch::ARCH_NAME,
            Color::rgb(8, 14, 22),
            font::UI_SMALL.italic(),
        );
        font::draw_text(
            &mut self.framebuffer,
            branding_x,
            branding_y,
            arch::ARCH_NAME,
            Color::rgb(221, 238, 244),
            font::UI_SMALL.italic(),
        );
    }

    fn render_desktop_icons(&mut self) {
        let terminal = self.desktop_terminal_icon();
        if self.desktop_icon_selected {
            self.framebuffer
                .fill_rect(terminal, Color::rgb(35, 81, 105));
            self.framebuffer.border(terminal, Theme::ACCENT);
        }
        self.draw_system_icon(
            IconKind::Terminal,
            Rect::new(terminal.x + 12, terminal.y + 3, 48, 48),
        );
        font::draw_text(
            &mut self.framebuffer,
            terminal.x + 5,
            terminal.y + 61,
            "TERMINAL",
            Theme::TEXT,
            font::UI_SMALL.bold(),
        );
        let trash = self.desktop_trash_icon();
        self.draw_system_icon(
            IconKind::Trash,
            Rect::new(trash.x + 12, trash.y + 2, 48, 52),
        );
        font::draw_text(
            &mut self.framebuffer,
            trash.x + 16,
            trash.y + 63,
            "TRASH",
            Theme::TEXT,
            font::UI_SMALL.bold(),
        );
    }

    fn draw_system_icon(&mut self, kind: IconKind, rect: Rect) {
        self.icon_packs.active().unwrap_or(CLASSIC_ICON_PACK).draw(
            &mut self.framebuffer,
            kind,
            rect,
        );
    }

    fn render_window(&mut self, id: WindowId) {
        let Some(slot) = self.window_slot(id) else {
            return;
        };
        let rect = video_rect(slot.model.rect());
        let style = slot.model.style();
        let maximized = slot.model.is_maximized();
        let kind = slot.kind();
        let focused = self.focused == Some(id);

        self.framebuffer.fill_rect(
            Rect::new(
                rect.x + rect.width as i32,
                rect.y + 8,
                WINDOW_SHADOW_RIGHT,
                rect.height,
            ),
            Color::rgb(7, 12, 20),
        );
        self.framebuffer.fill_rect(
            Rect::new(
                rect.x + 7,
                rect.y + rect.height as i32,
                rect.width,
                WINDOW_SHADOW_BOTTOM,
            ),
            Color::rgb(7, 12, 20),
        );
        Panel {
            rect,
            color: Theme::PANEL,
            border: style.contains(WindowStyle::BORDER).then_some(if focused {
                Theme::ACCENT
            } else {
                Theme::BORDER
            }),
        }
        .draw(&mut self.framebuffer);
        if style.contains(WindowStyle::TITLE_BAR) {
            self.framebuffer.horizontal_gradient(
                Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, TITLE_HEIGHT - 1),
                if focused {
                    Color::rgb(38, 62, 86)
                } else {
                    Color::rgb(30, 40, 54)
                },
                Color::rgb(19, 28, 42),
            );
            match kind {
                ApplicationKind::Terminal => self.draw_system_icon(
                    IconKind::Terminal,
                    Rect::new(rect.x + 8, rect.y + 6, 22, 22),
                ),
                ApplicationKind::UiShowcase => {
                    components::start_icon(&mut self.framebuffer, rect.x + 8, rect.y + 6)
                }
            }
            Label {
                rect: Rect::new(rect.x + 38, rect.y + 8, 310, 22),
                text: kind.title(),
                color: if focused {
                    Theme::TEXT
                } else {
                    Theme::TEXT_MUTED
                },
                style: font::UI_TITLE,
            }
            .draw(&mut self.framebuffer);
            let (minimize, maximize, close) = self.window_controls(id);
            if let Some(control) = minimize {
                Button {
                    rect: control,
                    label: "-",
                    hovered: focused && control.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: false,
                }
                .draw(&mut self.framebuffer);
            }
            if let Some(control) = maximize {
                Button {
                    rect: control,
                    label: if maximized { "[]" } else { "+" },
                    hovered: focused && control.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: false,
                }
                .draw(&mut self.framebuffer);
            }
            if let Some(control) = close {
                Button {
                    rect: control,
                    label: "X",
                    hovered: focused && control.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: true,
                }
                .draw(&mut self.framebuffer);
            }
        }
        if style.contains(WindowStyle::BORDER)
            && style.contains(WindowStyle::RESIZABLE)
            && !maximized
        {
            for inset in [4, 8, 12] {
                self.framebuffer.fill_rect(
                    Rect::new(
                        rect.x + rect.width as i32 - inset,
                        rect.y + rect.height as i32 - 2,
                        2,
                        2,
                    ),
                    Theme::ACCENT,
                );
            }
        }

        let content = content_rect_from(rect, style);
        let Some(index) = self.window_slot_index(id) else {
            return;
        };
        let (framebuffer, windows) = (&mut self.framebuffer, &mut self.windows);
        let Some(slot) = windows[index].as_mut() else {
            return;
        };
        match slot.application.get_mut() {
            Application::Terminal(terminal) => terminal.draw(framebuffer, content),
            Application::UiShowcase(showcase) => {
                showcase.resize(content);
                let _ = showcase.draw(framebuffer, true);
            }
        }
    }

    fn draw_focused_terminal_line(&mut self) -> Option<Rect> {
        let id = self.focused?;
        let content = self.window_content_rect(id)?;
        let index = self.window_slot_index(id)?;
        let (framebuffer, windows) = (&mut self.framebuffer, &mut self.windows);
        let slot = windows[index].as_mut()?;
        match slot.application.get_mut() {
            Application::Terminal(terminal) => terminal.draw_input_line(framebuffer, content),
            Application::UiShowcase(_) => None,
        }
    }

    fn render_taskbar(&mut self) {
        let y = self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32;
        self.framebuffer.fill_rect(
            Rect::new(0, y, self.framebuffer.width(), TASKBAR_HEIGHT),
            Color::rgb(13, 19, 30),
        );
        self.framebuffer
            .fill_rect(Rect::new(0, y, self.framebuffer.width(), 1), Theme::BORDER);
        let start = self.start_button();
        self.framebuffer.fill_rect(
            start,
            if self.start_open {
                Theme::PANEL_LIGHT
            } else {
                Theme::PANEL
            },
        );
        components::start_icon(&mut self.framebuffer, start.x + 14, start.y + 11);
        font::draw_text(
            &mut self.framebuffer,
            start.x + 47,
            start.y + 17,
            "START",
            Theme::TEXT,
            font::UI_NORMAL,
        );

        for index in 0..self.window_count {
            let id = self.task_order[index];
            let Some((kind, minimized)) = self
                .window_slot(id)
                .map(|slot| (slot.kind(), slot.model.is_minimized()))
            else {
                continue;
            };
            let task = self.task_button(index, self.window_count);
            self.framebuffer.fill_rect(
                task,
                if self.focused == Some(id) && !minimized {
                    Theme::PANEL_LIGHT
                } else {
                    Theme::PANEL
                },
            );
            match kind {
                ApplicationKind::Terminal => self.draw_system_icon(
                    IconKind::Terminal,
                    Rect::new(task.x + 6, task.y + 6, 26, 26),
                ),
                ApplicationKind::UiShowcase => {
                    components::start_icon(&mut self.framebuffer, task.x + 7, task.y + 8)
                }
            }
            if task.width >= 92 {
                font::draw_text(
                    &mut self.framebuffer,
                    task.x + 38,
                    task.y + 16,
                    kind.task_label(),
                    if minimized {
                        Theme::TEXT_MUTED
                    } else {
                        Theme::TEXT
                    },
                    font::UI_SMALL,
                );
            }
        }
        let status_x = self.framebuffer.width() as i32 - 150;
        font::draw_text(
            &mut self.framebuffer,
            status_x,
            y + 17,
            "CPU0  64-BIT",
            Theme::TEXT_MUTED,
            font::UI_SMALL,
        );
    }

    fn render_start_menu(&mut self) {
        let menu = self.start_menu();
        Panel {
            rect: menu,
            color: Color::rgb(19, 28, 42),
            border: Some(Theme::BORDER),
        }
        .draw(&mut self.framebuffer);
        font::draw_text(
            &mut self.framebuffer,
            menu.x + 18,
            menu.y + 18,
            "RUSTOS",
            Theme::ACCENT,
            font::UI_LARGE,
        );
        let terminal = self.start_terminal_item();
        self.framebuffer.fill_rect(terminal, Theme::PANEL_LIGHT);
        self.draw_system_icon(
            IconKind::Terminal,
            Rect::new(terminal.x + 10, terminal.y + 8, 34, 34),
        );
        font::draw_text(
            &mut self.framebuffer,
            terminal.x + 58,
            terminal.y + 20,
            "NEW TERMINAL",
            Theme::TEXT,
            font::UI_NORMAL,
        );
        let ui = self.start_ui_item();
        self.framebuffer.fill_rect(ui, Theme::PANEL_LIGHT);
        components::start_icon(&mut self.framebuffer, ui.x + 12, ui.y + 12);
        font::draw_text(
            &mut self.framebuffer,
            ui.x + 58,
            ui.y + 20,
            "NEW UI GALLERY",
            Theme::TEXT,
            font::UI_NORMAL,
        );
        let shutdown = self.start_shutdown_item();
        self.framebuffer.fill_rect(shutdown, Color::rgb(45, 31, 39));
        font::draw_text(
            &mut self.framebuffer,
            shutdown.x + 18,
            shutdown.y + 17,
            "SHUTDOWN",
            Color::rgb(245, 151, 157),
            font::UI_NORMAL,
        );
    }

    fn desktop_terminal_icon(&self) -> Rect {
        Rect::new(28, 35, 74, 86)
    }

    fn desktop_trash_icon(&self) -> Rect {
        Rect::new(28, 138, 74, 82)
    }

    fn start_button(&self) -> Rect {
        Rect::new(
            6,
            self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 + 4,
            112,
            TASKBAR_HEIGHT - 8,
        )
    }

    fn task_button(&self, index: usize, count: usize) -> Rect {
        let available = self.framebuffer.width().saturating_sub(126 + 160);
        let width = if count == 0 {
            180
        } else {
            (available / count as u32).min(180).max(22)
        };
        Rect::new(
            126 + index as i32 * width as i32,
            self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 + 4,
            width.saturating_sub(3),
            TASKBAR_HEIGHT - 8,
        )
    }

    fn start_menu(&self) -> Rect {
        Rect::new(
            6,
            self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 - 288,
            300,
            282,
        )
    }

    fn start_terminal_item(&self) -> Rect {
        let menu = self.start_menu();
        Rect::new(menu.x + 12, menu.y + 65, menu.width - 24, 52)
    }

    fn start_ui_item(&self) -> Rect {
        let menu = self.start_menu();
        Rect::new(menu.x + 12, menu.y + 123, menu.width - 24, 52)
    }

    fn start_shutdown_item(&self) -> Rect {
        let menu = self.start_menu();
        Rect::new(
            menu.x + 12,
            menu.y + menu.height as i32 - 58,
            menu.width - 24,
            44,
        )
    }

    fn window_controls(&self, id: WindowId) -> (Option<Rect>, Option<Rect>, Option<Rect>) {
        let Some(slot) = self.window_slot(id) else {
            return (None, None, None);
        };
        let style = slot.model.style();
        if !style.contains(WindowStyle::TITLE_BAR) {
            return (None, None, None);
        }
        let rect = video_rect(slot.model.rect());
        let y = rect.y + 4;
        let mut right = rect.x.saturating_add(rect.width as i32).saturating_sub(4);
        let close = place_window_control(style.contains(WindowStyle::BUTTON_CLOSE), &mut right, y);
        let maximize =
            place_window_control(style.contains(WindowStyle::BUTTON_MAXIMIZE), &mut right, y);
        let minimize =
            place_window_control(style.contains(WindowStyle::BUTTON_MINIMIZE), &mut right, y);
        (minimize, maximize, close)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Redraw {
    None,
    TerminalLine,
    Scene,
    DragMove {
        window: WindowId,
        previous: Rect,
        first: bool,
    },
    DragEnd {
        window: WindowId,
        preview: Rect,
        visible: bool,
        resized: bool,
    },
}

fn remove_id(order: &mut [WindowId; MAX_WINDOWS], count: &mut usize, id: WindowId) {
    let Some(position) = order[..*count]
        .iter()
        .position(|candidate| *candidate == id)
    else {
        return;
    };
    for index in position..count.saturating_sub(1) {
        order[index] = order[index + 1];
    }
    *count = count.saturating_sub(1);
    order[*count] = WindowId::new(0);
}

const fn video_rect(rect: WindowRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}

const fn window_rect(rect: Rect) -> WindowRect {
    WindowRect::new(rect.x, rect.y, rect.width, rect.height)
}

fn content_rect_for(window: &ManagedWindow) -> Rect {
    content_rect_from(video_rect(window.rect()), window.style())
}

fn content_rect_from(rect: Rect, style: WindowStyle) -> Rect {
    let border = u32::from(style.contains(WindowStyle::BORDER)) * 4;
    let top = if style.contains(WindowStyle::TITLE_BAR) {
        TITLE_HEIGHT
    } else {
        border
    };
    Rect::new(
        rect.x + border as i32,
        rect.y + top as i32,
        rect.width.saturating_sub(border * 2),
        rect.height.saturating_sub(top + border),
    )
}

fn place_window_control(enabled: bool, right: &mut i32, y: i32) -> Option<Rect> {
    if !enabled {
        return None;
    }
    *right = right.saturating_sub(28);
    let rect = Rect::new(*right, y, 28, 26);
    *right = right.saturating_sub(3);
    Some(rect)
}

fn window_damage(rect: Rect) -> Rect {
    Rect::new(
        rect.x,
        rect.y,
        rect.width.saturating_add(WINDOW_SHADOW_RIGHT),
        rect.height.saturating_add(WINDOW_SHADOW_BOTTOM),
    )
}

fn preview_damage(rect: Rect) -> [Rect; 4] {
    let body_height = rect.height.saturating_sub(TITLE_HEIGHT);
    [
        Rect::new(rect.x, rect.y, rect.width, TITLE_HEIGHT),
        Rect::new(rect.x, rect.y + TITLE_HEIGHT as i32, 2, body_height),
        Rect::new(
            rect.x + rect.width.saturating_sub(2) as i32,
            rect.y + TITLE_HEIGHT as i32,
            2,
            body_height,
        ),
        Rect::new(
            rect.x,
            rect.y + rect.height.saturating_sub(2) as i32,
            rect.width,
            2,
        ),
    ]
}

fn clipped_area(rect: Rect, width: u32, height: u32) -> u64 {
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = rect
        .x
        .saturating_add(rect.width as i32)
        .clamp(0, width as i32) as u32;
    let y1 = rect
        .y
        .saturating_add(rect.height as i32)
        .clamp(0, height as i32) as u32;
    u64::from(x1.saturating_sub(x0)) * u64::from(y1.saturating_sub(y0))
}

fn cursor_for_resize(edges: ResizeEdges) -> PointerCursor {
    let horizontal = edges.contains(ResizeEdges::LEFT) || edges.contains(ResizeEdges::RIGHT);
    let vertical = edges.contains(ResizeEdges::TOP) || edges.contains(ResizeEdges::BOTTOM);
    if horizontal && vertical {
        if edges.contains(ResizeEdges::LEFT) == edges.contains(ResizeEdges::TOP) {
            PointerCursor::ResizeNwSe
        } else {
            PointerCursor::ResizeNeSw
        }
    } else if horizontal {
        PointerCursor::ResizeHorizontal
    } else {
        PointerCursor::ResizeVertical
    }
}

fn cursor_name(kind: PointerCursor) -> &'static str {
    match kind {
        PointerCursor::Arrow => "ARROW",
        PointerCursor::Text => "TEXT",
        PointerCursor::Link => "LINK",
        PointerCursor::Grab => "GRAB",
        PointerCursor::Grabbing => "GRABBING",
        PointerCursor::Busy => "BUSY",
        PointerCursor::Crosshair => "CROSSHAIR",
        PointerCursor::NotAllowed => "FORBIDDEN",
        PointerCursor::ResizeHorizontal => "HRESIZE",
        PointerCursor::ResizeVertical => "VRESIZE",
        PointerCursor::ResizeNwSe => "NWSE",
        PointerCursor::ResizeNeSw => "NESW",
    }
}

fn fit_window_extent(available: u32, roomy_margin: u32, minimum: u32, maximum: u32) -> u32 {
    let hard_maximum = available.saturating_sub(16).max(1);
    let margin = if available >= minimum.saturating_add(roomy_margin) {
        roomy_margin
    } else {
        16
    };
    let lower = minimum.min(hard_maximum);
    let upper = maximum.min(hard_maximum).max(lower);
    available.saturating_sub(margin).clamp(lower, upper)
}

fn shutdown() -> ! {
    serial::put_str("[platform] shutdown requested\n");
    arch::power_off()
}

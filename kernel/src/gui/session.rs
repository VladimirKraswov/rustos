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
        desktop_settings::{DesktopSettings, DesktopSettingsAction, DesktopSettingsSnapshot},
        file_explorer::FileExplorer,
        gpu_demo::GpuDemo,
        shell_ui::{active_icon_or_default, ShellAction, ShellUi},
        terminal::{
            CursorCommand, CursorThemeName, IconThemeName, MouseCommand, Terminal, TerminalAction,
        },
        ui_showcase::UiShowcase,
    },
    arch, font,
    graphics::{Color, Framebuffer, Rect},
    gui::{
        chrome::{Button, Label, Panel, Theme, Widget},
        cursor::Cursor,
    },
    input::{Event, Key, MouseEvent, PlatformInput, PointerMotion},
    memory::{self, FrameBlock},
    process, serial,
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
    AURORA_ICON_PACK, CLASSIC_ICON_PACK, MIDNIGHT_ICON_PACK, MONO_ICON_PACK,
};
use rustos_system_ui::{FrameResult, Key as UiKey, PointerKind as UiPointerKind, WindowMetrics};
use rustos_video::{
    hit_test_resize, resize_from_edges, ColorMode, CpuPixelFormat, DamageRegion, DisplayDriver,
    DisplayMode, ManagedWindow, ModeSetError, ResizeEdges, Scanout, WindowEventQueue,
};

const TASKBAR_HEIGHT: u32 = 46;
const TITLE_HEIGHT: u32 = 34;
const RESIZE_BORDER: u32 = 6;
const WINDOW_SHADOW_RIGHT: u32 = 10;
const WINDOW_SHADOW_BOTTOM: u32 = 12;
const TERMINAL_MIN_WIDTH: u32 = 480;
const TERMINAL_MIN_HEIGHT: u32 = 300;
const GALLERY_MIN_WIDTH: u32 = 560;
const GALLERY_MIN_HEIGHT: u32 = 360;
const SETTINGS_MIN_WIDTH: u32 = 620;
const SETTINGS_MIN_HEIGHT: u32 = 560;
const EXPLORER_MIN_WIDTH: u32 = 940;
const EXPLORER_MIN_HEIGHT: u32 = 440;
const GPU_DEMO_MIN_WIDTH: u32 = 640;
const GPU_DEMO_MIN_HEIGHT: u32 = 430;

/// Bounded registry защищает ядро от исчерпания памяти одним GUI-клиентом.
/// Состояние приложений при этом выделяется динамически из frame allocator,
/// поэтому лимит можно менять независимо от размера kernel stack.
const MAX_WINDOWS: usize = 16;
const WINDOW_EVENT_CAPACITY: usize = 128;
/// Preferred mode, EDID timings и стандартные режимы virtio-gpu помещаются в
/// один bounded snapshot без heap allocation.
const DISPLAY_MODE_CAPACITY: usize = 48;
/// Сюда объединяются dirty rectangles одного приложения/shell и старого с
/// новым курсора. Переполнение безопасно схлопывается `DamageRegion` в один
/// bounding rectangle, но не приводит к скрытой allocation.
const INCREMENTAL_DAMAGE_CAPACITY: usize = 64;

pub fn run(info: &BootInfo) -> ! {
    let Some(mut framebuffer) = Framebuffer::from_boot(info) else {
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
        CpuPixelFormat::Rgb888 => "rgb888",
        CpuPixelFormat::Bgr888 => "bgr888",
        CpuPixelFormat::Argb8888 => "argb8888",
        CpuPixelFormat::Rgb565 => "rgb565",
        CpuPixelFormat::Grayscale8 => "gray8",
    });
    serial::put_str(" present=");
    serial::put_str(if capabilities.page_flip {
        "async-mailbox"
    } else {
        "immediate"
    });
    serial::put_str(" page-flip=");
    serial::put_str(if capabilities.page_flip { "yes" } else { "no" });
    serial::put_str("\n");
    serial::put_str(
        "[font] families=console,sans scripts=latin,cyrillic styles=regular,bold,italic sizes=10..48\n",
    );
    serial::put_str("[ui] constructing independent application sessions\n");
    // Наличие отдельной cursor plane не является условием GPU desktop.
    // Если устройство её не даёт, тот же cursor рисуется как обычная часть
    // GPU scene; CPU recovery нужен только при потере renderer'а.
    let gpu_ui = process::system_ui_gpu_available();
    framebuffer.set_gpu_recording(gpu_ui);
    serial::put_str("[system-ui] selected-backend=");
    serial::put_str(if gpu_ui {
        "gpu-renderd"
    } else {
        "cpu-recovery"
    });
    serial::put_str(" app-api=renderer-neutral\n");
    let mut session = DesktopSession::new(
        framebuffer,
        info.total_usable_ram() / (1024 * 1024),
        info.initramfs,
    );
    session.log_display_metrics();
    let _ = session.spawn_application(ApplicationKind::Terminal);
    serial::put_str("[ui] window server ready capacity=");
    serial::put_u32(MAX_WINDOWS as u32);
    serial::put_str("\n");
    serial::put_str("[gui] scheduling=input-first services=idle-quantum animation=last\n");
    session.render_all();
    serial::put_str("[gui] GUI_READY desktop=1 terminal=1 multiwindow=1 start=system-ui clock=");
    serial::put_str(session.shell.clock_source());
    serial::put_str(" mouse=");
    serial::put_str(session.input.backend_name());
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
    FileExplorer,
    UiShowcase,
    DesktopSettings,
    GpuDemo,
}

impl ApplicationKind {
    const fn title(self) -> &'static str {
        match self {
            Self::Terminal => "RustOS · Терминал",
            Self::FileExplorer => "RustOS · Проводник",
            Self::UiShowcase => "RustOS · Библиотека компонентов",
            Self::DesktopSettings => "RustOS · Параметры рабочего стола",
            Self::GpuDemo => "RustOS · Aurora 3D",
        }
    }

    const fn task_label(self) -> &'static str {
        match self {
            Self::Terminal => "Терминал",
            Self::FileExplorer => "Проводник",
            Self::UiShowcase => "Компоненты",
            Self::DesktopSettings => "Параметры",
            Self::GpuDemo => "Aurora 3D",
        }
    }

    /// Стабильный ASCII label машинного журнала. В отличие от локализованной
    /// подписи taskbar он является частью интеграционных test markers.
    const fn log_label(self) -> &'static str {
        match self {
            Self::Terminal => "TERMINAL",
            Self::FileExplorer => "EXPLORER",
            Self::UiShowcase => "UI GALLERY",
            Self::DesktopSettings => "SETTINGS",
            Self::GpuDemo => "AURORA 3D",
        }
    }

    const fn minimum_size(self) -> (u32, u32) {
        match self {
            Self::Terminal => (TERMINAL_MIN_WIDTH, TERMINAL_MIN_HEIGHT),
            Self::FileExplorer => (EXPLORER_MIN_WIDTH, EXPLORER_MIN_HEIGHT),
            Self::UiShowcase => (GALLERY_MIN_WIDTH, GALLERY_MIN_HEIGHT),
            Self::DesktopSettings => (SETTINGS_MIN_WIDTH, SETTINGS_MIN_HEIGHT),
            Self::GpuDemo => (GPU_DEMO_MIN_WIDTH, GPU_DEMO_MIN_HEIGHT),
        }
    }
}

/// Временное типизированное представление объекта, уже лежащего в его
/// собственных физических кадрах. Сам enum содержит только ссылку и поэтому
/// не копирует большой retained tree приложения на kernel stack.
enum Application<'a> {
    Terminal(&'a mut Terminal),
    FileExplorer(&'a mut FileExplorer),
    UiShowcase(&'a mut UiShowcase),
    DesktopSettings(&'a mut DesktopSettings),
    GpuDemo(&'a mut GpuDemo),
}

/// Heap ядру не нужен: объект приложения размещается в непрерывных физических
/// кадрах и уничтожается через Drop. Именно этот владелец превращает `close`
/// из визуального флага в настоящий lifecycle transition с освобождением RAM.
struct ApplicationMemory {
    pointer: *mut u8,
    block: FrameBlock,
    kind: ApplicationKind,
}

impl ApplicationMemory {
    /// Создаёт конкретный тип сразу в выделенном диапазоне. `factory`
    /// принципиально вызывается внутри `ptr::write`: large-return ABI может
    /// построить FileExplorer непосредственно в destination и не требует
    /// временного 200-KiB enum на ограниченном стеке ядра.
    fn new<T>(kind: ApplicationKind, factory: impl FnOnce() -> T) -> Option<Self> {
        let result = Self::allocate_for::<T>(kind)?;
        // SAFETY: allocate_for предоставил уникальный storage нужного типа.
        unsafe { result.pointer.cast::<T>().write(factory()) };
        Some(result)
    }

    fn new_file_explorer(
        kind: ApplicationKind,
        content: Rect,
        initramfs: BootInitramfs,
        ui_scale_milli: u16,
    ) -> Option<Self> {
        let result = Self::allocate_for::<FileExplorer>(kind)?;
        // SAFETY: storage ещё не опубликован и имеет размер FileExplorer.
        unsafe {
            FileExplorer::initialize_in_place(
                result.pointer.cast::<FileExplorer>(),
                content,
                initramfs,
                ui_scale_milli,
            )
        };
        Some(result)
    }

    fn new_gpu_demo(kind: ApplicationKind, now_ms: u64, instance_id: u32) -> Option<Self> {
        let result = Self::allocate_for::<GpuDemo>(kind)?;
        // SAFETY: allocate_for выделил уникальный диапазон полного размера;
        // большой pixel surface строится на месте без временного значения.
        unsafe {
            GpuDemo::initialize_in_place(result.pointer.cast::<GpuDemo>(), now_ms, instance_id)
        };
        Some(result)
    }

    fn allocate_for<T>(kind: ApplicationKind) -> Option<Self> {
        let bytes = u64::try_from(size_of::<T>()).ok()?;
        let frames = bytes.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE;
        let block = memory::allocate(frames.max(1), 1).ok()?;
        let pointer = block.phys as *mut u8;
        Some(Self {
            pointer,
            block,
            kind,
        })
    }

    fn get_mut(&mut self) -> Application<'_> {
        // SAFETY: kind записан вместе с объектом, pointer живёт до Drop, а
        // `&mut self` гарантирует единственный mutable access.
        unsafe {
            match self.kind {
                ApplicationKind::Terminal => {
                    Application::Terminal(&mut *self.pointer.cast::<Terminal>())
                }
                ApplicationKind::FileExplorer => {
                    Application::FileExplorer(&mut *self.pointer.cast::<FileExplorer>())
                }
                ApplicationKind::UiShowcase => {
                    Application::UiShowcase(&mut *self.pointer.cast::<UiShowcase>())
                }
                ApplicationKind::DesktopSettings => {
                    Application::DesktopSettings(&mut *self.pointer.cast::<DesktopSettings>())
                }
                ApplicationKind::GpuDemo => {
                    Application::GpuDemo(&mut *self.pointer.cast::<GpuDemo>())
                }
            }
        }
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
        // SAFETY: kind соответствует конкретному типу, записанному в new.
        unsafe {
            match self.kind {
                ApplicationKind::Terminal => ptr::drop_in_place(self.pointer.cast::<Terminal>()),
                ApplicationKind::FileExplorer => {
                    ptr::drop_in_place(self.pointer.cast::<FileExplorer>())
                }
                ApplicationKind::UiShowcase => {
                    ptr::drop_in_place(self.pointer.cast::<UiShowcase>())
                }
                ApplicationKind::DesktopSettings => {
                    ptr::drop_in_place(self.pointer.cast::<DesktopSettings>())
                }
                ApplicationKind::GpuDemo => ptr::drop_in_place(self.pointer.cast::<GpuDemo>()),
            }
        }
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
    const fn kind(&self) -> ApplicationKind {
        self.application.kind
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
    /// Режим, автоматически выбранный из EDID при старте. Ручной mode-set не
    /// перезаписывает рекомендацию, поэтому Settings всегда может её показать.
    recommended_display_mode: DisplayMode,
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
    /// Следующее окно, с которого начинается обход анимаций. Без cursor
    /// первый тяжёлый клиент навсегда лишал кадров все последующие окна.
    animation_cursor: usize,
    cascade: u32,
    window_events: WindowEventQueue<WINDOW_EVENT_CAPACITY>,
    interaction: WindowInteraction,
    cursor: Cursor,
    icon_packs: PackRegistry<IconPack, 8>,
    wallpaper: WallpaperId,
    mouse_x: i32,
    mouse_y: i32,
    previous_left: bool,
    previous_right: bool,
    /// Start/taskbar UI — такой же component runtime, как интерфейс обычного
    /// приложения; оконный сервер получает от него только команды.
    shell: ShellUi,
    drag_frames: u32,
    drag_packets: u32,
    drag_present_pixels: u64,
    drag_preview_visible: bool,
    incremental_application_logged: bool,
    incremental_shell_logged: bool,
    display_fallback_logged: bool,
    desktop_icon_selected: bool,
    desktop_gpu_demo_selected: bool,
    desktop_icon_pressed: bool,
    desktop_terminal_x: i32,
    desktop_terminal_y: i32,
    desktop_trash_x: i32,
    desktop_trash_y: i32,
    /// Геометрия layout и raster surface разделена даже в текущем режиме 1:1.
    /// Это не даёт compositor'у незаметно растягивать уже готовый bitmap и
    /// оставляет прямой путь к fractional HiDPI rasterization.
    display_metrics: WindowMetrics,
    /// Пользовательское увеличение элементов интерфейса. Это accessibility
    /// scale, а не device scale монитора.
    ui_scale_milli: u16,
    click_tracker: ClickTracker,
}

impl DesktopSession {
    fn new(framebuffer: Framebuffer, usable_ram_mib: u64, initramfs: BootInitramfs) -> Self {
        let screen_width = framebuffer.width();
        let screen_height = framebuffer.height();
        let recommended_display_mode = framebuffer.mode();
        let display_metrics = WindowMetrics::one_to_one(screen_width, screen_height);
        let mut icon_packs = PackRegistry::new();
        let _ = icon_packs.install(AURORA_ICON_PACK);
        let _ = icon_packs.install(CLASSIC_ICON_PACK);
        let _ = icon_packs.install(MIDNIGHT_ICON_PACK);
        let _ = icon_packs.install(MONO_ICON_PACK);
        let shell = ShellUi::new(
            screen_width,
            screen_height,
            TASKBAR_HEIGHT,
            arch::monotonic_milliseconds(),
        );
        serial::put_str("[clock] source=");
        serial::put_str(shell.clock_source());
        serial::put_str(" time=");
        serial::put_str(shell.clock_time());
        serial::put_str(" date=");
        serial::put_str(shell.clock_date());
        serial::put_str("\n");
        Self {
            framebuffer,
            recommended_display_mode,
            input: PlatformInput::new(),
            usable_ram_mib,
            initramfs,
            windows: [const { None }; MAX_WINDOWS],
            z_order: [WindowId::new(0); MAX_WINDOWS],
            task_order: [WindowId::new(0); MAX_WINDOWS],
            window_count: 0,
            focused: None,
            next_window_id: 1,
            animation_cursor: 0,
            cascade: 0,
            window_events: WindowEventQueue::new(),
            interaction: WindowInteraction::None,
            cursor: Cursor::new(),
            icon_packs,
            wallpaper: WallpaperId::SpringRiver,
            mouse_x: (screen_width / 2) as i32,
            mouse_y: (screen_height / 2) as i32,
            previous_left: false,
            previous_right: false,
            shell,
            drag_frames: 0,
            drag_packets: 0,
            drag_present_pixels: 0,
            drag_preview_visible: false,
            incremental_application_logged: false,
            incremental_shell_logged: false,
            display_fallback_logged: false,
            desktop_icon_selected: false,
            desktop_gpu_demo_selected: false,
            desktop_icon_pressed: false,
            desktop_terminal_x: 28,
            desktop_terminal_y: 35,
            desktop_trash_x: 28,
            desktop_trash_y: 138,
            display_metrics,
            ui_scale_milli: 1_000,
            click_tracker: ClickTracker::new(),
        }
    }

    fn log_display_metrics(&self) {
        serial::put_str("[display-metrics] logical=");
        serial::put_u32(self.display_metrics.logical_width());
        serial::put_str("x");
        serial::put_u32(self.display_metrics.logical_height());
        serial::put_str(" physical=");
        serial::put_u32(self.display_metrics.physical_width());
        serial::put_str("x");
        serial::put_u32(self.display_metrics.physical_height());
        serial::put_str(" device-scale-milli=");
        serial::put_u32(u32::from(self.display_metrics.device_scale_milli()));
        serial::put_str(" framebuffer=");
        serial::put_u32(self.framebuffer.width());
        serial::put_str("x");
        serial::put_u32(self.framebuffer.height());
        serial::put_str(" compositor-scale-milli=");
        serial::put_u32(u32::from(self.display_metrics.compositor_scale_milli()));
        serial::put_str("\n");
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

                if self.framebuffer.gpu_recording() {
                    let rendered = match redraw {
                        Redraw::None if self.framebuffer.hardware_cursor_supported() => {
                            // Hardware cursor plane двигается независимо от
                            // frame и не заставляет перерисовывать desktop.
                            self.cursor
                                .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                            true
                        }
                        Redraw::DragMove { window, first, .. }
                            if matches!(self.interaction, WindowInteraction::Move { .. }) =>
                        {
                            self.cursor
                                .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                            let transformed = !first && self.render_gpu_transform(window);
                            if !transformed {
                                self.drag_preview_visible = true;
                                self.render_gpu_frame()
                            } else {
                                true
                            }
                        }
                        _ => self.render_gpu_frame(),
                    };
                    if !rendered {
                        self.activate_cpu_recovery("render-service-failure");
                    }
                    self.dispatch_window_events();
                    continue;
                }

                let mut terminal_line = None;
                let mut incremental_damage = None;
                let mut drag_cached = false;
                match redraw {
                    Redraw::Scene => self.render_scene(),
                    Redraw::TerminalLine => {
                        terminal_line = self.draw_focused_terminal_line();
                    }
                    Redraw::Application(window) | Redraw::Ui(window) => {
                        incremental_damage = Some(self.render_application_incremental(window));
                    }
                    Redraw::Shell => {
                        incremental_damage = Some(self.render_shell_incremental());
                    }
                    Redraw::DesktopIcon => {
                        incremental_damage = Some(self.render_desktop_icon_incremental());
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
                        let mut damage = self.empty_frame_damage();
                        damage.add(old_cursor);
                        damage.add(self.cursor.rect());
                        self.framebuffer.present_damage(&damage);
                    }
                    Redraw::TerminalLine => {
                        let mut damage = self.empty_frame_damage();
                        damage.add(old_cursor);
                        if let Some(line) = terminal_line {
                            damage.add(line);
                        }
                        damage.add(self.cursor.rect());
                        self.framebuffer.present_damage(&damage);
                    }
                    Redraw::Application(_)
                    | Redraw::Ui(_)
                    | Redraw::Shell
                    | Redraw::DesktopIcon => {
                        let bounds =
                            Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
                        let mut damage = incremental_damage.unwrap_or_else(|| {
                            DamageRegion::<INCREMENTAL_DAMAGE_CAPACITY>::new(bounds)
                        });
                        // Курсор рисуется software-композитором поверх client
                        // surface, поэтому его старое и новое положение входят
                        // в тот же единственный incremental present.
                        damage.add(old_cursor);
                        damage.add(self.cursor.rect());
                        match redraw {
                            Redraw::Ui(_) if !self.incremental_application_logged => {
                                log_incremental_damage("application", &damage);
                                self.incremental_application_logged = true;
                            }
                            Redraw::Shell if !self.incremental_shell_logged => {
                                log_incremental_damage("shell", &damage);
                                self.incremental_shell_logged = true;
                            }
                            _ => {}
                        }
                        self.framebuffer.present_damage(&damage);
                    }
                    Redraw::DragMove {
                        window,
                        previous,
                        first: _,
                    } => {
                        let mut damage = self.empty_frame_damage();
                        if drag_cached {
                            self.record_drag_rect(&mut damage, window_damage(previous));
                            if let Some(current) = self.window_rect(window) {
                                self.record_drag_rect(&mut damage, window_damage(current));
                            }
                        } else {
                            self.record_drag_full(&mut damage);
                        }
                        damage.add(old_cursor);
                        damage.add(self.cursor.rect());
                        self.framebuffer.present_damage(&damage);
                    }
                    Redraw::DragEnd {
                        window,
                        visible,
                        resized,
                        ..
                    } => {
                        let mut damage = self.empty_frame_damage();
                        if visible {
                            if drag_cached {
                                if let Some(current) = self.window_rect(window) {
                                    self.record_drag_rect(&mut damage, window_damage(current));
                                }
                            } else {
                                self.record_drag_full(&mut damage);
                            }
                        } else {
                            damage.add(old_cursor);
                            damage.add(self.cursor.rect());
                        }
                        if !damage.is_empty() {
                            self.framebuffer.present_damage(&damage);
                        }
                        self.log_drag_finished(window, resized);
                    }
                    Redraw::Scene => self.framebuffer.present(),
                }
                self.dispatch_window_events();
            } else {
                // Driver/services получают bounded квант только после input.
                // При непрерывном потоке событий очередь дренируется по одному
                // report и всё равно регулярно становится пустой, не создавая
                // ни starvation сервисов, ни дополнительной input latency.
                if process::pump_interactive_services().is_err() && !self.display_fallback_logged {
                    serial::put_str(
                        "[supervisor] display stack unavailable; kernel desktop fallback active\n",
                    );
                    self.display_fallback_logged = true;
                }
                // Completion может освободить один из трёх scanout buffers,
                // пока входных событий нет. Продвигаем newest mailbox frame
                // здесь, а не из IRQ: input всегда имеет приоритет над копией
                // кадра и следующей анимацией.
                self.framebuffer.service_scanout();
                let now_ms = arch::monotonic_milliseconds();
                // Сначала полностью обслуживаем уже опубликованный input.
                // Медленный renderer не вправе запускать следующий animation
                // frame раньше ожидающего mouse/key event и тем самым делать
                // desktop неуправляемым при просадке частоты кадров.
                if let Some(window) = self.tick_animated_application(now_ms) {
                    if self.framebuffer.gpu_recording() {
                        let _ = window;
                        if !self.render_gpu_frame() {
                            self.activate_cpu_recovery("animation-submit-failure");
                        }
                        continue;
                    }
                    let old_cursor = self.cursor.rect();
                    self.cursor.restore(&mut self.framebuffer);
                    let mut damage = self.render_application_incremental(window);
                    self.cursor
                        .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                    damage.add(old_cursor);
                    damage.add(self.cursor.rect());
                    self.framebuffer.present_damage(&damage);
                    continue;
                }
                if self.shell.update_clock(now_ms) {
                    if self.framebuffer.gpu_recording() {
                        if !self.render_gpu_frame() {
                            self.activate_cpu_recovery("clock-submit-failure");
                        }
                        continue;
                    }
                    let old_cursor = self.cursor.rect();
                    self.cursor.restore(&mut self.framebuffer);
                    self.render_taskbar();
                    if self.shell.is_open() {
                        self.render_start_menu();
                    }
                    self.cursor
                        .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                    let mut damage = self.empty_frame_damage();
                    damage.add(old_cursor);
                    damage.add(Rect::new(
                        0,
                        self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32,
                        self.framebuffer.width(),
                        TASKBAR_HEIGHT,
                    ));
                    if self.shell.is_open() {
                        damage.add(self.shell.menu_rect());
                    }
                    damage.add(self.cursor.rect());
                    self.framebuffer.present_damage(&damage);
                    continue;
                }
                if self.cursor.animate(now_ms) {
                    if self.framebuffer.gpu_recording() {
                        if self.framebuffer.hardware_cursor_supported() {
                            self.cursor
                                .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                        } else if !self.render_gpu_frame() {
                            self.activate_cpu_recovery("cursor-submit-failure");
                        }
                        continue;
                    }
                    let old_cursor = self.cursor.rect();
                    self.cursor.restore(&mut self.framebuffer);
                    self.cursor
                        .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                    let mut damage = self.empty_frame_damage();
                    damage.add(old_cursor);
                    damage.add(self.cursor.rect());
                    self.framebuffer.present_damage(&damage);
                } else {
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn handle_key(&mut self, key: Key) -> Redraw {
        if self.shell.has_popup() {
            if matches!(key, Key::Escape) {
                self.shell.close_popups();
                serial::put_str("[desktop-menu] closed by keyboard\n");
                return Redraw::Scene;
            }
            let ui_key = match key {
                Key::Tab => Some(UiKey::Tab),
                Key::Enter => Some(UiKey::Enter),
                Key::Left => Some(UiKey::Left),
                Key::Right => Some(UiKey::Right),
                Key::Up => Some(UiKey::Up),
                Key::Down => Some(UiKey::Down),
                Key::PageUp => Some(UiKey::PageUp),
                Key::PageDown => Some(UiKey::PageDown),
                Key::Home => Some(UiKey::Home),
                Key::End => Some(UiKey::End),
                Key::Character(b' ') => Some(UiKey::Space),
                Key::Character(byte) if byte.is_ascii() => Some(UiKey::Character(char::from(byte))),
                Key::Escape | Key::Backspace | Key::Character(_) => None,
            };
            if let Some(ui_key) = ui_key {
                let result = self.shell.key(ui_key, false);
                if result.action != ShellAction::None {
                    return self.handle_shell_action(result.action);
                }
                if result.changed || result.consumed {
                    return if result.changed {
                        Redraw::Shell
                    } else {
                        Redraw::None
                    };
                }
            }
            // Открытое menu владеет keyboard scope: случайный символ не
            // должен одновременно попасть в terminal под ним.
            return Redraw::None;
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
                    Key::Left => UiKey::Left,
                    Key::Right => UiKey::Right,
                    Key::Up => UiKey::Up,
                    Key::Down => UiKey::Down,
                    Key::PageUp => UiKey::PageUp,
                    Key::PageDown => UiKey::PageDown,
                    Key::Home => UiKey::Home,
                    Key::End => UiKey::End,
                    Key::Character(b' ') => UiKey::Space,
                    Key::Character(byte) if byte.is_ascii() => UiKey::Character(char::from(byte)),
                    Key::Backspace | Key::Character(_) => return Redraw::None,
                };
                let changed = match self.application_mut(id) {
                    Some(Application::UiShowcase(showcase)) => showcase.key(key, false),
                    _ => false,
                };
                if changed {
                    Redraw::Ui(id)
                } else {
                    Redraw::None
                }
            }
            Some(ApplicationKind::DesktopSettings) => {
                let key = match key {
                    Key::Tab => UiKey::Tab,
                    Key::Enter => UiKey::Enter,
                    Key::Escape => UiKey::Escape,
                    Key::Left => UiKey::Left,
                    Key::Right => UiKey::Right,
                    Key::Up => UiKey::Up,
                    Key::Down => UiKey::Down,
                    Key::PageUp => UiKey::PageUp,
                    Key::PageDown => UiKey::PageDown,
                    Key::Home => UiKey::Home,
                    Key::End => UiKey::End,
                    Key::Character(b' ') => UiKey::Space,
                    Key::Character(byte) if byte.is_ascii() => UiKey::Character(char::from(byte)),
                    Key::Backspace | Key::Character(_) => return Redraw::None,
                };
                self.handle_settings_key(id, key)
            }
            Some(ApplicationKind::FileExplorer) => {
                let now_ms = arch::monotonic_milliseconds();
                let settings = self.input.mouse_settings();
                let changed = match self.application_mut(id) {
                    Some(Application::FileExplorer(explorer)) => {
                        explorer.key(key, now_ms, settings)
                    }
                    _ => false,
                };
                if changed {
                    Redraw::Ui(id)
                } else {
                    Redraw::None
                }
            }
            Some(ApplicationKind::Terminal) => self.handle_terminal_key(id, key),
            Some(ApplicationKind::GpuDemo) => Redraw::None,
            None => Redraw::None,
        }
    }

    fn handle_shell_action(&mut self, action: ShellAction) -> Redraw {
        match action {
            ShellAction::None => Redraw::None,
            ShellAction::ToggleStart => {
                self.shell.toggle();
                serial::put_str(if self.shell.is_open() {
                    "[start] opened component-runtime=system-ui-v1\n"
                } else {
                    "[start] closed component-runtime=system-ui-v1\n"
                });
                Redraw::Scene
            }
            ShellAction::OpenTerminal => {
                self.shell.set_open(false);
                let _ = self.spawn_application(ApplicationKind::Terminal);
                serial::put_str("[start] command=terminal\n");
                Redraw::Scene
            }
            ShellAction::OpenFileExplorer => {
                self.shell.set_open(false);
                let _ = self.spawn_application(ApplicationKind::FileExplorer);
                serial::put_str("[start] command=explorer\n");
                Redraw::Scene
            }
            ShellAction::OpenGallery => {
                self.shell.set_open(false);
                let _ = self.spawn_application(ApplicationKind::UiShowcase);
                serial::put_str("[start] command=ui-gallery\n");
                Redraw::Scene
            }
            ShellAction::OpenGpuDemo => {
                self.shell.set_open(false);
                self.launch_gpu_demo();
                Redraw::Scene
            }
            ShellAction::OpenDesktopSettings => {
                self.shell.set_open(false);
                let _ = self.open_or_focus_application(ApplicationKind::DesktopSettings);
                serial::put_str("[start] command=desktop-settings\n");
                Redraw::Scene
            }
            ShellAction::ArrangeDesktop => {
                self.shell.close_popups();
                self.arrange_desktop_icons();
                serial::put_str("[desktop-menu] command=arrange-icons\n");
                Redraw::Scene
            }
            ShellAction::OpenDesktopProperties => {
                self.shell.close_popups();
                let _ = self.open_or_focus_application(ApplicationKind::DesktopSettings);
                serial::put_str("[desktop-menu] command=properties\n");
                Redraw::Scene
            }
            ShellAction::Shutdown => {
                serial::put_str("[start] command=shutdown\n");
                shutdown()
            }
        }
    }

    /// Единая точка запуска desktop и Start. Aurora теперь обычное приложение:
    /// получает собственное окно, независимо сворачивается/закрывается и не
    /// забирает scanout у desktop.
    fn launch_gpu_demo(&mut self) {
        serial::put_str("[gpu-demo] launch requested source=desktop-shell\n");
        if self.spawn_application(ApplicationKind::GpuDemo).is_some() {
            serial::put_str("[desktop] Aurora 3D window created gpu-fallback=automatic\n");
        } else {
            serial::put_str("[desktop] Aurora 3D window create failed\n");
        }
    }

    fn tick_animated_application(&mut self, now_ms: u64) -> Option<WindowId> {
        let gpu_recording = self.framebuffer.gpu_recording();
        for offset in 0..MAX_WINDOWS {
            let index = (self.animation_cursor + offset) % MAX_WINDOWS;
            let Some(slot) = self.windows[index].as_mut() else {
                continue;
            };
            if slot.model.is_minimized() {
                continue;
            }
            let id = slot.model.id();
            if let Application::GpuDemo(demo) = slot.application.get_mut() {
                if demo.tick(now_ms, gpu_recording) {
                    self.animation_cursor = (index + 1) % MAX_WINDOWS;
                    return Some(id);
                }
            }
        }
        None
    }

    fn handle_settings_key(&mut self, id: WindowId, key: UiKey) -> Redraw {
        let result = match self.application_mut(id) {
            Some(Application::DesktopSettings(settings)) => settings.key(key, false),
            _ => return Redraw::None,
        };
        if result.changed
            && matches!(
                key,
                UiKey::PageUp | UiKey::PageDown | UiKey::Home | UiKey::End
            )
        {
            serial::put_str("[settings] resolution-list scrolled input=keyboard\n");
        }
        if result.action != DesktopSettingsAction::None {
            self.apply_desktop_settings_action(result.action);
            Redraw::Scene
        } else if result.changed {
            Redraw::Ui(id)
        } else {
            Redraw::None
        }
    }

    fn handle_settings_pointer(
        &mut self,
        id: WindowId,
        kind: UiPointerKind,
        x: i32,
        y: i32,
    ) -> Redraw {
        let result = match self.application_mut(id) {
            Some(Application::DesktopSettings(settings)) => settings.pointer(kind, x, y),
            _ => return Redraw::None,
        };
        if result.action != DesktopSettingsAction::None {
            self.apply_desktop_settings_action(result.action);
            Redraw::Scene
        } else if result.changed {
            Redraw::Ui(id)
        } else {
            Redraw::None
        }
    }

    fn apply_desktop_settings_action(&mut self, action: DesktopSettingsAction) {
        match action {
            DesktopSettingsAction::None => return,
            DesktopSettingsAction::SetResolution { width, height } => {
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
                serial::put_str("[settings] resolution=");
                serial::put_u32(width);
                serial::put_str("x");
                serial::put_u32(height);
                serial::put_str(" result=");
                serial::put_str(mode_set_result_name(result));
                serial::put_str("\n");
            }
            DesktopSettingsAction::SetColor(mode) => {
                self.framebuffer.set_color_mode(mode);
                serial::put_str("[settings] color=");
                serial::put_str(color_mode_name(mode));
                serial::put_str("\n");
            }
            DesktopSettingsAction::SetWallpaper(selected) => {
                self.wallpaper = selected;
                serial::put_str("[settings] wallpaper=");
                serial::put_str(wallpaper(selected).name);
                serial::put_str("\n");
            }
            DesktopSettingsAction::SetUiScale(scale_milli) => {
                self.ui_scale_milli = scale_milli.clamp(1_000, 1_500);
                self.shell.set_scale(self.ui_scale_milli);
                for slot in self.windows.iter_mut().flatten() {
                    match slot.application.get_mut() {
                        Application::UiShowcase(showcase) => {
                            showcase.set_scale(self.ui_scale_milli)
                        }
                        Application::FileExplorer(explorer) => {
                            explorer.set_scale(self.ui_scale_milli)
                        }
                        Application::Terminal(_)
                        | Application::DesktopSettings(_)
                        | Application::GpuDemo(_) => {}
                    }
                }
                serial::put_str("[settings] ui-scale=");
                serial::put_u32(u32::from(self.ui_scale_milli));
                serial::put_str("\n");
            }
        }
        self.sync_desktop_settings();
    }

    fn desktop_settings_snapshot(&self) -> DesktopSettingsSnapshot {
        let mode = self.framebuffer.mode();
        DesktopSettingsSnapshot {
            width: mode.width,
            height: mode.height,
            color: self.framebuffer.color_mode(),
            wallpaper: self.wallpaper,
            ui_scale_milli: self.ui_scale_milli,
        }
    }

    fn sync_desktop_settings(&mut self) {
        let snapshot = self.desktop_settings_snapshot();
        for slot in self.windows.iter_mut().flatten() {
            if let Application::DesktopSettings(settings) = slot.application.get_mut() {
                settings.sync(snapshot);
            }
        }
    }

    fn arrange_desktop_icons(&mut self) {
        self.desktop_terminal_x = 28;
        self.desktop_terminal_y = 35;
        self.desktop_trash_x = 28;
        self.desktop_trash_y = 138;
        self.desktop_icon_selected = false;
        self.desktop_gpu_demo_selected = false;
        self.desktop_icon_pressed = false;
    }

    fn desktop_background_at(&self, x: i32, y: i32) -> bool {
        y >= 0
            && y < self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32
            && self.top_window_at(x, y).is_none()
            && !self.desktop_terminal_icon().contains(x, y)
            && !self.desktop_gpu_demo_icon().contains(x, y)
            && !self.desktop_trash_icon().contains(x, y)
    }

    fn handle_terminal_key(&mut self, id: WindowId, key: Key) -> Redraw {
        let action = match self.application_mut(id) {
            Some(Application::Terminal(terminal)) => terminal.handle_key(key),
            _ => return Redraw::None,
        };
        match action {
            TerminalAction::None => Redraw::None,
            TerminalAction::RedrawInputLine => Redraw::TerminalLine,
            TerminalAction::RedrawAll => Redraw::Application(id),
            TerminalAction::DisplayInfo => {
                let mode = self.framebuffer.mode();
                let connector = self.framebuffer.connector();
                let driver = self.framebuffer.driver_name();
                let color = self.framebuffer.color_mode();
                let metrics = self.display_metrics;
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
                        metrics.logical_width(),
                        metrics.logical_height(),
                        metrics.device_scale_milli(),
                        metrics.compositor_scale_milli(),
                    );
                }
                Redraw::Scene
            }
            TerminalAction::DisplayModes => {
                let mut modes = [self.framebuffer.mode(); DISPLAY_MODE_CAPACITY];
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
                self.sync_desktop_settings();
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
                self.sync_desktop_settings();
                Redraw::Scene
            }
            TerminalAction::OpenUiShowcase => {
                let _ = self.spawn_application(ApplicationKind::UiShowcase);
                serial::put_str("[ui] Gallery opened runtime=system-ui-v1 independent-window=1\n");
                Redraw::Scene
            }
            TerminalAction::OpenFileExplorer => {
                let _ = self.spawn_application(ApplicationKind::FileExplorer);
                serial::put_str("[ui] Explorer opened runtime=system-ui-v1 independent-window=1\n");
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
                    IconThemeName::Aurora => PackId(0x2004),
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
                self.sync_desktop_settings();
                Redraw::Scene
            }
            TerminalAction::Shutdown => shutdown(),
        }
    }

    fn handle_wheel(&mut self, wheel_x: i16, wheel_y: i16) -> Redraw {
        let x = self.mouse_x;
        let y = self.mouse_y;
        let Some(id) = self.top_window_at(x, y) else {
            return Redraw::None;
        };
        if !self
            .window_content_rect(id)
            .is_some_and(|rect| rect.contains(x, y))
        {
            return Redraw::None;
        }
        let changed = match self.application_mut(id) {
            Some(Application::UiShowcase(showcase)) => {
                showcase.pointer(UiPointerKind::Scroll, x, y, wheel_y)
            }
            Some(Application::FileExplorer(explorer)) => explorer.scroll(x, y, wheel_x, wheel_y),
            Some(Application::DesktopSettings(settings)) => settings.scroll(x, y, wheel_x, wheel_y),
            Some(Application::Terminal(_)) | Some(Application::GpuDemo(_)) | None => false,
        };
        if changed {
            serial::put_str("[pointer] wheel window=0x");
            serial::put_hex(id.0);
            serial::put_str(" dx=");
            put_serial_i16(wheel_x);
            serial::put_str(" dy=");
            put_serial_i16(wheel_y);
            serial::put_str("\n");
            Redraw::Ui(id)
        } else {
            Redraw::None
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> Redraw {
        let right_pressed = event.right && !self.previous_right;
        self.previous_right = event.right;
        let _middle_pressed = event.middle;
        let maximum_pixel_x = self.framebuffer.width().saturating_sub(1);
        let maximum_pixel_y = self.framebuffer.height().saturating_sub(1);
        match event.motion {
            PointerMotion::Relative { dx, dy } => {
                self.mouse_x = (self.mouse_x + i32::from(dx)).clamp(0, maximum_pixel_x as i32);
                self.mouse_y = (self.mouse_y + i32::from(dy)).clamp(0, maximum_pixel_y as i32);
            }
            PointerMotion::Absolute {
                x,
                y,
                maximum_x,
                maximum_y,
            } => {
                // Умножение выполняется до деления, чтобы весь диапазон HID
                // точно покрывал framebuffer и не накапливал округление.
                self.mouse_x = scale_absolute_axis(x, maximum_x, maximum_pixel_x) as i32;
                self.mouse_y = scale_absolute_axis(y, maximum_y, maximum_pixel_y) as i32;
            }
        }

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

        // Wheel маршрутизируется окну под указателем, а не focused-окну. Это
        // позволяет прокручивать соседнюю галерею без лишнего click-to-focus.
        // Button transitions обрабатываются обычным путём ниже и не теряются.
        if (event.wheel_x != 0 || event.wheel_y != 0)
            && event.left == self.previous_left
            && !right_pressed
        {
            let redraw = self.handle_wheel(event.wheel_x, event.wheel_y);
            if !matches!(redraw, Redraw::None) {
                return redraw;
            }
        }

        if right_pressed {
            if let Some(id) = self.top_window_at(self.mouse_x, self.mouse_y) {
                let in_content = self
                    .window_content_rect(id)
                    .is_some_and(|rect| rect.contains(self.mouse_x, self.mouse_y));
                if in_content && self.window_kind(id) == Some(ApplicationKind::FileExplorer) {
                    self.shell.close_popups();
                    self.focus_window(id);
                    let x = self.mouse_x;
                    let y = self.mouse_y;
                    if let Some(Application::FileExplorer(explorer)) = self.application_mut(id) {
                        explorer.open_context_menu(x, y);
                    }
                    serial::put_str("[explorer] context-menu opened\n");
                    return Redraw::Scene;
                }
            }
            if self.desktop_background_at(self.mouse_x, self.mouse_y) {
                self.shell.open_desktop_menu(self.mouse_x, self.mouse_y);
                serial::put_str("[desktop-menu] opened component-runtime=system-ui-v1 x=");
                serial::put_u32(self.mouse_x as u32);
                serial::put_str(" y=");
                serial::put_u32(self.mouse_y as u32);
                serial::put_str("\n");
                return Redraw::Scene;
            }
            if self.shell.has_popup() {
                self.shell.close_popups();
                return Redraw::Scene;
            }
        }

        let was_left = self.previous_left;
        let pressed = event.left && !was_left;
        let released = !event.left && was_left;
        self.previous_left = event.left;
        if released {
            self.desktop_icon_pressed = false;
        }

        let x = self.mouse_x;
        let y = self.mouse_y;
        if pressed {
            // Один marker на mouse-down (не на каждый movement packet) делает
            // GUI-тесты и реальные bug reports воспроизводимыми.
            serial::put_str("[pointer] down x=");
            serial::put_u32(x as u32);
            serial::put_str(" y=");
            serial::put_u32(y as u32);
            serial::put_str(" top=0x");
            serial::put_hex(self.top_window_at(x, y).map_or(0, |window| window.0));
            serial::put_str("\n");
        }

        let pointer_kind = if pressed {
            UiPointerKind::Down
        } else if released {
            UiPointerKind::Up
        } else {
            UiPointerKind::Move
        };
        let programs_was_open = self.shell.programs_menu_is_open();
        let shell_result = self.shell.pointer(pointer_kind, x, y);
        if programs_was_open != self.shell.programs_menu_is_open() {
            serial::put_str(if self.shell.programs_menu_is_open() {
                "[start] programs submenu opened applications=5\n"
            } else {
                "[start] programs submenu closed\n"
            });
        }
        if shell_result.action != ShellAction::None {
            return self.handle_shell_action(shell_result.action);
        }
        if pressed && self.shell.has_popup() && !self.shell.interactive_at(x, y) {
            self.shell.close_popups();
            serial::put_str("[desktop-menu] closed by outside click\n");
            return Redraw::Scene;
        }
        if shell_result.changed || shell_result.consumed {
            return if shell_result.changed {
                Redraw::Shell
            } else {
                Redraw::None
            };
        }

        if !pressed {
            let Some(id) = self.focused else {
                return Redraw::None;
            };
            if self.window_is_visible(id)
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
                match self.window_kind(id) {
                    Some(ApplicationKind::UiShowcase) => {
                        let changed = match self.application_mut(id) {
                            Some(Application::UiShowcase(showcase)) => {
                                showcase.pointer(kind, x, y, 0)
                            }
                            _ => false,
                        };
                        if changed {
                            return Redraw::Ui(id);
                        }
                    }
                    Some(ApplicationKind::DesktopSettings) => {
                        return self.handle_settings_pointer(id, kind, x, y);
                    }
                    Some(ApplicationKind::FileExplorer) => {
                        let now_ms = arch::monotonic_milliseconds();
                        let settings = self.input.mouse_settings();
                        let changed = match self.application_mut(id) {
                            Some(Application::FileExplorer(explorer)) => {
                                explorer.pointer(kind, x, y, now_ms, settings)
                            }
                            _ => false,
                        };
                        if changed {
                            return Redraw::Ui(id);
                        }
                    }
                    Some(ApplicationKind::Terminal) | Some(ApplicationKind::GpuDemo) | None => {}
                }
            }
            return Redraw::None;
        }

        if self.desktop_terminal_icon().contains(x, y) {
            self.desktop_icon_selected = true;
            self.desktop_gpu_demo_selected = false;
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
                return Redraw::Scene;
            }
            return Redraw::DesktopIcon;
        }
        if self.desktop_gpu_demo_icon().contains(x, y) {
            self.desktop_icon_selected = false;
            self.desktop_gpu_demo_selected = true;
            let click = self.click_tracker.pressed(
                arch::monotonic_milliseconds(),
                x,
                y,
                self.input.mouse_settings(),
            );
            if click == ClickKind::Double {
                self.launch_gpu_demo();
                return Redraw::Scene;
            }
            serial::put_str("[desktop] Aurora 3D selected; double-click to launch\n");
            // Первый клик меняет только selection двух ярлыков. Полная
            // композиция desktop задерживала второй клик под TCG настолько,
            // что корректная double-click последовательность превращалась в
            // два одиночных клика и приложение визуально «не запускалось».
            return Redraw::DesktopIcon;
        }
        self.desktop_icon_selected = false;
        self.desktop_gpu_demo_selected = false;

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

        if self
            .window_content_rect(id)
            .is_some_and(|content| content.contains(x, y))
        {
            match self.window_kind(id) {
                Some(ApplicationKind::UiShowcase) => {
                    let changed = match self.application_mut(id) {
                        Some(Application::UiShowcase(showcase)) => {
                            showcase.pointer(UiPointerKind::Down, x, y, 0)
                        }
                        _ => false,
                    };
                    return if focus_changed {
                        Redraw::Scene
                    } else if changed {
                        Redraw::Ui(id)
                    } else {
                        Redraw::None
                    };
                }
                Some(ApplicationKind::DesktopSettings) => {
                    let redraw = self.handle_settings_pointer(id, UiPointerKind::Down, x, y);
                    return if focus_changed { Redraw::Scene } else { redraw };
                }
                Some(ApplicationKind::FileExplorer) => {
                    let now_ms = arch::monotonic_milliseconds();
                    let settings = self.input.mouse_settings();
                    let changed = match self.application_mut(id) {
                        Some(Application::FileExplorer(explorer)) => {
                            explorer.pointer(UiPointerKind::Down, x, y, now_ms, settings)
                        }
                        _ => false,
                    };
                    return if focus_changed {
                        Redraw::Scene
                    } else if changed {
                        Redraw::Ui(id)
                    } else {
                        Redraw::None
                    };
                }
                Some(ApplicationKind::Terminal) | Some(ApplicationKind::GpuDemo) | None => {}
            }
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
                if self.shell.interactive_at(x, y)
                    || self.task_window_at(x, y).is_some()
                    || self.desktop_terminal_icon().contains(x, y)
                    || self.desktop_gpu_demo_icon().contains(x, y)
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
        let memory = match kind {
            ApplicationKind::Terminal => {
                ApplicationMemory::new(kind, || Terminal::new(self.usable_ram_mib, self.initramfs))
            }
            ApplicationKind::FileExplorer => ApplicationMemory::new_file_explorer(
                kind,
                content,
                self.initramfs,
                self.ui_scale_milli,
            ),
            ApplicationKind::UiShowcase => ApplicationMemory::new(kind, || {
                let mut showcase = UiShowcase::new(content);
                showcase.set_scale(self.ui_scale_milli);
                showcase
            }),
            ApplicationKind::DesktopSettings => {
                let snapshot = self.desktop_settings_snapshot();
                let recommended = self.recommended_display_mode;
                let mut modes = [self.framebuffer.mode(); DISPLAY_MODE_CAPACITY];
                let count = self.framebuffer.modes(&mut modes);
                ApplicationMemory::new(kind, move || {
                    DesktopSettings::new(content, snapshot, &modes[..count], recommended)
                })
            }
            ApplicationKind::GpuDemo => {
                ApplicationMemory::new_gpu_demo(kind, arch::monotonic_milliseconds(), id.0 as u32)
            }
        }?;
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
        serial::put_str(kind.log_label());
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

    fn open_or_focus_application(&mut self, kind: ApplicationKind) -> Option<WindowId> {
        if let Some(id) = self.z_order[..self.window_count]
            .iter()
            .copied()
            .find(|id| self.window_kind(*id) == Some(kind))
        {
            if self.window_is_minimized(id) {
                let _ = self.apply_window_command(WindowCommand::restore(id));
            }
            self.focus_window(id);
            return Some(id);
        }
        self.spawn_application(kind)
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
        serial::put_str(kind.log_label());
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
        let (preferred_width, preferred_height) = match kind {
            ApplicationKind::DesktopSettings => (760, 620),
            // После chrome/header/padding остаётся ровно 800x450: готовый
            // GPU target копируется линейно, без bilinear resample каждого
            // пикселя. При ручном resize качественная фильтрация сохраняется.
            ApplicationKind::GpuDemo => (832, 564),
            _ => (1040, 650),
        };
        let width = fit_window_extent(
            self.framebuffer.width(),
            180,
            minimum_width,
            preferred_width,
        );
        let height = fit_window_extent(work_height, 114, minimum_height, preferred_height);
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

    fn application_mut(&mut self, id: WindowId) -> Option<Application<'_>> {
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
        if matches!(self.interaction, WindowInteraction::Move { .. }) {
            if let Some(rect) = self.window_rect(window) {
                let _ = self.framebuffer.cache_drag_layer(window_damage(rect));
            }
        }
    }

    fn finish_window_gesture(&mut self, window: WindowId, resized: bool) -> Redraw {
        self.interaction = WindowInteraction::None;
        let preview = self.window_rect(window).unwrap_or(Rect::new(0, 0, 0, 0));
        let visible = self.drag_preview_visible;
        self.drag_preview_visible = false;
        self.previous_left = false;
        if resized {
            if let Some(content) = self.window_content_rect(window) {
                match self.application_mut(window) {
                    Some(Application::UiShowcase(showcase)) => showcase.resize(content),
                    Some(Application::DesktopSettings(settings)) => settings.resize(content),
                    Some(Application::FileExplorer(explorer)) => explorer.resize(content),
                    Some(Application::Terminal(_)) | Some(Application::GpuDemo(_)) | None => {}
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
        // До включения отдельного HiDPI raster surface любой mode-set остаётся
        // честным 1:1: layout и framebuffer одного размера, bitmap не тянется.
        self.display_metrics = WindowMetrics::one_to_one(screen_width, screen_height);
        self.log_display_metrics();
        self.cursor.invalidate();
        self.mouse_x = self.mouse_x.clamp(0, screen_width.saturating_sub(1) as i32);
        self.mouse_y = self
            .mouse_y
            .clamp(0, screen_height.saturating_sub(1) as i32);
        self.interaction = WindowInteraction::None;
        self.drag_preview_visible = false;
        self.shell.resize(screen_width, screen_height);
        let work_area = self.window_work_area();
        for index in 0..MAX_WINDOWS {
            let event = if let Some(slot) = self.windows[index].as_mut() {
                let event = slot.model.reflow(work_area);
                let content = content_rect_for(&slot.model);
                match slot.application.get_mut() {
                    Application::UiShowcase(showcase) => showcase.resize(content),
                    Application::DesktopSettings(settings) => settings.resize(content),
                    Application::FileExplorer(explorer) => explorer.resize(content),
                    Application::Terminal(_) | Application::GpuDemo(_) => {}
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

    /// Исполняет display list только одного приложения с уже накопленным в
    /// его runtime damage. Window chrome, wallpaper и остальные окна при
    /// hover не меняются и поэтому здесь принципиально не рисуются.
    fn render_application_incremental(
        &mut self,
        id: WindowId,
    ) -> DamageRegion<INCREMENTAL_DAMAGE_CAPACITY> {
        let bounds = Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
        let mut damage = DamageRegion::new(bounds);
        let Some(content) = self.window_content_rect(id) else {
            return damage;
        };
        let Some(index) = self.window_slot_index(id) else {
            return damage;
        };
        let icon_pack = active_icon_or_default(self.icon_packs.active());
        let (framebuffer, windows) = (&mut self.framebuffer, &mut self.windows);
        let Some(slot) = windows[index].as_mut() else {
            return damage;
        };
        let window_rect = video_rect(slot.model.rect());
        let resizable =
            slot.model.style().contains(WindowStyle::RESIZABLE) && !slot.model.is_maximized();
        match slot.application.get_mut() {
            Application::Terminal(terminal) => {
                terminal.draw(framebuffer, content);
                damage.add(content);
            }
            Application::FileExplorer(explorer) => {
                explorer.resize(content);
                let frame = explorer.draw(framebuffer, icon_pack, false);
                for rect in frame.iter().copied() {
                    damage.add(rect);
                }
            }
            Application::UiShowcase(showcase) => {
                showcase.resize(content);
                append_frame_damage(&mut damage, showcase.draw(framebuffer, false));
            }
            Application::DesktopSettings(settings) => {
                settings.resize(content);
                append_frame_damage(&mut damage, settings.draw(framebuffer, false));
            }
            Application::GpuDemo(demo) => {
                demo.draw(framebuffer, content);
                damage.add(content);
            }
        }
        if resizable {
            draw_resize_grip(framebuffer, window_rect, true);
            damage.add(resize_grip_rect(window_rect));
        }
        damage
    }

    /// Shell состоит из независимых runtime'ов. Их damage объединяется, но
    /// неизменившийся фон taskbar/desktop повторно не растеризуется.
    fn render_shell_incremental(&mut self) -> DamageRegion<INCREMENTAL_DAMAGE_CAPACITY> {
        let bounds = Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
        let mut damage = DamageRegion::new(bounds);
        let icon_pack = active_icon_or_default(self.icon_packs.active());
        append_frame_damage(
            &mut damage,
            self.shell
                .draw_launcher(&mut self.framebuffer, icon_pack, false),
        );
        if self.shell.is_open() {
            append_frame_damage(
                &mut damage,
                self.shell
                    .draw_menu(&mut self.framebuffer, icon_pack, false),
            );
            if self.shell.programs_menu_is_open() {
                append_frame_damage(
                    &mut damage,
                    self.shell
                        .draw_programs_menu(&mut self.framebuffer, icon_pack, false),
                );
            }
        }
        if self.shell.desktop_menu_is_open() {
            append_frame_damage(
                &mut damage,
                self.shell
                    .draw_desktop_menu(&mut self.framebuffer, icon_pack, false),
            );
        }
        damage
    }

    /// Изменение selection desktop icon не должно повторно растеризовать
    /// обои, taskbar и все окна: на медленном CPU backend это ломало даже
    /// double click. После обновления base layer восстанавливаем только окна,
    /// которые действительно перекрывают ярлык.
    fn render_desktop_icon_incremental(&mut self) -> DamageRegion<INCREMENTAL_DAMAGE_CAPACITY> {
        let bounds = Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
        let mut damage = DamageRegion::new(bounds);
        let icon = self.desktop_terminal_icon();
        let desktop = Rect::new(
            0,
            0,
            self.framebuffer.width(),
            self.framebuffer.height().saturating_sub(TASKBAR_HEIGHT),
        );
        self.framebuffer
            .draw_wallpaper_clipped(desktop, icon, wallpaper(self.wallpaper));
        self.render_terminal_desktop_icon();
        let _ = self.framebuffer.cache_background_rect(icon);

        for index in 0..self.window_count {
            let id = self.z_order[index];
            if self.window_is_visible(id)
                && self
                    .window_rect(id)
                    .is_some_and(|window| !window.intersection(icon).is_empty())
            {
                self.render_window(id);
                if let Some(window) = self.window_rect(id) {
                    damage.add(window_damage(window));
                }
            }
        }
        damage.add(icon);
        damage
    }

    fn render_all(&mut self) {
        if self.framebuffer.gpu_recording() {
            if self.render_gpu_frame() {
                return;
            }
            self.activate_cpu_recovery("initial-frame-failure");
            return;
        }
        self.render_scene();
        self.cursor
            .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
        self.framebuffer.present();
    }

    fn render_gpu_frame(&mut self) -> bool {
        crate::gui::gpu_scene::begin(self.framebuffer.width(), self.framebuffer.height());
        self.render_scene();
        // Обычно draw завершится обновлением аппаратного cursor plane и слой
        // останется пустым. Небольшая отдельная surface сохраняет корректный
        // software fallback, не привязывая курсор к последнему окну.
        crate::gui::gpu_scene::begin_layer(
            3,
            Rect::new(self.mouse_x - 64, self.mouse_y - 64, 128, 128),
            0,
        );
        self.cursor
            .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
        let Some((header, layers, quads)) = crate::gui::gpu_scene::finish() else {
            serial::put_str("[system-ui-gpu] frame rejected reason=command-capacity\n");
            return false;
        };
        self.present_gpu_stream(header, layers, quads)
    }

    fn render_gpu_transform(&mut self, window: WindowId) -> bool {
        let Some(rect) = self.window_rect(window) else {
            return false;
        };
        let Some((header, layers, quads)) =
            crate::gui::gpu_scene::transform_layer(0x1000_0000_0000_0000 | window.0, rect)
        else {
            return false;
        };
        self.present_gpu_stream(header, layers, quads)
    }

    fn present_gpu_stream(
        &self,
        header: rustos_abi::gpu::GpuUiFrameHeader,
        layers: &[rustos_abi::gpu::GpuUiLayer],
        quads: &[rustos_abi::gpu::GpuUiQuad],
    ) -> bool {
        match process::present_system_ui_gpu(header, layers, quads) {
            Ok(()) => true,
            Err(error) => {
                serial::put_str("[system-ui-gpu] submit failed reason=");
                serial::put_str(error.label());
                serial::put_str("\n");
                false
            }
        }
    }

    fn activate_cpu_recovery(&mut self, reason: &str) {
        self.framebuffer.set_gpu_recording(false);
        serial::put_str("[system-ui] fallback=cpu reason=");
        serial::put_str(reason);
        serial::put_str("\n");
        self.render_scene();
        self.cursor
            .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
        self.framebuffer.present();
    }

    fn render_scene(&mut self) {
        if self.framebuffer.gpu_recording() {
            let screen = Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
            // Popup содержит прозрачные области, а VirGL BLIT не является
            // alpha compositor. При открытом меню строим один непрозрачный
            // snapshot всей сцены: это редкое событие и оно гарантирует
            // правильный z-order без чёрных прямоугольников и stale damage.
            if self.shell.desktop_menu_is_open() || self.shell.is_open() {
                crate::gui::gpu_scene::begin_layer(
                    4,
                    screen,
                    rustos_abi::gpu::ui_layer_flag::OPAQUE,
                );
                self.render_base();
                for index in 0..self.window_count {
                    let id = self.z_order[index];
                    if self.window_is_visible(id) {
                        self.render_window(id);
                    }
                }
                if self.shell.desktop_menu_is_open() {
                    self.render_desktop_menu();
                }
                if self.shell.is_open() {
                    self.render_start_menu();
                }
                return;
            }
            crate::gui::gpu_scene::begin_layer(1, screen, rustos_abi::gpu::ui_layer_flag::OPAQUE);
            self.render_base();
            for index in 0..self.window_count {
                let id = self.z_order[index];
                if self.window_is_visible(id) {
                    if let Some(rect) = self.window_rect(id) {
                        // Оконная surface содержит premultiplied alpha:
                        // прозрачные corner pixels сохраняют desktop под
                        // системным скруглением без CPU readback.
                        crate::gui::gpu_scene::begin_layer(0x1000_0000_0000_0000 | id.0, rect, 0);
                        self.render_window(id);
                    }
                }
            }
            return;
        }
        self.render_base();
        let _ = self.framebuffer.cache_background();
        for index in 0..self.window_count {
            let id = self.z_order[index];
            if self.window_is_visible(id) {
                self.render_window(id);
            }
        }
        if self.shell.desktop_menu_is_open() {
            self.render_desktop_menu();
        }
        if self.shell.is_open() {
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
        let _ = first;
        self.restore_preview(previous);
        if let Some(rect) = self.window_rect(window) {
            let damage = window_damage(rect);
            let copied = matches!(self.interaction, WindowInteraction::Move { .. })
                && self.framebuffer.draw_cached_drag_layer(damage);
            if !copied {
                self.render_window(window);
            }
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

    fn restore_preview(&mut self, rect: Rect) {
        let _ = self.framebuffer.restore_background(window_damage(rect));
    }

    fn empty_frame_damage(&self) -> DamageRegion<INCREMENTAL_DAMAGE_CAPACITY> {
        let bounds = Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
        DamageRegion::new(bounds)
    }

    fn record_drag_rect<const CAPACITY: usize>(
        &mut self,
        region: &mut DamageRegion<CAPACITY>,
        rect: Rect,
    ) {
        region.add(rect);
        self.drag_present_pixels = self.drag_present_pixels.saturating_add(clipped_area(
            rect,
            self.framebuffer.width(),
            self.framebuffer.height(),
        ));
    }

    fn record_drag_full<const CAPACITY: usize>(&mut self, region: &mut DamageRegion<CAPACITY>) {
        region.add(Rect::new(
            0,
            0,
            self.framebuffer.width(),
            self.framebuffer.height(),
        ));
        self.drag_present_pixels = self.drag_present_pixels.saturating_add(
            u64::from(self.framebuffer.width()) * u64::from(self.framebuffer.height()),
        );
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
        if let Some(rect) = self.window_rect(window) {
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
            font::UI_SMALL.italic().scaled(self.ui_scale_milli),
        );
        font::draw_text(
            &mut self.framebuffer,
            branding_x,
            branding_y,
            arch::ARCH_NAME,
            Color::rgb(221, 238, 244),
            font::UI_SMALL.italic().scaled(self.ui_scale_milli),
        );
    }

    fn render_desktop_icons(&mut self) {
        self.render_terminal_desktop_icon();
        let demo = self.desktop_gpu_demo_icon();
        if self.desktop_gpu_demo_selected {
            self.framebuffer
                .fill_rounded_rect(demo, 10, Theme::ACCENT_SOFT);
            self.framebuffer.rounded_border(demo, 10, 1, Theme::ACCENT);
        }
        self.draw_system_icon(
            IconKind::GpuDemo,
            Rect::new(demo.x + 12, demo.y + 3, 48, 48),
        );
        font::draw_text(
            &mut self.framebuffer,
            demo.x + 2,
            demo.y + 61,
            "Aurora 3D",
            Theme::TEXT,
            font::UI_SMALL.bold().scaled(self.ui_scale_milli),
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
            "Корзина",
            Theme::TEXT,
            font::UI_SMALL.bold().scaled(self.ui_scale_milli),
        );
    }

    fn render_terminal_desktop_icon(&mut self) {
        let terminal = self.desktop_terminal_icon();
        if self.desktop_icon_selected {
            self.framebuffer
                .fill_rounded_rect(terminal, 10, Theme::ACCENT_SOFT);
            self.framebuffer
                .rounded_border(terminal, 10, 1, Theme::ACCENT);
        }
        self.draw_system_icon(
            IconKind::Terminal,
            Rect::new(terminal.x + 12, terminal.y + 3, 48, 48),
        );
        font::draw_text(
            &mut self.framebuffer,
            terminal.x + 5,
            terminal.y + 61,
            "Терминал",
            Theme::TEXT,
            font::UI_SMALL.bold().scaled(self.ui_scale_milli),
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

        let screen = Rect::new(0, 0, self.framebuffer.width(), self.framebuffer.height());
        if !self.framebuffer.gpu_recording() {
            self.framebuffer.soft_shadow(rect, Theme::RADIUS, screen);
        }
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
            let title_color = if focused {
                Color::rgb(24, 38, 59)
            } else {
                Color::rgb(25, 33, 47)
            };
            let title = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, TITLE_HEIGHT - 1);
            self.framebuffer
                .fill_rounded_rect(title, Theme::RADIUS.saturating_sub(1), title_color);
            // У title bar скруглены только верхние углы: нижняя полоса
            // перекрывает нижние corner cut-outs тем же токеном поверхности.
            self.framebuffer.fill_rect(
                Rect::new(
                    title.x,
                    title.y + Theme::RADIUS as i32,
                    title.width,
                    title.height.saturating_sub(u32::from(Theme::RADIUS)),
                ),
                title_color,
            );
            self.framebuffer.fill_rect(
                Rect::new(title.x, title.bottom().saturating_sub(1), title.width, 1),
                Theme::BORDER,
            );
            match kind {
                ApplicationKind::Terminal => self.draw_system_icon(
                    IconKind::Terminal,
                    Rect::new(rect.x + 8, rect.y + 6, 22, 22),
                ),
                ApplicationKind::FileExplorer => self
                    .draw_system_icon(IconKind::Folder, Rect::new(rect.x + 8, rect.y + 6, 22, 22)),
                ApplicationKind::UiShowcase => {
                    self.draw_system_icon(IconKind::Grid, Rect::new(rect.x + 8, rect.y + 6, 22, 22))
                }
                ApplicationKind::DesktopSettings => self.draw_system_icon(
                    IconKind::Settings,
                    Rect::new(rect.x + 8, rect.y + 6, 22, 22),
                ),
                ApplicationKind::GpuDemo => self
                    .draw_system_icon(IconKind::GpuDemo, Rect::new(rect.x + 8, rect.y + 6, 22, 22)),
            }
            Label {
                rect: Rect::new(rect.x + 38, rect.y + 8, 310, 22),
                text: kind.title(),
                color: if focused {
                    Theme::TEXT
                } else {
                    Theme::TEXT_MUTED
                },
                style: font::UI_TITLE.scaled(self.ui_scale_milli),
            }
            .draw(&mut self.framebuffer);
            let (minimize, maximize, close) = self.window_controls(id);
            if let Some(control) = minimize {
                Button {
                    rect: control,
                    label: "",
                    hovered: focused && control.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: false,
                }
                .draw(&mut self.framebuffer);
                MIDNIGHT_ICON_PACK.draw(
                    &mut self.framebuffer,
                    IconKind::Minimize,
                    inset_rect(control, 5),
                );
            }
            if let Some(control) = maximize {
                Button {
                    rect: control,
                    label: "",
                    hovered: focused && control.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: false,
                }
                .draw(&mut self.framebuffer);
                MIDNIGHT_ICON_PACK.draw(
                    &mut self.framebuffer,
                    if maximized {
                        IconKind::Restore
                    } else {
                        IconKind::Maximize
                    },
                    inset_rect(control, 5),
                );
            }
            if let Some(control) = close {
                Button {
                    rect: control,
                    label: "",
                    hovered: focused && control.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: true,
                }
                .draw(&mut self.framebuffer);
                MIDNIGHT_ICON_PACK.draw(
                    &mut self.framebuffer,
                    IconKind::Close,
                    inset_rect(control, 5),
                );
            }
        }
        let content = content_rect_from(rect, style);
        let Some(index) = self.window_slot_index(id) else {
            return;
        };
        let icon_pack = active_icon_or_default(self.icon_packs.active());
        let (framebuffer, windows) = (&mut self.framebuffer, &mut self.windows);
        let Some(slot) = windows[index].as_mut() else {
            return;
        };
        match slot.application.get_mut() {
            Application::Terminal(terminal) => terminal.draw(framebuffer, content),
            Application::FileExplorer(explorer) => {
                explorer.resize(content);
                let _ = explorer.draw(framebuffer, icon_pack, true);
            }
            Application::UiShowcase(showcase) => {
                showcase.resize(content);
                let _ = showcase.draw(framebuffer, true);
            }
            Application::DesktopSettings(settings) => {
                settings.resize(content);
                let _ = settings.draw(framebuffer, true);
            }
            Application::GpuDemo(demo) => demo.draw(framebuffer, content),
        }
        if style.contains(WindowStyle::BORDER)
            && style.contains(WindowStyle::RESIZABLE)
            && !maximized
        {
            draw_resize_grip(framebuffer, rect, focused);
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
            Application::FileExplorer(_)
            | Application::UiShowcase(_)
            | Application::DesktopSettings(_)
            | Application::GpuDemo(_) => None,
        }
    }

    fn render_taskbar(&mut self) {
        let y = self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32;
        self.framebuffer.horizontal_gradient(
            Rect::new(0, y, self.framebuffer.width(), TASKBAR_HEIGHT),
            Color::rgb(13, 19, 30),
            Color::rgb(18, 26, 40),
        );
        self.framebuffer
            .fill_rect(Rect::new(0, y, self.framebuffer.width(), 1), Theme::BORDER);
        let icon_pack = active_icon_or_default(self.icon_packs.active());
        let _ = self
            .shell
            .draw_launcher(&mut self.framebuffer, icon_pack, true);

        for index in 0..self.window_count {
            let id = self.task_order[index];
            let Some((kind, minimized)) = self
                .window_slot(id)
                .map(|slot| (slot.kind(), slot.model.is_minimized()))
            else {
                continue;
            };
            let task = self.task_button(index, self.window_count);
            self.framebuffer.fill_rounded_rect(
                task,
                9,
                if self.focused == Some(id) && !minimized {
                    Theme::ACCENT_SOFT
                } else {
                    Theme::PANEL
                },
            );
            match kind {
                ApplicationKind::Terminal => self.draw_system_icon(
                    IconKind::Terminal,
                    Rect::new(task.x + 6, task.y + 6, 26, 26),
                ),
                ApplicationKind::FileExplorer => self
                    .draw_system_icon(IconKind::Folder, Rect::new(task.x + 6, task.y + 6, 26, 26)),
                ApplicationKind::UiShowcase => {
                    self.draw_system_icon(IconKind::Grid, Rect::new(task.x + 6, task.y + 6, 26, 26))
                }
                ApplicationKind::DesktopSettings => self.draw_system_icon(
                    IconKind::Settings,
                    Rect::new(task.x + 6, task.y + 6, 26, 26),
                ),
                ApplicationKind::GpuDemo => self
                    .draw_system_icon(IconKind::GpuDemo, Rect::new(task.x + 6, task.y + 6, 26, 26)),
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
                    font::UI_SMALL.scaled(self.ui_scale_milli),
                );
            }
        }
        let _ = self
            .shell
            .draw_clock(&mut self.framebuffer, icon_pack, true);
    }

    fn render_start_menu(&mut self) {
        let icon_pack = active_icon_or_default(self.icon_packs.active());
        let _ = self.shell.draw_menu(&mut self.framebuffer, icon_pack, true);
        if self.shell.programs_menu_is_open() {
            let _ = self
                .shell
                .draw_programs_menu(&mut self.framebuffer, icon_pack, true);
        }
    }

    fn render_desktop_menu(&mut self) {
        let icon_pack = active_icon_or_default(self.icon_packs.active());
        let _ = self
            .shell
            .draw_desktop_menu(&mut self.framebuffer, icon_pack, true);
    }

    fn desktop_terminal_icon(&self) -> Rect {
        Rect::new(self.desktop_terminal_x, self.desktop_terminal_y, 74, 86)
    }

    fn desktop_gpu_demo_icon(&self) -> Rect {
        Rect::new(
            self.desktop_terminal_x + 84,
            self.desktop_terminal_y,
            74,
            86,
        )
    }

    fn desktop_trash_icon(&self) -> Rect {
        Rect::new(self.desktop_trash_x, self.desktop_trash_y, 74, 82)
    }

    fn task_button(&self, index: usize, count: usize) -> Rect {
        let available = self.framebuffer.width().saturating_sub(126 + 160);
        let width = if count == 0 {
            180
        } else {
            (available / count as u32).clamp(22, 180)
        };
        Rect::new(
            126 + index as i32 * width as i32,
            self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 + 4,
            width.saturating_sub(3),
            TASKBAR_HEIGHT - 8,
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
    /// Перерисовать только изменившиеся controls одного client viewport.
    Application(WindowId),
    /// Тот же локальный путь, но причина — state/damage component runtime.
    /// Разделение нужно regression telemetry: terminal full-content redraw не
    /// должен подменять собой измерение hover budget.
    Ui(WindowId),
    /// Перерисовать только dirty controls taskbar/Start/context menu.
    Shell,
    /// Локально обновить selection ярлыка на desktop base layer.
    DesktopIcon,
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

fn append_frame_damage<const D: usize>(
    damage: &mut DamageRegion<INCREMENTAL_DAMAGE_CAPACITY>,
    frame: FrameResult<D>,
) {
    for rect in frame.damage().iter().copied() {
        damage.add(rect);
    }
}

fn resize_grip_rect(window: Rect) -> Rect {
    Rect::new(
        window.right().saturating_sub(21),
        window.bottom().saturating_sub(21),
        18,
        18,
    )
}

/// Resize grip рисуется оконным сервером последним: приложение не может
/// случайно стереть системную drag-цель своим footer. Несколько диагоналей
/// сохраняют узнаваемость и на светлой, и на тёмной поверхности.
fn draw_resize_grip(framebuffer: &mut Framebuffer, window: Rect, focused: bool) {
    let grip = resize_grip_rect(window);
    let color = if focused {
        Theme::ACCENT
    } else {
        Theme::TEXT_MUTED
    };
    for length in [4i32, 9, 14] {
        for step in 0..length {
            let x = grip.right().saturating_sub(length).saturating_add(step);
            let y = grip.bottom().saturating_sub(2 + step);
            if grip.contains(x, y) {
                framebuffer.blend_pixel(x, y, color, 210);
            }
        }
    }
}

fn log_incremental_damage(scope: &str, damage: &DamageRegion<INCREMENTAL_DAMAGE_CAPACITY>) {
    serial::put_str("[compositor] repaint=incremental scope=");
    serial::put_str(scope);
    serial::put_str(" rects=");
    serial::put_u32(damage.len() as u32);
    serial::put_str(" present-kpx=");
    serial::put_u32((damage.covered_pixels() / 1_000) as u32);
    serial::put_str(" full-screen=no\n");
}

fn put_serial_i16(value: i16) {
    if value < 0 {
        serial::put_str("-");
    }
    serial::put_u32(u32::from(value.unsigned_abs()));
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
        rect.x.saturating_sub(WINDOW_SHADOW_RIGHT as i32),
        rect.y.saturating_sub(WINDOW_SHADOW_RIGHT as i32),
        rect.width
            .saturating_add(WINDOW_SHADOW_RIGHT.saturating_mul(2)),
        rect.height
            .saturating_add(WINDOW_SHADOW_RIGHT)
            .saturating_add(WINDOW_SHADOW_BOTTOM),
    )
}

const fn inset_rect(rect: Rect, amount: u32) -> Rect {
    Rect::new(
        rect.x.saturating_add(amount as i32),
        rect.y.saturating_add(amount as i32),
        rect.width.saturating_sub(amount.saturating_mul(2)),
        rect.height.saturating_sub(amount.saturating_mul(2)),
    )
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

fn scale_absolute_axis(value: u16, logical_maximum: u16, pixel_maximum: u32) -> u32 {
    if logical_maximum == 0 || pixel_maximum == 0 {
        return 0;
    }
    (u64::from(value.min(logical_maximum)) * u64::from(pixel_maximum) / u64::from(logical_maximum))
        as u32
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

fn color_mode_name(mode: ColorMode) -> &'static str {
    match mode {
        ColorMode::TrueColor24 => "truecolor24",
        ColorMode::HighColor16 => "rgb565",
        ColorMode::Grayscale8 => "gray8",
    }
}

fn mode_set_result_name(result: Result<DisplayMode, ModeSetError>) -> &'static str {
    match result {
        Ok(_) => "active",
        Err(ModeSetError::RequiresReboot) => "reboot-required",
        Err(ModeSetError::UnsupportedMode) => "unsupported",
        Err(ModeSetError::OutOfMemory) => "out-of-memory",
        Err(ModeSetError::DeviceLost) => "device-lost",
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

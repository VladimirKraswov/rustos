//! Desktop session, software compositor и первый оконный менеджер RustOS.
//!
//! В текущем вертикальном срезе session работает на CPU0. Границы между
//! input, terminal, widgets и compositor уже разделены; при включении ring 3
//! эти вызовы станут IPC-сообщениями без переписывания визуальных компонентов.

use crate::{
    apps::terminal::{Terminal, TerminalAction},
    arch, font,
    graphics::{Color, Framebuffer, Rect},
    gui::components::{self, Button, Label, Panel, Theme, Widget},
    input::{self, Event, Key, MouseEvent, PlatformInput},
    serial,
};
use rustos_abi::{
    bootinfo::BootInitramfs,
    window::{
        event as window_event, WindowCommand, WindowEvent, WindowId, WindowRect, WindowStyle,
    },
    BootInfo,
};
use rustos_video::{
    hit_test_resize, resize_from_edges, DamageRegion, DisplayDriver, DisplayMode, ManagedWindow,
    PixelFormat, ResizeEdges, Scanout, WindowEventQueue,
};

/// Высота taskbar'а: desktop-иконки и maximized-окна не заходят на неё.
const TASKBAR_HEIGHT: u32 = 46;
/// Высота заголовка окна (зона перетаскивания + кнопки -/+ /X).
const TITLE_HEIGHT: u32 = 34;
/// Ширина hit area вокруг рамки для resize мышью.
const RESIZE_BORDER: u32 = 6;
const TERMINAL_WINDOW_ID: WindowId = WindowId::new(1);
const TERMINAL_MIN_WIDTH: u32 = 480;
const TERMINAL_MIN_HEIGHT: u32 = 300;
/// Размер области курсора (рисуется стрелкой 14×20).
const CURSOR_WIDTH: usize = 14;
const CURSOR_HEIGHT: usize = 20;

/// Точка входа GUI-сессии: создаёт compositor'а, рисует desktop и
/// уходит в бесконечный event loop. Возвращается только через
/// [`shutdown`] (ACPI power off) — отсюда `!`.
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
    let mut session = DesktopSession::new(
        framebuffer,
        info.total_usable_ram() / (1024 * 1024),
        info.initramfs,
    );
    session.render_all();
    serial::put_str("[gui] GUI_READY desktop=1 terminal=1 mouse=");
    serial::put_str(input::backend_name());
    serial::put_str("\n");
    session.event_loop()
}

/// Текущая pointer-операция. Persistent state живёт в `ManagedWindow`;
/// здесь хранятся только данны одного mouse gesture.
#[derive(Clone, Copy)]
enum WindowInteraction {
    None,
    Move {
        offset_x: i32,
        offset_y: i32,
    },
    Resize {
        edges: ResizeEdges,
        start_mouse_x: i32,
        start_mouse_y: i32,
        start: WindowRect,
    },
}

/// Desktop-сессия: владеет framebuffer'ом, input'ом, окном терминала и
/// курсором; отвечает и за event loop, и за отрисовку (см. модуль).
struct DesktopSession {
    framebuffer: Framebuffer,
    input: PlatformInput,
    terminal: Terminal,
    window: ManagedWindow,
    window_events: WindowEventQueue<32>,
    interaction: WindowInteraction,
    cursor: Cursor,
    mouse_x: i32,
    mouse_y: i32,
    previous_left: bool,
    start_open: bool,
    drag_frames: u32,
    drag_packets: u32,
    drag_present_pixels: u64,
    drag_preview_visible: bool,
}

impl DesktopSession {
    fn new(framebuffer: Framebuffer, usable_ram_mib: u64, initramfs: BootInitramfs) -> Self {
        let screen_width = framebuffer.width();
        let screen_height = framebuffer.height();
        let width = screen_width.saturating_sub(180).clamp(620, 1040);
        let height = screen_height.saturating_sub(160).clamp(420, 650);
        // На широком экране оставляем слева удобную область под desktop
        // icons, а терминал стартует с постоянным 120px полем. На маленьком
        // режиме окно по-прежнему центрируется и не выходит за границы.
        let x = if screen_width >= width.saturating_add(240) {
            120
        } else {
            ((screen_width - width) / 2) as i32
        };
        let y = ((screen_height.saturating_sub(TASKBAR_HEIGHT) - height) / 2) as i32;
        let rect = Rect::new(x, y, width, height);
        Self {
            mouse_x: (screen_width / 2) as i32,
            mouse_y: (screen_height / 2) as i32,
            framebuffer,
            input: PlatformInput::new(),
            terminal: Terminal::new(usable_ram_mib, initramfs),
            window: ManagedWindow::new(
                TERMINAL_WINDOW_ID,
                window_rect(rect),
                WindowStyle::STANDARD,
                TERMINAL_MIN_WIDTH,
                TERMINAL_MIN_HEIGHT,
            ),
            window_events: WindowEventQueue::new(),
            interaction: WindowInteraction::None,
            cursor: Cursor::new(),
            previous_left: false,
            start_open: false,
            drag_frames: 0,
            drag_packets: 0,
            drag_present_pixels: 0,
            drag_preview_visible: false,
        }
    }

    /// Основной цикл: poll input → обработчик → минимум перерисовки
    /// (Redraw) → present. Без прерываний, поэтому между событиями —
    /// `spin_loop` (см. модуль: в ring-3 срезе это станет yield).
    fn event_loop(&mut self) -> ! {
        loop {
            if let Some(event) = self.input.poll() {
                let old_cursor = self.cursor.rect();
                self.cursor.restore(&mut self.framebuffer);
                let redraw = match event {
                    Event::Key(key) => self.handle_key(key),
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                };
                let mut terminal_line = None;
                let mut drag_cached = false;
                match redraw {
                    Redraw::All => self.render_scene(),
                    Redraw::Window => {
                        self.render_window_area();
                        self.render_taskbar();
                        if self.start_open {
                            self.render_start_menu();
                        }
                    }
                    Redraw::TerminalLine => {
                        let content = self.window_content_rect();
                        terminal_line = self
                            .terminal
                            .draw_input_line(&mut self.framebuffer, content);
                    }
                    Redraw::DragMove { previous, first } => {
                        drag_cached = self.render_drag_preview(previous, first);
                    }
                    Redraw::DragEnd {
                        preview, visible, ..
                    } => {
                        drag_cached = self.render_drag_end(preview, visible);
                    }
                    Redraw::None => {}
                }
                self.cursor
                    .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
                match redraw {
                    Redraw::None => {
                        // Обычное движение мыши меняет только две маленькие
                        // области — прежний и новый курсор.
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
                    Redraw::DragMove { previous, first } => {
                        if drag_cached {
                            if first {
                                self.present_drag_rect(window_damage(previous));
                            } else {
                                self.present_preview(previous);
                            }
                            self.present_preview(self.window_rect());
                        } else {
                            self.present_drag_full();
                        }
                        self.framebuffer.present_rect(old_cursor);
                        self.framebuffer.present_rect(self.cursor.rect());
                    }
                    Redraw::DragEnd {
                        visible, resized, ..
                    } => {
                        if visible {
                            if drag_cached {
                                self.present_drag_rect(window_damage(self.window_rect()));
                            } else {
                                self.present_drag_full();
                            }
                        } else {
                            self.framebuffer.present_rect(old_cursor);
                            self.framebuffer.present_rect(self.cursor.rect());
                        }
                        self.log_drag_finished(resized);
                    }
                    Redraw::Window => self.framebuffer.present(),
                    Redraw::All => {
                        self.framebuffer.present();
                        if self.window.is_minimized() {
                            serial::put_str("[wm] frame committed minimized=1\n");
                        }
                    }
                }
                self.dispatch_window_events();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    /// Клавиша: Escape закрывает start menu, остальное — в терминал
    /// (Shutdown из терминала гасит систему).
    fn handle_key(&mut self, key: Key) -> Redraw {
        if matches!(key, Key::Escape) && self.start_open {
            self.start_open = false;
            return Redraw::All;
        }
        if self.window.is_closed() || self.window.is_minimized() {
            return Redraw::None;
        }
        match self.terminal.handle_key(key) {
            TerminalAction::None => Redraw::None,
            TerminalAction::RedrawInputLine => Redraw::TerminalLine,
            TerminalAction::RedrawAll => Redraw::Window,
            TerminalAction::DisplayInfo => {
                let mode = self.framebuffer.mode();
                let connector = self.framebuffer.connector();
                serial::put_str("[display] info driver=");
                serial::put_str(self.framebuffer.driver_name());
                serial::put_str(" mode=");
                serial::put_u32(mode.width);
                serial::put_str("x");
                serial::put_u32(mode.height);
                serial::put_str("\n");
                self.terminal.report_display_info(
                    self.framebuffer.driver_name(),
                    mode.width,
                    mode.height,
                    connector.width_mm,
                    connector.height_mm,
                    self.framebuffer.color_mode(),
                );
                Redraw::Window
            }
            TerminalAction::DisplayModes => {
                let mut modes = [self.framebuffer.mode(); 20];
                let count = self.framebuffer.modes(&mut modes);
                serial::put_str("[display] modes count=");
                serial::put_u32(count as u32);
                serial::put_str("\n");
                self.terminal.report_display_modes(&modes[..count]);
                Redraw::Window
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
                self.terminal.report_display_mode(width, height, result);
                if result.is_ok() {
                    Redraw::All
                } else {
                    Redraw::Window
                }
            }
            TerminalAction::DisplayColor(mode) => {
                self.framebuffer.set_color_mode(mode);
                serial::put_str("[display] color=");
                serial::put_str(match mode {
                    rustos_video::ColorMode::TrueColor24 => "truecolor24",
                    rustos_video::ColorMode::HighColor16 => "rgb565",
                    rustos_video::ColorMode::Grayscale8 => "gray8",
                });
                serial::put_str("\n");
                self.terminal.report_color_mode(mode);
                Redraw::All
            }
            TerminalAction::Shutdown => shutdown(),
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> Redraw {
        // Вторичная и средняя кнопки уже нормализованы драйвером; текущий
        // desktop не назначает им действий, но состояние читается здесь,
        // чтобы дальнейшее context menu не меняло input ABI.
        let _secondary_pressed = event.right || event.middle;
        self.mouse_x = (self.mouse_x + event.dx as i32)
            .clamp(0, self.framebuffer.width().saturating_sub(1) as i32);
        self.mouse_y = (self.mouse_y + event.dy as i32)
            .clamp(0, self.framebuffer.height().saturating_sub(1) as i32);

        if let WindowInteraction::Move { offset_x, offset_y } = self.interaction {
            if !event.left {
                return self.finish_window_gesture(false);
            }
            let old_window = self.window_rect();
            let first = !self.drag_preview_visible;
            let command = WindowCommand::move_to(
                TERMINAL_WINDOW_ID,
                self.mouse_x - offset_x,
                self.mouse_y - offset_y,
            );
            let _ = self.apply_window_command(command);
            self.previous_left = event.left;
            self.drag_frames = self.drag_frames.saturating_add(1);
            self.drag_packets = self.drag_packets.saturating_add(u32::from(event.packets));
            return Redraw::DragMove {
                previous: old_window,
                first,
            };
        }
        if let WindowInteraction::Resize {
            edges,
            start_mouse_x,
            start_mouse_y,
            start,
        } = self.interaction
        {
            if !event.left {
                return self.finish_window_gesture(true);
            }
            let old_window = self.window_rect();
            let first = !self.drag_preview_visible;
            let requested = resize_from_edges(
                start,
                edges,
                self.mouse_x - start_mouse_x,
                self.mouse_y - start_mouse_y,
                TERMINAL_MIN_WIDTH,
                TERMINAL_MIN_HEIGHT,
            );
            let _ = self.apply_window_command(WindowCommand::resize(TERMINAL_WINDOW_ID, requested));
            self.previous_left = event.left;
            self.drag_frames = self.drag_frames.saturating_add(1);
            self.drag_packets = self.drag_packets.saturating_add(u32::from(event.packets));
            return Redraw::DragMove {
                previous: old_window,
                first,
            };
        }

        let pressed = event.left && !self.previous_left;
        self.previous_left = event.left;
        if !pressed {
            return Redraw::None;
        }

        let x = self.mouse_x;
        let y = self.mouse_y;
        if self.start_button().contains(x, y) {
            self.start_open = !self.start_open;
            serial::put_str("[wm] start menu toggled\n");
            return Redraw::All;
        }
        if self.start_open {
            if self.start_terminal_item().contains(x, y) {
                self.open_terminal();
                self.start_open = false;
                return Redraw::All;
            }
            if self.start_shutdown_item().contains(x, y) {
                shutdown();
            }
            self.start_open = false;
            return Redraw::All;
        }
        if self.desktop_terminal_icon().contains(x, y) {
            self.open_terminal();
            return Redraw::All;
        }
        if self.task_terminal_button().contains(x, y) && !self.window.is_closed() {
            let command = if self.window.is_minimized() {
                WindowCommand::restore(TERMINAL_WINDOW_ID)
            } else {
                WindowCommand::minimize(TERMINAL_WINDOW_ID)
            };
            let _ = self.apply_window_command(command);
            return Redraw::All;
        }
        if !self.window.is_visible() {
            return Redraw::None;
        }

        let (minimize, maximize, close) = self.window_controls();
        if close.is_some_and(|rect| rect.contains(x, y)) {
            if let Ok(request) = self.window.request_close() {
                self.push_window_event(request);
            }
            let _ = self.apply_window_command(WindowCommand::close(TERMINAL_WINDOW_ID));
            serial::put_str("[wm] terminal closed\n");
            return Redraw::All;
        }
        if minimize.is_some_and(|rect| rect.contains(x, y)) {
            let _ = self.apply_window_command(WindowCommand::minimize(TERMINAL_WINDOW_ID));
            serial::put_str("[wm] terminal minimized\n");
            return Redraw::All;
        }
        if maximize.is_some_and(|rect| rect.contains(x, y)) {
            self.toggle_maximize();
            serial::put_str("[wm] terminal maximize toggled\n");
            return Redraw::All;
        }
        let rect = self.window.rect();
        let resize_edges = if self.window.style().contains(WindowStyle::RESIZABLE)
            && !self.window.is_maximized()
        {
            hit_test_resize(rect, x, y, RESIZE_BORDER)
        } else {
            ResizeEdges::NONE
        };
        if !resize_edges.is_empty() {
            self.interaction = WindowInteraction::Resize {
                edges: resize_edges,
                start_mouse_x: x,
                start_mouse_y: y,
                start: rect,
            };
            self.begin_window_gesture();
            serial::put_str("[wm] terminal resize started\n");
            return Redraw::None;
        }
        let rect = self.window_rect();
        let title = Rect::new(rect.x, rect.y, rect.width.saturating_sub(100), TITLE_HEIGHT);
        if self.window.style().contains(WindowStyle::TITLE_BAR)
            && self.window.style().contains(WindowStyle::MOVABLE)
            && title.contains(x, y)
            && !self.window.is_maximized()
        {
            self.interaction = WindowInteraction::Move {
                offset_x: x - rect.x,
                offset_y: y - rect.y,
            };
            self.begin_window_gesture();
            serial::put_str("[wm] terminal drag started\n");
        }
        Redraw::None
    }

    fn open_terminal(&mut self) {
        let command = if self.window.is_closed() {
            WindowCommand::show(TERMINAL_WINDOW_ID)
        } else if self.window.is_minimized() {
            WindowCommand::restore(TERMINAL_WINDOW_ID)
        } else {
            return;
        };
        let _ = self.apply_window_command(command);
    }

    fn window_rect(&self) -> Rect {
        video_rect(self.window.rect())
    }

    fn window_content_rect(&self) -> Rect {
        let rect = self.window_rect();
        let border = u32::from(self.window.style().contains(WindowStyle::BORDER)) * 4;
        let top = if self.window.style().contains(WindowStyle::TITLE_BAR) {
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
        match self.window.apply(command, work_area) {
            Ok(event) => {
                self.push_window_event(event);
                true
            }
            Err(_) => false,
        }
    }

    fn push_window_event(&mut self, event: WindowEvent) {
        if !self.window_events.push(event) {
            // GUI client не должен молча терять lifecycle event. В bootstrap
            // сессии client живёт в том же event loop, поэтому сначала
            // дренируем очередь и повторяем. Ring-3 displayd вернёт IPC
            // backpressure отправителю.
            self.dispatch_window_events();
            let _ = self.window_events.push(event);
        }
    }

    fn dispatch_window_events(&mut self) {
        while let Some(event) = self.window_events.pop() {
            // Terminal пока встроен в session и не требует IPC. Маркеры
            // lifecycle проверяют двунаправленный API, но не спамят serial
            // на каждом pixel move/resize.
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

    fn begin_window_gesture(&mut self) {
        self.drag_frames = 0;
        self.drag_packets = 0;
        self.drag_present_pixels = 0;
        self.drag_preview_visible = false;
    }

    fn finish_window_gesture(&mut self, resized: bool) -> Redraw {
        self.interaction = WindowInteraction::None;
        let preview = self.window_rect();
        let visible = self.drag_preview_visible;
        self.drag_preview_visible = false;
        self.previous_left = false;
        Redraw::DragEnd {
            preview,
            visible,
            resized,
        }
    }

    /// Перестраивает desktop geometry после подтверждённого native mode-set.
    /// Ни один старый rectangle или сохранённый cursor pixel не должен
    /// ссылаться на освобождённую поверхность прежнего размера.
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
        let event = self.window.reflow(work_area);
        self.push_window_event(event);
    }

    fn toggle_maximize(&mut self) {
        let command = if self.window.is_maximized() {
            WindowCommand::restore(TERMINAL_WINDOW_ID)
        } else {
            WindowCommand::maximize(TERMINAL_WINDOW_ID)
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
        self.render_wallpaper();
        self.render_desktop_icons();
        self.render_taskbar();
        let _ = self.framebuffer.cache_background();
        if self.window.is_visible() {
            self.render_window();
        }
        if self.start_open {
            self.render_start_menu();
        }
    }

    /// Во время drag показывает лёгкий preview (title bar + контур), а не
    /// перерисовывает и не копирует мегабайты содержимого окна на каждый
    /// PS/2-пакет. Полное окно появляется один раз при mouse-up.
    fn render_drag_preview(&mut self, previous: Rect, first: bool) -> bool {
        if !self.framebuffer.has_background_cache() {
            self.render_scene();
            return false;
        }
        if first {
            let _ = self.framebuffer.restore_background(window_damage(previous));
        } else {
            self.restore_preview(previous);
        }
        self.draw_drag_preview(self.window_rect());
        self.drag_preview_visible = true;
        true
    }

    fn render_drag_end(&mut self, preview: Rect, visible: bool) -> bool {
        if !visible {
            return self.framebuffer.has_background_cache();
        }
        if !self.framebuffer.has_background_cache() {
            self.render_scene();
            return false;
        }
        self.restore_preview(preview);
        self.render_window();
        true
    }

    fn draw_drag_preview(&mut self, rect: Rect) {
        self.framebuffer.fill_rect(
            Rect::new(rect.x, rect.y, rect.width, TITLE_HEIGHT),
            Color::rgb(28, 43, 62),
        );
        self.framebuffer.border(rect, Theme::ACCENT);
        font::draw_text(
            &mut self.framebuffer,
            rect.x + 12,
            rect.y + 11,
            "RUSTOS TERMINAL",
            Theme::TEXT,
            1,
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

    fn log_drag_finished(&self, resized: bool) {
        serial::put_str(if resized {
            "[wm] terminal resize finished frames="
        } else {
            "[wm] terminal drag finished frames="
        });
        serial::put_u32(self.drag_frames);
        serial::put_str(" packets=");
        serial::put_u32(self.drag_packets);
        serial::put_str(" present-kpx=");
        serial::put_u32((self.drag_present_pixels / 1000) as u32);
        serial::put_str(" compositor=");
        serial::put_str(if self.framebuffer.has_background_cache() {
            "preview"
        } else {
            "full"
        });
        serial::put_str("\n");
    }

    fn render_wallpaper(&mut self) {
        let width = self.framebuffer.width();
        let height = self.framebuffer.height().saturating_sub(TASKBAR_HEIGHT);
        self.framebuffer.fill(Theme::DESKTOP_TOP);
        self.framebuffer.vertical_gradient(
            Rect::new(0, 0, width, height),
            Theme::DESKTOP_TOP,
            Theme::DESKTOP_BOTTOM,
        );

        // Спокойные полупрозрачные волны имитируются смешанными полосами.
        let band = Color::rgb(40, 109, 139);
        for y in 0..height {
            let center = height as i32 / 2 + ((y as i32 / 18) % 7 - 3) * 3;
            let x = center + (y as i32 * 3 / 2);
            self.framebuffer.fill_rect(
                Rect::new(x - 260, y as i32, 520, 1),
                band.mix(Theme::DESKTOP_BOTTOM, 145),
            );
        }
        self.framebuffer.horizontal_gradient(
            Rect::new(0, height as i32 - 120, width, 120),
            Color::rgb(12, 51, 76),
            Color::rgb(37, 77, 98),
        );
        let branding_x = self.framebuffer.width() as i32 - 210;
        let branding_y = self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 - 28;
        font::draw_text(
            &mut self.framebuffer,
            branding_x,
            branding_y,
            arch::ARCH_NAME,
            Color::rgb(119, 158, 181),
            1,
        );
    }

    fn render_desktop_icons(&mut self) {
        let terminal = self.desktop_terminal_icon();
        components::terminal_icon(
            &mut self.framebuffer,
            Rect::new(terminal.x + 12, terminal.y + 3, 48, 48),
        );
        font::draw_text(
            &mut self.framebuffer,
            terminal.x + 5,
            terminal.y + 61,
            "TERMINAL",
            Theme::TEXT,
            1,
        );
        let trash = Rect::new(28, 138, 74, 82);
        components::trash_icon(
            &mut self.framebuffer,
            Rect::new(trash.x + 12, trash.y + 2, 48, 52),
        );
        font::draw_text(
            &mut self.framebuffer,
            trash.x + 16,
            trash.y + 63,
            "TRASH",
            Theme::TEXT,
            1,
        );
    }

    fn render_window_area(&mut self) {
        if self.window.is_visible() {
            self.render_window();
        }
    }

    fn render_window(&mut self) {
        let rect = self.window_rect();
        let style = self.window.style();
        // Тень рисуется только двумя видимыми полосами. Заливать весь
        // прямоугольник под непрозрачным окном — сотни тысяч лишних stores.
        self.framebuffer.fill_rect(
            Rect::new(rect.x + rect.width as i32, rect.y + 8, 7, rect.height),
            Color::rgb(7, 12, 20),
        );
        self.framebuffer.fill_rect(
            Rect::new(rect.x + 7, rect.y + rect.height as i32, rect.width, 8),
            Color::rgb(7, 12, 20),
        );
        Panel {
            rect,
            color: Theme::PANEL,
            border: style.contains(WindowStyle::BORDER).then_some(Theme::BORDER),
        }
        .draw(&mut self.framebuffer);
        if style.contains(WindowStyle::TITLE_BAR) {
            self.framebuffer.horizontal_gradient(
                Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, TITLE_HEIGHT - 1),
                Color::rgb(32, 48, 68),
                Color::rgb(22, 32, 48),
            );
            components::terminal_icon(
                &mut self.framebuffer,
                Rect::new(rect.x + 8, rect.y + 6, 22, 22),
            );
            Label {
                rect: Rect::new(rect.x + 38, rect.y + 11, 180, 16),
                text: "RUSTOS TERMINAL",
                color: Theme::TEXT,
                scale: 1,
            }
            .draw(&mut self.framebuffer);

            let (minimize, maximize, close) = self.window_controls();
            if let Some(rect) = minimize {
                Button {
                    rect,
                    label: "-",
                    hovered: rect.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: false,
                }
                .draw(&mut self.framebuffer);
            }
            if let Some(rect) = maximize {
                Button {
                    rect,
                    label: if self.window.is_maximized() {
                        "[]"
                    } else {
                        "+"
                    },
                    hovered: rect.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: false,
                }
                .draw(&mut self.framebuffer);
            }
            if let Some(rect) = close {
                Button {
                    rect,
                    label: "X",
                    hovered: rect.contains(self.mouse_x, self.mouse_y),
                    pressed: false,
                    danger: true,
                }
                .draw(&mut self.framebuffer);
            }
        }
        if style.contains(WindowStyle::BORDER)
            && style.contains(WindowStyle::RESIZABLE)
            && !self.window.is_maximized()
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
        let content = self.window_content_rect();
        self.terminal.draw(&mut self.framebuffer, content);
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
            1,
        );
        if !self.window.is_closed() {
            let task = self.task_terminal_button();
            self.framebuffer.fill_rect(
                task,
                if self.window.is_minimized() {
                    Theme::PANEL
                } else {
                    Theme::PANEL_LIGHT
                },
            );
            components::terminal_icon(
                &mut self.framebuffer,
                Rect::new(task.x + 7, task.y + 7, 28, 28),
            );
            font::draw_text(
                &mut self.framebuffer,
                task.x + 43,
                task.y + 17,
                "TERMINAL",
                Theme::TEXT,
                1,
            );
        }
        let status_x = self.framebuffer.width() as i32 - 150;
        font::draw_text(
            &mut self.framebuffer,
            status_x,
            y + 17,
            "CPU0  64-BIT",
            Theme::TEXT_MUTED,
            1,
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
            2,
        );
        let terminal = self.start_terminal_item();
        self.framebuffer.fill_rect(terminal, Theme::PANEL_LIGHT);
        components::terminal_icon(
            &mut self.framebuffer,
            Rect::new(terminal.x + 10, terminal.y + 8, 34, 34),
        );
        font::draw_text(
            &mut self.framebuffer,
            terminal.x + 58,
            terminal.y + 20,
            "TERMINAL",
            Theme::TEXT,
            1,
        );
        let shutdown = self.start_shutdown_item();
        self.framebuffer.fill_rect(shutdown, Color::rgb(45, 31, 39));
        font::draw_text(
            &mut self.framebuffer,
            shutdown.x + 18,
            shutdown.y + 17,
            "SHUTDOWN",
            Color::rgb(245, 151, 157),
            1,
        );
    }

    fn desktop_terminal_icon(&self) -> Rect {
        Rect::new(28, 35, 74, 86)
    }

    fn start_button(&self) -> Rect {
        Rect::new(
            6,
            self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 + 4,
            112,
            TASKBAR_HEIGHT - 8,
        )
    }

    fn task_terminal_button(&self) -> Rect {
        Rect::new(
            126,
            self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 + 4,
            180,
            TASKBAR_HEIGHT - 8,
        )
    }

    fn start_menu(&self) -> Rect {
        Rect::new(
            6,
            self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 - 230,
            300,
            224,
        )
    }

    fn start_terminal_item(&self) -> Rect {
        let menu = self.start_menu();
        Rect::new(menu.x + 12, menu.y + 65, menu.width - 24, 52)
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

    fn window_controls(&self) -> (Option<Rect>, Option<Rect>, Option<Rect>) {
        let style = self.window.style();
        if !style.contains(WindowStyle::TITLE_BAR) {
            return (None, None, None);
        }

        // Кнопки укладываются справа налево. Поэтому скрытие любой из них не
        // оставляет пустую «дырку» в title bar и не меняет hit-test остальных.
        let rect = self.window_rect();
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
    Window,
    All,
    DragMove {
        previous: Rect,
        first: bool,
    },
    DragEnd {
        preview: Rect,
        visible: bool,
        resized: bool,
    },
}

/// Переводит ABI geometry в локальный тип software renderer'а.
const fn video_rect(rect: WindowRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}

/// Переводит geometry renderer'а в не зависящий от реализации ABI.
const fn window_rect(rect: Rect) -> WindowRect {
    WindowRect::new(rect.x, rect.y, rect.width, rect.height)
}

/// Выделяет место для одной видимой системной кнопки справа налево.
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
        rect.width.saturating_add(7),
        rect.height.saturating_add(8),
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

/// Мышиный курсор со «сохранённым фоном»: `draw` снимает текущие пиксели
/// области в `saved`, затем рисует стрелку; `restore` возвращает фон
/// перед перерисовкой того же места. Так event loop обновляет только
/// две маленькие области вместо всего кадра.
struct Cursor {
    saved: [u32; CURSOR_WIDTH * CURSOR_HEIGHT],
    x: i32,
    y: i32,
    valid: bool,
}

impl Cursor {
    const fn new() -> Self {
        Self {
            saved: [0; CURSOR_WIDTH * CURSOR_HEIGHT],
            x: 0,
            y: 0,
            valid: false,
        }
    }

    fn invalidate(&mut self) {
        self.valid = false;
    }

    fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, CURSOR_WIDTH as u32, CURSOR_HEIGHT as u32)
    }

    fn restore(&mut self, fb: &mut Framebuffer) {
        if !self.valid {
            return;
        }
        for dy in 0..CURSOR_HEIGHT {
            for dx in 0..CURSOR_WIDTH {
                let x = self.x + dx as i32;
                let y = self.y + dy as i32;
                if x >= 0 && y >= 0 && x < fb.width() as i32 && y < fb.height() as i32 {
                    fb.write_raw(x as u32, y as u32, self.saved[dy * CURSOR_WIDTH + dx]);
                }
            }
        }
        self.valid = false;
    }

    fn draw(&mut self, fb: &mut Framebuffer, x: i32, y: i32) {
        self.x = x;
        self.y = y;
        for dy in 0..CURSOR_HEIGHT {
            for dx in 0..CURSOR_WIDTH {
                let px = x + dx as i32;
                let py = y + dy as i32;
                if px >= 0 && py >= 0 && px < fb.width() as i32 && py < fb.height() as i32 {
                    self.saved[dy * CURSOR_WIDTH + dx] = fb.read_raw(px as u32, py as u32);
                    let inside = dx == 0 || (dy < 14 && dx <= dy / 2 + 1) || (dy >= 11 && dx == 5);
                    if inside {
                        let outline = dx == 0 || dx == dy / 2 + 1 || dy == 13;
                        fb.put_pixel(
                            px,
                            py,
                            if outline {
                                Color::rgb(7, 12, 20)
                            } else {
                                Color::rgb(242, 248, 252)
                            },
                        );
                    }
                }
            }
        }
        self.valid = true;
    }
}

/// Platform power off: ACPI PM на PC либо PSCI на AArch64.
fn shutdown() -> ! {
    serial::put_str("[platform] shutdown requested\n");
    arch::power_off()
}

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
    input::{Event, Key, MouseEvent, Ps2Input},
    serial,
};
use rustos_abi::BootInfo;

const TASKBAR_HEIGHT: u32 = 46;
const TITLE_HEIGHT: u32 = 34;
const CURSOR_WIDTH: usize = 14;
const CURSOR_HEIGHT: usize = 20;

pub fn run(info: &BootInfo) -> ! {
    let Some(framebuffer) = Framebuffer::from_boot(&info.framebuffer) else {
        serial::put_str("[gui] no supported framebuffer; system halted\n");
        loop {
            arch::halt();
        }
    };
    let mut session = DesktopSession::new(framebuffer, info.total_usable_ram() / (1024 * 1024));
    session.render_all();
    serial::put_str("[gui] GUI_READY desktop=1 terminal=1 mouse=ps2\n");
    session.event_loop()
}

struct WindowState {
    rect: Rect,
    restore_rect: Rect,
    minimized: bool,
    maximized: bool,
    closed: bool,
    dragging: bool,
    drag_dx: i32,
    drag_dy: i32,
}

impl WindowState {
    fn content_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + 4,
            self.rect.y + TITLE_HEIGHT as i32,
            self.rect.width.saturating_sub(8),
            self.rect.height.saturating_sub(TITLE_HEIGHT + 4),
        )
    }
}

struct DesktopSession {
    framebuffer: Framebuffer,
    input: Ps2Input,
    terminal: Terminal,
    window: WindowState,
    cursor: Cursor,
    mouse_x: i32,
    mouse_y: i32,
    previous_left: bool,
    start_open: bool,
}

impl DesktopSession {
    fn new(framebuffer: Framebuffer, usable_ram_mib: u64) -> Self {
        let screen_width = framebuffer.width();
        let screen_height = framebuffer.height();
        let width = screen_width.saturating_sub(180).clamp(620, 1040);
        let height = screen_height.saturating_sub(160).clamp(420, 650);
        let x = ((screen_width - width) / 2) as i32;
        let y = ((screen_height.saturating_sub(TASKBAR_HEIGHT) - height) / 2) as i32;
        let rect = Rect::new(x, y, width, height);
        Self {
            mouse_x: (screen_width / 2) as i32,
            mouse_y: (screen_height / 2) as i32,
            framebuffer,
            input: Ps2Input::new(),
            terminal: Terminal::new(usable_ram_mib),
            window: WindowState {
                rect,
                restore_rect: rect,
                minimized: false,
                maximized: false,
                closed: false,
                dragging: false,
                drag_dx: 0,
                drag_dy: 0,
            },
            cursor: Cursor::new(),
            previous_left: false,
            start_open: false,
        }
    }

    fn event_loop(&mut self) -> ! {
        loop {
            if let Some(event) = self.input.poll() {
                self.cursor.restore(&mut self.framebuffer);
                let redraw = match event {
                    Event::Key(key) => self.handle_key(key),
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                };
                if redraw == Redraw::All {
                    self.render_scene();
                } else if redraw == Redraw::Window {
                    self.render_window_area();
                    self.render_taskbar();
                    if self.start_open {
                        self.render_start_menu();
                    }
                }
                self.cursor
                    .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
            } else {
                core::hint::spin_loop();
            }
        }
    }

    fn handle_key(&mut self, key: Key) -> Redraw {
        if matches!(key, Key::Escape) && self.start_open {
            self.start_open = false;
            return Redraw::All;
        }
        if self.window.closed || self.window.minimized {
            return Redraw::None;
        }
        if self.terminal.handle_key(key) == TerminalAction::Shutdown {
            shutdown();
        }
        Redraw::Window
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

        if self.window.dragging && event.left {
            self.window.rect.x = (self.mouse_x - self.window.drag_dx).clamp(
                -((self.window.rect.width as i32) - 120),
                self.framebuffer.width() as i32 - 120,
            );
            self.window.rect.y = (self.mouse_y - self.window.drag_dy).clamp(
                0,
                self.framebuffer.height() as i32 - TASKBAR_HEIGHT as i32 - TITLE_HEIGHT as i32,
            );
            self.previous_left = event.left;
            return Redraw::All;
        }
        if self.window.dragging && !event.left {
            self.window.dragging = false;
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
        if self.task_terminal_button().contains(x, y) && !self.window.closed {
            self.window.minimized = !self.window.minimized;
            return Redraw::All;
        }
        if self.window.closed || self.window.minimized || !self.window.rect.contains(x, y) {
            return Redraw::None;
        }

        let (minimize, maximize, close) = self.window_controls();
        if close.contains(x, y) {
            self.window.closed = true;
            serial::put_str("[wm] terminal closed\n");
            return Redraw::All;
        }
        if minimize.contains(x, y) {
            self.window.minimized = true;
            serial::put_str("[wm] terminal minimized\n");
            return Redraw::All;
        }
        if maximize.contains(x, y) {
            self.toggle_maximize();
            serial::put_str("[wm] terminal maximize toggled\n");
            return Redraw::All;
        }
        let title = Rect::new(
            self.window.rect.x,
            self.window.rect.y,
            self.window.rect.width.saturating_sub(100),
            TITLE_HEIGHT,
        );
        if title.contains(x, y) && !self.window.maximized {
            self.window.dragging = true;
            self.window.drag_dx = x - self.window.rect.x;
            self.window.drag_dy = y - self.window.rect.y;
        }
        Redraw::None
    }

    fn open_terminal(&mut self) {
        self.window.closed = false;
        self.window.minimized = false;
    }

    fn toggle_maximize(&mut self) {
        if self.window.maximized {
            self.window.rect = self.window.restore_rect;
            self.window.maximized = false;
        } else {
            self.window.restore_rect = self.window.rect;
            self.window.rect = Rect::new(
                8,
                8,
                self.framebuffer.width().saturating_sub(16),
                self.framebuffer
                    .height()
                    .saturating_sub(TASKBAR_HEIGHT + 16),
            );
            self.window.maximized = true;
        }
    }

    fn render_all(&mut self) {
        self.render_scene();
        self.cursor
            .draw(&mut self.framebuffer, self.mouse_x, self.mouse_y);
    }

    fn render_scene(&mut self) {
        self.render_wallpaper();
        self.render_desktop_icons();
        if !self.window.closed && !self.window.minimized {
            self.render_window();
        }
        self.render_taskbar();
        if self.start_open {
            self.render_start_menu();
        }
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
            "RUSTOS / X86-64",
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
        if !self.window.closed && !self.window.minimized {
            self.render_window();
        }
    }

    fn render_window(&mut self) {
        let rect = self.window.rect;
        // Небольшая тень подчёркивает границу окна на тёмных обоях.
        self.framebuffer.fill_rect(
            Rect::new(rect.x + 7, rect.y + 8, rect.width, rect.height),
            Color::rgb(7, 12, 20),
        );
        Panel {
            rect,
            color: Theme::PANEL,
            border: Some(Theme::BORDER),
        }
        .draw(&mut self.framebuffer);
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
        Button {
            rect: minimize,
            label: "-",
            hovered: minimize.contains(self.mouse_x, self.mouse_y),
            pressed: false,
            danger: false,
        }
        .draw(&mut self.framebuffer);
        Button {
            rect: maximize,
            label: if self.window.maximized { "[]" } else { "+" },
            hovered: maximize.contains(self.mouse_x, self.mouse_y),
            pressed: false,
            danger: false,
        }
        .draw(&mut self.framebuffer);
        Button {
            rect: close,
            label: "X",
            hovered: close.contains(self.mouse_x, self.mouse_y),
            pressed: false,
            danger: true,
        }
        .draw(&mut self.framebuffer);
        self.terminal
            .draw(&mut self.framebuffer, self.window.content_rect());
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
        if !self.window.closed {
            let task = self.task_terminal_button();
            self.framebuffer.fill_rect(
                task,
                if self.window.minimized {
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

    fn window_controls(&self) -> (Rect, Rect, Rect) {
        let y = self.window.rect.y + 4;
        let close = Rect::new(
            self.window.rect.x + self.window.rect.width as i32 - 32,
            y,
            28,
            26,
        );
        let maximize = Rect::new(close.x - 31, y, 28, 26);
        let minimize = Rect::new(maximize.x - 31, y, 28, 26);
        (minimize, maximize, close)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Redraw {
    None,
    Window,
    All,
}

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

fn shutdown() -> ! {
    serial::put_str("[platform] ACPI shutdown requested\n");
    // QEMU q35 ACPI PM control block: SLP_TYP=0, SLP_EN=1<<13.
    unsafe { arch::outw(0x604, 0x2000) };
    loop {
        arch::halt();
    }
}

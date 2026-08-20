//! Первый GUI-клиент RustOS: цветной системный terminal.
//!
//! Файловые команды раннего shell ещё вызывают bootstrap VFS напрямую, но
//! `RUN` уже проходит через постоянный process manager, ring-3 `vfsd`, RUNE
//! loader и capability pipes. Следующий перенос вынесет сам shell/console в
//! user space, не меняя terminal widget и оконный менеджер.

use crate::{
    arch, font,
    fs::{BootstrapFs, FileKind, FsError, FILE_CAPACITY, PATH_CAPACITY},
    graphics::{Color, Framebuffer, Rect},
    input::Key,
    serial,
};
use rustos_abi::bootinfo::BootInitramfs;
use rustos_abi::input::{MouseCapabilities, MouseSettings, PointerCursor};
use rustos_system_assets::WallpaperId;
use rustos_video::{ColorMode, DisplayMode, ModeSetError};

/// Размер логического буфера terminal. Число видимых строк и столбцов
/// вычисляется из текущего размера окна и выбранного системного шрифта.
const COLS: usize = 118;
const ROWS: usize = 52;
/// Максимальная длина строки ввода (без учёта переноса).
const INPUT_CAPACITY: usize = 96;

const WHITE: Color = Color::rgb(225, 234, 242);
const MUTED: Color = Color::rgb(139, 158, 178);
const CYAN: Color = Color::rgb(83, 207, 222);
const GREEN: Color = Color::rgb(116, 216, 153);
const YELLOW: Color = Color::rgb(238, 198, 108);
const RED: Color = Color::rgb(239, 110, 118);
const BACKGROUND: Color = Color::rgb(7, 12, 20);

/// Компактная клетка terminal: BMP code point + индекс фиксированной палитры.
/// Latin/Cyrillic целиком лежат в BMP. Важно не хранить здесь `char + Color`:
/// из-за alignment такая клетка заняла бы 8 байт и переполнила ранний 128-KiB
/// kernel stack при глубоком process-spawn пути.
#[derive(Clone, Copy)]
struct Cell {
    character: u16,
    color: u8,
}

const _: [(); 4] = [(); core::mem::size_of::<Cell>()];

const EMPTY_CELL: Cell = Cell {
    character: ' ' as u16,
    color: COLOR_WHITE,
};

const COLOR_WHITE: u8 = 0;
const COLOR_MUTED: u8 = 1;
const COLOR_CYAN: u8 = 2;
const COLOR_GREEN: u8 = 3;
const COLOR_YELLOW: u8 = 4;
const COLOR_RED: u8 = 5;

impl Cell {
    fn character(self) -> char {
        char::from_u32(u32::from(self.character)).unwrap_or('�')
    }

    fn color(self) -> Color {
        match self.color {
            COLOR_MUTED => MUTED,
            COLOR_CYAN => CYAN,
            COLOR_GREEN => GREEN,
            COLOR_YELLOW => YELLOW,
            COLOR_RED => RED,
            _ => WHITE,
        }
    }
}

fn color_index(color: Color) -> u8 {
    if color == MUTED {
        COLOR_MUTED
    } else if color == CYAN {
        COLOR_CYAN
    } else if color == GREEN {
        COLOR_GREEN
    } else if color == YELLOW {
        COLOR_YELLOW
    } else if color == RED {
        COLOR_RED
    } else {
        COLOR_WHITE
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalAction {
    /// Состояние экрана не изменилось.
    None,
    /// Изменилась только текущая строка ввода.
    RedrawInputLine,
    /// Команда изменила несколько строк или прокрутила терминал.
    RedrawAll,
    /// Запросить текущий monitor/scanout mode у display driver'а.
    DisplayInfo,
    /// Показать все режимы, доступные monitor/display driver'у.
    DisplayModes,
    /// Запросить физический mode-set. Firmware framebuffer может вернуть,
    /// что режим применяется только через меню GRUB после перезапуска.
    DisplayMode {
        width: u32,
        height: u32,
    },
    /// Переключить software-renderer между 24-bit, RGB565 и grayscale.
    DisplayColor(ColorMode),
    /// Открыть системное демонстрационное приложение UI Gallery.
    OpenUiShowcase,
    /// Показать или изменить профиль мыши.
    Mouse(MouseCommand),
    /// Выбрать cursor pack или временно показать конкретный cursor shape.
    Cursor(CursorCommand),
    /// Выбрать icon pack.
    Icons(IconThemeName),
    /// Выбрать системные обои.
    Wallpaper(WallpaperId),
    Shutdown,
}

/// Команда input service, разобранная shell без доступа к драйверу.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseCommand {
    /// Показать профиль и hardware capabilities.
    Info,
    /// Частота hardware reports, Гц.
    Rate(u16),
    /// Разрешение PS/2, уровень 0..3.
    Resolution(u8),
    /// Линейная чувствительность, проценты.
    Sensitivity(u16),
    /// Ускорение быстрых движений, проценты.
    Acceleration(u16),
    /// Окно двойного клика, миллисекунды.
    DoubleClick(u16),
    /// Подавление повторного контакта, миллисекунды.
    Debounce(u16),
    /// Порог начала drag, пиксели.
    DragThreshold(u16),
}

/// Управление cursor service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorCommand {
    /// Вернуть автоматический выбор курсора по hit-test.
    Auto,
    /// Выбрать тему.
    Theme(CursorThemeName),
    /// Зафиксировать форму для просмотра и отладки.
    Preview(PointerCursor),
}

/// Имена встроенных cursor packs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorThemeName {
    /// Светлая.
    Light,
    /// Тёмная.
    Midnight,
    /// Высококонтрастная.
    Contrast,
}

/// Имена встроенных icon packs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IconThemeName {
    /// Классическая, с жёлтыми папками.
    Classic,
    /// Тёмная.
    Midnight,
    /// Монохромная.
    Mono,
}

/// Буфер экрана + shell + доступ к bootstrap-файловой системе.
/// Держит собственный cwd: у ядра пока нет общего «процессного» контекста.
pub struct Terminal {
    cells: [Cell; COLS * ROWS],
    column: usize,
    row: usize,
    input: [u8; INPUT_CAPACITY],
    input_len: usize,
    usable_ram_mib: u64,
    font_style: font::FontStyle,
    fs: BootstrapFs,
    cwd: [u8; PATH_CAPACITY],
    cwd_len: usize,
}

impl Terminal {
    /// Создаёт терминал, монтирует initramfs в `/boot` (RO) + RAM-оверлей
    /// (RW) и печатает приветственный banner.
    pub fn new(usable_ram_mib: u64, initramfs: BootInitramfs) -> Self {
        let mut cwd = [0u8; PATH_CAPACITY];
        cwd[0] = b'/';
        let mut terminal = Self {
            cells: [EMPTY_CELL; COLS * ROWS],
            column: 0,
            row: 0,
            input: [0; INPUT_CAPACITY],
            input_len: 0,
            usable_ram_mib,
            font_style: font::TERMINAL_DEFAULT,
            fs: BootstrapFs::new(initramfs),
            cwd,
            cwd_len: 1,
        };
        terminal.print("RUSTOS GRAPHICAL TERMINAL 0.2\n", CYAN);
        terminal.print("СИСТЕМНЫЙ ТЕРМИНАЛ · LATIN + КИРИЛЛИЦА\n", MUTED);
        terminal.print("BOOT VFS: RIFS /BOOT (RO) + RAM OVERLAY (RW)\n", GREEN);
        terminal.print("TYPE HELP TO SEE AVAILABLE COMMANDS.\n\n", WHITE);
        terminal.prompt();
        terminal
    }

    /// Принимает нажатие клавиши и обновляет буфер ввода/экрана.
    /// Возвращает, какую часть экрана compositor должен перерисовать.
    pub fn handle_key(&mut self, key: Key) -> TerminalAction {
        let previous_row = self.row;
        match key {
            Key::Character(byte) if byte.is_ascii_graphic() || byte == b' ' => {
                if self.input_len < INPUT_CAPACITY {
                    self.input[self.input_len] = byte;
                    self.input_len += 1;
                    self.put(char::from(byte), WHITE);
                    return self.input_redraw(previous_row);
                }
            }
            Key::Backspace => {
                if self.input_len > 0 {
                    self.input_len -= 1;
                    self.input[self.input_len] = 0;
                    self.backspace();
                    return self.input_redraw(previous_row);
                }
            }
            Key::Enter => return self.execute(),
            Key::Tab => {
                for _ in 0..4 {
                    if self.input_len < INPUT_CAPACITY {
                        self.input[self.input_len] = b' ';
                        self.input_len += 1;
                        self.put(' ', WHITE);
                    }
                }
                return self.input_redraw(previous_row);
            }
            Key::Escape | Key::Character(_) => {}
        }
        TerminalAction::None
    }

    /// Быстрый путь для интерактивного ввода. Пока курсор остаётся на той
    /// же строке, compositor перерисует только её. Перенос или scroll требует
    /// полного обновления содержимого окна.
    fn input_redraw(&self, previous_row: usize) -> TerminalAction {
        if self.row == previous_row {
            TerminalAction::RedrawInputLine
        } else {
            TerminalAction::RedrawAll
        }
    }

    /// Рисует видимую часть буфера в `rect` (отступ 6px, автопрокрутка
    /// к последней строке) + caret. Полная отрисовка окна.
    pub fn draw(&self, fb: &mut Framebuffer, rect: Rect) {
        fb.fill_rect(rect, BACKGROUND);
        let cell_width = self.font_style.cell_width().max(1);
        let line_height = self.font_style.line_height().max(1);
        let visible_cols =
            ((rect.width.saturating_sub(12)) / cell_width as u32).min(COLS as u32) as usize;
        let visible_rows =
            ((rect.height.saturating_sub(12)) / line_height as u32).min(ROWS as u32) as usize;
        let first_row = self.row.saturating_add(1).saturating_sub(visible_rows);
        for screen_row in 0..visible_rows {
            let source_row = first_row + screen_row;
            if source_row >= ROWS {
                break;
            }
            for column in 0..visible_cols {
                let cell = self.cells[source_row * COLS + column];
                if cell.character != ' ' as u16 {
                    font::draw_char(
                        fb,
                        rect.x + 6 + column as i32 * cell_width,
                        rect.y + 6 + screen_row as i32 * line_height,
                        cell.character(),
                        cell.color(),
                        self.font_style,
                    );
                }
            }
        }

        // Тонкий caret в активной позиции.
        if self.row >= first_row && self.row < first_row + visible_rows {
            let caret_x = rect.x + 6 + self.column as i32 * cell_width;
            let caret_y = rect.y
                + 6
                + (self.row - first_row) as i32 * line_height
                + line_height.saturating_sub(2);
            fb.fill_rect(Rect::new(caret_x, caret_y, cell_width as u32, 2), CYAN);
        }
    }

    /// Перерисовывает только видимую строку с caret и возвращает её dirty
    /// rectangle. Это критично для раннего polling input: полный software
    /// redraw на каждую букву надолго оставлял виртуальный 8042 без чтения.
    pub fn draw_input_line(&self, fb: &mut Framebuffer, rect: Rect) -> Option<Rect> {
        let cell_width = self.font_style.cell_width().max(1);
        let line_height = self.font_style.line_height().max(1);
        let visible_cols =
            ((rect.width.saturating_sub(12)) / cell_width as u32).min(COLS as u32) as usize;
        let visible_rows =
            ((rect.height.saturating_sub(12)) / line_height as u32).min(ROWS as u32) as usize;
        let first_row = self.row.saturating_add(1).saturating_sub(visible_rows);
        if self.row < first_row || self.row >= first_row + visible_rows {
            return None;
        }

        let screen_row = self.row - first_row;
        let line = Rect::new(
            rect.x,
            rect.y + 6 + screen_row as i32 * line_height,
            rect.width,
            line_height as u32,
        );
        fb.fill_rect(line, BACKGROUND);
        for column in 0..visible_cols {
            let cell = self.cells[self.row * COLS + column];
            if cell.character != ' ' as u16 {
                font::draw_char(
                    fb,
                    rect.x + 6 + column as i32 * cell_width,
                    line.y,
                    cell.character(),
                    cell.color(),
                    self.font_style,
                );
            }
        }
        let caret_x = rect.x + 6 + self.column as i32 * cell_width;
        fb.fill_rect(
            Rect::new(
                caret_x,
                line.y + line_height.saturating_sub(2),
                cell_width as u32,
                2,
            ),
            CYAN,
        );
        Some(line)
    }

    /// Запускает строку ввода как команду (системные + файловые).
    /// `Shutdown` передаёт наверх — session глушит систему.
    fn execute(&mut self) -> TerminalAction {
        self.newline();
        let input_len = self.input_len;
        let mut command = [0u8; INPUT_CAPACITY];
        command[..input_len].copy_from_slice(&self.input[..input_len]);
        self.input.fill(0);
        self.input_len = 0;
        let command = core::str::from_utf8(&command[..input_len])
            .unwrap_or("")
            .trim();
        serial::put_str("[terminal] command: ");
        serial::put_str(command);
        serial::put_str("\n");

        let action = if command.is_empty() {
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("help") {
            self.print("AVAILABLE COMMANDS:\n", YELLOW);
            self.print("  HELP      SHOW THIS LIST\n", WHITE);
            self.print("  CLEAR     CLEAR TERMINAL\n", WHITE);
            self.print("  ABOUT     SYSTEM INFORMATION\n", WHITE);
            self.print("  MEM       USABLE MEMORY\n", WHITE);
            self.print("  GUI       GUI SERVER STATUS\n", WHITE);
            self.print("  UIDEMO    OPEN SYSTEM UI GALLERY\n", WHITE);
            self.print("  DISPLAY   MONITOR/MODE/COLOR SETTINGS\n", WHITE);
            self.print("  FONT      FAMILY/SIZE/STYLE SETTINGS\n", WHITE);
            self.print("  MOUSE     RATE/SENSITIVITY/CLICK SETTINGS\n", WHITE);
            self.print("  CURSOR    THEME/PREVIEW/AUTO\n", WHITE);
            self.print("  ICONS     SWITCH EXTENSIBLE ICON PACK\n", WHITE);
            self.print("  WALLPAPER SPRING|AUTUMN|WINTER\n", WHITE);
            self.print("  ECHO TEXT PRINT TEXT\n", WHITE);
            self.print("  PWD/CD    CURRENT DIRECTORY\n", WHITE);
            self.print("  LS/CAT    LIST OR READ FILES\n", WHITE);
            self.print("  MKDIR     CREATE DIRECTORY\n", WHITE);
            self.print("  WRITE     REPLACE RAM FILE\n", WHITE);
            self.print("  APPEND    APPEND RAM FILE\n", WHITE);
            self.print("  TOUCH/RM  CREATE OR REMOVE\n", WHITE);
            self.print("  STAT      FILE INFORMATION\n", WHITE);
            self.print("  RUN PATH [ARGS]  START A RING3 RUNE PROGRAM\n", WHITE);
            self.print("  FS        FILE COMMAND HELP\n", WHITE);
            self.print("  SHUTDOWN  POWER OFF VM\n", WHITE);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("clear") {
            self.clear();
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("about") {
            self.print("RUSTOS 0.1.0 ", CYAN);
            self.print(arch::ARCH_NAME, CYAN);
            self.print("\n", CYAN);
            self.print("RUST, GRUB MULTIBOOT2, CPU SOFTWARE COMPOSITOR\n", WHITE);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("mem") {
            self.print("USABLE RAM: ", GREEN);
            self.print_number(self.usable_ram_mib);
            self.print(" MIB\n", GREEN);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("gui") {
            self.print("DISPLAYD: ONLINE\n", GREEN);
            self.print("COMPOSITOR: SOFTWARE / ALIGNED 32-BIT SURFACES\n", WHITE);
            self.print("WINDOW MANAGER: ONLINE\n", GREEN);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("uidemo") {
            TerminalAction::OpenUiShowcase
        } else if command.eq_ignore_ascii_case("display")
            || command
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("display "))
        {
            self.command_display(command)
        } else if command.eq_ignore_ascii_case("font")
            || command
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("font "))
        {
            self.command_font(command);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("mouse")
            || command
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mouse "))
        {
            self.command_mouse(command)
        } else if command.eq_ignore_ascii_case("cursor")
            || command
                .get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cursor "))
        {
            self.command_cursor(command)
        } else if command.eq_ignore_ascii_case("icons")
            || command
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("icons "))
        {
            self.command_icons(command)
        } else if command.eq_ignore_ascii_case("wallpaper")
            || command
                .get(..10)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("wallpaper "))
        {
            self.command_wallpaper(command)
        } else if command.eq_ignore_ascii_case("shutdown") {
            self.print("POWERING OFF...\n", YELLOW);
            TerminalAction::Shutdown
        } else if command.len() >= 5 && command[..5].eq_ignore_ascii_case("echo ") {
            self.print(&command[5..], WHITE);
            self.newline();
            TerminalAction::None
        } else if command.len() > 4 && command[..4].eq_ignore_ascii_case("run ") {
            self.command_run(command[4..].trim());
            TerminalAction::None
        } else if self.execute_fs_command(command) {
            TerminalAction::None
        } else {
            self.print("COMMAND NOT FOUND: ", RED);
            self.print(command, WHITE);
            self.newline();
            TerminalAction::None
        };
        match action {
            TerminalAction::Shutdown
            | TerminalAction::DisplayInfo
            | TerminalAction::DisplayModes
            | TerminalAction::DisplayMode { .. }
            | TerminalAction::DisplayColor(_)
            | TerminalAction::OpenUiShowcase
            | TerminalAction::Mouse(_)
            | TerminalAction::Cursor(_)
            | TerminalAction::Icons(_)
            | TerminalAction::Wallpaper(_) => action,
            _ => {
                self.prompt();
                TerminalAction::RedrawAll
            }
        }
    }

    fn command_display(&mut self, command: &str) -> TerminalAction {
        let arguments = command.get(7..).unwrap_or("").trim();
        if arguments.is_empty() || arguments.eq_ignore_ascii_case("info") {
            return TerminalAction::DisplayInfo;
        }
        if arguments.eq_ignore_ascii_case("modes") {
            return TerminalAction::DisplayModes;
        }
        if let Some(value) = strip_prefix_ascii_case(arguments, "mode ") {
            if let Some((width, height)) = parse_resolution(value.trim()) {
                return TerminalAction::DisplayMode { width, height };
            }
            self.print("USAGE: DISPLAY MODE WIDTHxHEIGHT\n", RED);
            return TerminalAction::None;
        }
        if let Some(value) = strip_prefix_ascii_case(arguments, "color ") {
            let mode = if value.eq_ignore_ascii_case("truecolor")
                || value.eq_ignore_ascii_case("24")
            {
                Some(ColorMode::TrueColor24)
            } else if value.eq_ignore_ascii_case("rgb565") || value.eq_ignore_ascii_case("16") {
                Some(ColorMode::HighColor16)
            } else if value.eq_ignore_ascii_case("gray8") || value.eq_ignore_ascii_case("grayscale")
            {
                Some(ColorMode::Grayscale8)
            } else {
                None
            };
            if let Some(mode) = mode {
                return TerminalAction::DisplayColor(mode);
            }
            self.print("COLOR: TRUECOLOR | RGB565 | GRAY8\n", RED);
            return TerminalAction::None;
        }
        self.print("DISPLAY [INFO]\n", YELLOW);
        self.print("DISPLAY MODES\n", WHITE);
        self.print("DISPLAY MODE WIDTHxHEIGHT\n", WHITE);
        self.print("DISPLAY COLOR TRUECOLOR|RGB565|GRAY8\n", WHITE);
        TerminalAction::None
    }

    fn command_mouse(&mut self, command: &str) -> TerminalAction {
        let arguments = command.get(5..).unwrap_or("").trim();
        if arguments.is_empty() || arguments.eq_ignore_ascii_case("info") {
            return TerminalAction::Mouse(MouseCommand::Info);
        }
        let Some((name, value)) = arguments.split_once(' ') else {
            self.print_mouse_help();
            return TerminalAction::None;
        };
        let Some(number) = value.trim().parse::<u16>().ok() else {
            self.print("MOUSE VALUE MUST BE A POSITIVE INTEGER\n", RED);
            return TerminalAction::None;
        };
        let setting = if name.eq_ignore_ascii_case("rate") {
            MouseCommand::Rate(number)
        } else if name.eq_ignore_ascii_case("resolution") {
            MouseCommand::Resolution(number.min(u16::from(u8::MAX)) as u8)
        } else if name.eq_ignore_ascii_case("sensitivity") || name.eq_ignore_ascii_case("speed") {
            MouseCommand::Sensitivity(number)
        } else if name.eq_ignore_ascii_case("acceleration") {
            MouseCommand::Acceleration(number)
        } else if name.eq_ignore_ascii_case("double") || name.eq_ignore_ascii_case("double-click") {
            MouseCommand::DoubleClick(number)
        } else if name.eq_ignore_ascii_case("debounce") || name.eq_ignore_ascii_case("single-click")
        {
            MouseCommand::Debounce(number)
        } else if name.eq_ignore_ascii_case("drag") {
            MouseCommand::DragThreshold(number)
        } else {
            self.print_mouse_help();
            return TerminalAction::None;
        };
        TerminalAction::Mouse(setting)
    }

    fn print_mouse_help(&mut self) {
        self.print("MOUSE [INFO]\n", YELLOW);
        self.print("MOUSE RATE 10|20|40|60|80|100|200\n", WHITE);
        self.print("MOUSE RESOLUTION 0..3\n", WHITE);
        self.print("MOUSE SENSITIVITY 25..400 (%)\n", WHITE);
        self.print("MOUSE ACCELERATION 0..300 (%)\n", WHITE);
        self.print("MOUSE DOUBLE 100..1200 (MS)\n", WHITE);
        self.print("MOUSE DEBOUNCE 0..250 (MS)\n", WHITE);
        self.print("MOUSE DRAG 1..32 (PX)\n", WHITE);
    }

    fn command_cursor(&mut self, command: &str) -> TerminalAction {
        let arguments = command.get(6..).unwrap_or("").trim();
        if arguments.eq_ignore_ascii_case("auto") {
            return TerminalAction::Cursor(CursorCommand::Auto);
        }
        if let Some(value) = strip_prefix_ascii_case(arguments, "theme ") {
            let theme = if value.eq_ignore_ascii_case("light") {
                Some(CursorThemeName::Light)
            } else if value.eq_ignore_ascii_case("midnight") || value.eq_ignore_ascii_case("dark") {
                Some(CursorThemeName::Midnight)
            } else if value.eq_ignore_ascii_case("contrast") {
                Some(CursorThemeName::Contrast)
            } else {
                None
            };
            if let Some(theme) = theme {
                return TerminalAction::Cursor(CursorCommand::Theme(theme));
            }
        }
        if let Some(value) = strip_prefix_ascii_case(arguments, "preview ") {
            if let Some(cursor) = parse_cursor(value.trim()) {
                return TerminalAction::Cursor(CursorCommand::Preview(cursor));
            }
        }
        self.print("CURSOR THEME LIGHT|MIDNIGHT|CONTRAST\n", YELLOW);
        self.print("CURSOR PREVIEW ARROW|TEXT|LINK|GRAB|GRABBING|BUSY\n", WHITE);
        self.print(
            "               CROSSHAIR|FORBIDDEN|HRESIZE|VRESIZE|NWSE|NESW\n",
            WHITE,
        );
        self.print("CURSOR AUTO\n", WHITE);
        TerminalAction::None
    }

    fn command_icons(&mut self, command: &str) -> TerminalAction {
        let arguments = command.get(5..).unwrap_or("").trim();
        let value = strip_prefix_ascii_case(arguments, "theme ").unwrap_or(arguments);
        let theme = if value.eq_ignore_ascii_case("classic") {
            Some(IconThemeName::Classic)
        } else if value.eq_ignore_ascii_case("midnight") || value.eq_ignore_ascii_case("dark") {
            Some(IconThemeName::Midnight)
        } else if value.eq_ignore_ascii_case("mono") {
            Some(IconThemeName::Mono)
        } else {
            None
        };
        if let Some(theme) = theme {
            TerminalAction::Icons(theme)
        } else {
            self.print("ICONS THEME CLASSIC|MIDNIGHT|MONO\n", YELLOW);
            TerminalAction::None
        }
    }

    fn command_wallpaper(&mut self, command: &str) -> TerminalAction {
        let value = command.get(9..).unwrap_or("").trim();
        let wallpaper = if value.eq_ignore_ascii_case("spring") {
            Some(WallpaperId::SpringRiver)
        } else if value.eq_ignore_ascii_case("autumn") {
            Some(WallpaperId::AutumnRiver)
        } else if value.eq_ignore_ascii_case("winter") {
            Some(WallpaperId::WinterField)
        } else {
            None
        };
        if let Some(wallpaper) = wallpaper {
            TerminalAction::Wallpaper(wallpaper)
        } else {
            self.print("WALLPAPER SPRING|AUTUMN|WINTER\n", YELLOW);
            TerminalAction::None
        }
    }

    /// Настройка типографики терминала без перезапуска GUI. Семейство Sans
    /// полезно для проверки SDK, но default всегда остаётся моноширинным.
    fn command_font(&mut self, command: &str) {
        let arguments = command.get(4..).unwrap_or("").trim();
        if arguments.is_empty() {
            self.print_font_info();
            self.print("FONT FAMILY CONSOLE|SANS\n", WHITE);
            self.print("FONT SIZE 10..48\n", WHITE);
            self.print("FONT STYLE REGULAR|BOLD|ITALIC|BOLDITALIC\n", WHITE);
            return;
        }

        let (setting, value) = arguments.split_once(' ').unwrap_or(("size", arguments));
        let value = value.trim();
        if setting.eq_ignore_ascii_case("family") {
            self.font_style.family = if value.eq_ignore_ascii_case("console") {
                font::FontFamily::Console
            } else if value.eq_ignore_ascii_case("sans") {
                font::FontFamily::Sans
            } else {
                self.print("FONT FAMILY: CONSOLE | SANS\n", RED);
                return;
            };
        } else if setting.eq_ignore_ascii_case("size") {
            let Some(size) = value
                .parse::<u16>()
                .ok()
                .filter(|size| (10..=48).contains(size))
            else {
                self.print("FONT SIZE MUST BE 10..48\n", RED);
                return;
            };
            self.font_style.size = size;
        } else if setting.eq_ignore_ascii_case("style") {
            let (weight, italic) = if value.eq_ignore_ascii_case("regular") {
                (font::FontWeight::Regular, false)
            } else if value.eq_ignore_ascii_case("bold") {
                (font::FontWeight::Bold, false)
            } else if value.eq_ignore_ascii_case("italic") {
                (font::FontWeight::Regular, true)
            } else if value.eq_ignore_ascii_case("bolditalic")
                || value.eq_ignore_ascii_case("bold-italic")
            {
                (font::FontWeight::Bold, true)
            } else {
                self.print("FONT STYLE: REGULAR | BOLD | ITALIC | BOLDITALIC\n", RED);
                return;
            };
            self.font_style.weight = weight;
            self.font_style.italic = italic;
        } else {
            self.print("UNKNOWN FONT SETTING\n", RED);
            return;
        }
        self.log_font_style();
        self.print("FONT UPDATED: ", GREEN);
        self.print_font_info();
    }

    fn log_font_style(&self) {
        serial::put_str("[font] terminal family=");
        serial::put_str(match self.font_style.family {
            font::FontFamily::Console => "console",
            font::FontFamily::Sans => "sans",
        });
        serial::put_str(" size=");
        serial::put_u32(u32::from(self.font_style.size));
        serial::put_str(" style=");
        serial::put_str(match (self.font_style.weight, self.font_style.italic) {
            (font::FontWeight::Regular, false) => "regular",
            (font::FontWeight::Bold, false) => "bold",
            (font::FontWeight::Regular, true) => "italic",
            (font::FontWeight::Bold, true) => "bolditalic",
        });
        serial::put_str("\n");
    }

    fn print_font_info(&mut self) {
        self.print(
            match self.font_style.family {
                font::FontFamily::Console => "CONSOLE ",
                font::FontFamily::Sans => "SANS ",
            },
            CYAN,
        );
        self.print_number(u64::from(self.font_style.size));
        self.print("PX ", WHITE);
        self.print(
            match (self.font_style.weight, self.font_style.italic) {
                (font::FontWeight::Regular, false) => "REGULAR\n",
                (font::FontWeight::Bold, false) => "BOLD\n",
                (font::FontWeight::Regular, true) => "ITALIC\n",
                (font::FontWeight::Bold, true) => "BOLDITALIC\n",
            },
            WHITE,
        );
    }

    pub fn report_display_info(
        &mut self,
        driver: &str,
        width: u32,
        height: u32,
        width_mm: u16,
        height_mm: u16,
        color: ColorMode,
    ) {
        self.print("DISPLAY DRIVER: ", GREEN);
        self.print(driver, WHITE);
        self.print("\nPHYSICAL MODE: ", GREEN);
        self.print_number(u64::from(width));
        self.print("x", WHITE);
        self.print_number(u64::from(height));
        self.print("x32 (24 COLOR BITS)\nRENDER COLOR: ", WHITE);
        self.print_color_mode(color);
        if width_mm != 0 && height_mm != 0 {
            self.print("\nMONITOR SIZE: ", GREEN);
            self.print_number(u64::from(width_mm));
            self.print("x", WHITE);
            self.print_number(u64::from(height_mm));
            self.print(" MM", WHITE);
        }
        self.print("\nMODE SWITCH: NATIVE DISPLAY DRIVER API\n", MUTED);
        self.prompt();
    }

    pub fn report_display_modes(&mut self, modes: &[DisplayMode]) {
        self.print("AVAILABLE DISPLAY MODES:\n", GREEN);
        for mode in modes {
            self.print("  ", WHITE);
            self.print_number(u64::from(mode.width));
            self.print("x", WHITE);
            self.print_number(u64::from(mode.height));
            if mode.refresh_millihertz != 0 {
                self.print(" @ ", MUTED);
                self.print_number(u64::from(mode.refresh_millihertz / 1000));
                self.print(" HZ", MUTED);
            }
            self.newline();
        }
        self.prompt();
    }

    pub fn report_display_mode(
        &mut self,
        width: u32,
        height: u32,
        result: Result<DisplayMode, ModeSetError>,
    ) {
        match result {
            Ok(_) => self.print("DISPLAY MODE ACTIVE: ", GREEN),
            Err(ModeSetError::RequiresReboot) => {
                self.print("FIRMWARE FRAMEBUFFER REQUIRES GRUB RESTART: ", YELLOW)
            }
            Err(ModeSetError::UnsupportedMode) => self.print("UNSUPPORTED DISPLAY MODE: ", RED),
            Err(ModeSetError::OutOfMemory) => self.print("NOT ENOUGH RAM FOR DISPLAY MODE: ", RED),
            Err(ModeSetError::DeviceLost) => self.print("DISPLAY DEVICE LOST: ", RED),
        }
        self.print_number(u64::from(width));
        self.print("x", WHITE);
        self.print_number(u64::from(height));
        if matches!(result, Err(ModeSetError::RequiresReboot)) {
            self.print("\nSELECT THE MODE IN GRUB AND RESTART.\n", MUTED);
        } else {
            self.newline();
        }
        self.prompt();
    }

    pub fn report_color_mode(&mut self, mode: ColorMode) {
        self.print("SOFTWARE COLOR MODE: ", GREEN);
        self.print_color_mode(mode);
        self.print(" (PHYSICAL SCANOUT REMAINS ALIGNED XRGB8888)\n", MUTED);
        self.prompt();
    }

    /// Печатает фактически применённый профиль input service.
    pub fn report_mouse(
        &mut self,
        settings: MouseSettings,
        capabilities: MouseCapabilities,
        hardware_applied: Option<bool>,
    ) {
        self.print("MOUSE PROFILE: RATE=", GREEN);
        self.print_number(u64::from(settings.sample_rate_hz));
        self.print("HZ RESOLUTION=", WHITE);
        self.print_number(u64::from(settings.resolution_level));
        self.print(" SENSITIVITY=", WHITE);
        self.print_number(u64::from(settings.sensitivity_percent));
        self.print("% ACCELERATION=", WHITE);
        self.print_number(u64::from(settings.acceleration_percent));
        self.print("%\nDOUBLE=", WHITE);
        self.print_number(u64::from(settings.double_click_ms));
        self.print("MS DEBOUNCE=", WHITE);
        self.print_number(u64::from(settings.click_debounce_ms));
        self.print("MS DRAG=", WHITE);
        self.print_number(u64::from(settings.drag_threshold_px));
        self.print("PX\nHARDWARE RATE/RESOLUTION: ", WHITE);
        self.print(
            if capabilities.configurable_sample_rate != 0
                && capabilities.configurable_resolution != 0
            {
                "SUPPORTED"
            } else {
                "SOFTWARE FALLBACK"
            },
            if capabilities.configurable_sample_rate != 0 {
                GREEN
            } else {
                YELLOW
            },
        );
        if let Some(applied) = hardware_applied {
            self.print(
                if applied {
                    " / APPLIED\n"
                } else {
                    " / DEVICE DID NOT ACK\n"
                },
                if applied { GREEN } else { YELLOW },
            );
        } else {
            self.newline();
        }
        self.prompt();
    }

    /// Подтверждает выбор визуального ресурса.
    pub fn report_visual_setting(&mut self, category: &str, value: &str) {
        self.print(category, GREEN);
        self.print(": ", GREEN);
        self.print(value, WHITE);
        self.newline();
        self.prompt();
    }

    fn print_color_mode(&mut self, mode: ColorMode) {
        self.print(
            match mode {
                ColorMode::TrueColor24 => "TRUECOLOR/24-BIT",
                ColorMode::HighColor16 => "RGB565/16-BIT",
                ColorMode::Grayscale8 => "GRAYSCALE/8-BIT",
            },
            WHITE,
        );
    }

    fn command_run(&mut self, command: &str) {
        let mut output = [0u8; 4096];
        match crate::process::run_interactive_command(command, &mut output) {
            Ok(result) => {
                if result.output_length != 0 {
                    match core::str::from_utf8(&output[..result.output_length]) {
                        Ok(text) => self.print(text, WHITE),
                        Err(_) => self.print("[PROGRAM OUTPUT IS NOT UTF-8]\n", RED),
                    }
                }
                self.print("[EXIT status=", MUTED);
                if result.status < 0 {
                    self.print("-", RED);
                    self.print_number(u64::from(result.status.unsigned_abs()));
                } else {
                    self.print_number(result.status as u64);
                }
                if result.exception != 0 {
                    self.print(" exception=", RED);
                    self.print_number(u64::from(result.exception));
                }
                self.print("]\n", MUTED);
            }
            Err(_) => self.print("RUN FAILED: USE AN ABSOLUTE VARANIAFS PATH\n", RED),
        }
    }

    fn prompt(&mut self) {
        self.print("RUSTOS:", CYAN);
        let mut cwd = [0u8; PATH_CAPACITY];
        let cwd_len = self.cwd_len;
        cwd[..cwd_len].copy_from_slice(&self.cwd[..cwd_len]);
        self.print(core::str::from_utf8(&cwd[..cwd_len]).unwrap_or("/"), CYAN);
        self.print(" > ", CYAN);
    }

    fn execute_fs_command(&mut self, command: &str) -> bool {
        let command = if command.eq_ignore_ascii_case("fs") {
            ""
        } else if command.len() > 3 && command[..3].eq_ignore_ascii_case("fs ") {
            command[3..].trim()
        } else {
            command
        };
        let (verb, arguments) = command.split_once(' ').unwrap_or((command, ""));
        let arguments = arguments.trim();

        if verb.is_empty() {
            self.print_fs_help();
            return true;
        }
        if verb.eq_ignore_ascii_case("pwd") {
            self.print_cwd();
            // Lifecycle-тест GUI проверяет не текстовые пиксели, а состояние
            // нового shell instance. После закрытия окна новый терминал
            // обязан начать с `/`, даже если предыдущий находился в `/src`.
            let mut path = [0u8; PATH_CAPACITY];
            path[..self.cwd_len].copy_from_slice(&self.cwd[..self.cwd_len]);
            self.log_fs(
                "PWD",
                core::str::from_utf8(&path[..self.cwd_len]).unwrap_or("?"),
                0,
            );
            return true;
        }
        if verb.eq_ignore_ascii_case("ls") || verb.eq_ignore_ascii_case("dir") {
            self.command_ls(arguments);
            return true;
        }
        if verb.eq_ignore_ascii_case("cat") || verb.eq_ignore_ascii_case("read") {
            self.command_cat(arguments);
            return true;
        }
        if verb.eq_ignore_ascii_case("cd") {
            self.command_cd(arguments);
            return true;
        }
        if verb.eq_ignore_ascii_case("mkdir") {
            self.command_single_path("MKDIR", arguments, |fs, path| fs.make_dir(path));
            return true;
        }
        if verb.eq_ignore_ascii_case("touch") {
            self.command_single_path("TOUCH", arguments, |fs, path| fs.touch(path));
            return true;
        }
        if verb.eq_ignore_ascii_case("rm") {
            self.command_single_path("REMOVE", arguments, |fs, path| fs.remove(path));
            return true;
        }
        if verb.eq_ignore_ascii_case("write") {
            self.command_write(arguments, false);
            return true;
        }
        if verb.eq_ignore_ascii_case("append") {
            self.command_write(arguments, true);
            return true;
        }
        if verb.eq_ignore_ascii_case("stat") {
            self.command_stat(arguments);
            return true;
        }
        false
    }

    fn print_fs_help(&mut self) {
        self.print("FILESYSTEM COMMANDS (RAM WRITES ARE VOLATILE):\n", YELLOW);
        self.print("  LS [PATH]             LIST DIRECTORY\n", WHITE);
        self.print("  CAT PATH              READ TEXT FILE\n", WHITE);
        self.print("  CD PATH / PWD         CHANGE/SHOW CWD\n", WHITE);
        self.print("  MKDIR PATH            CREATE DIRECTORY\n", WHITE);
        self.print("  TOUCH PATH            CREATE EMPTY FILE\n", WHITE);
        self.print("  WRITE PATH TEXT       REPLACE FILE\n", WHITE);
        self.print("  APPEND PATH TEXT      APPEND FILE\n", WHITE);
        self.print("  RM PATH / STAT PATH   REMOVE/INSPECT\n", WHITE);
        self.print("INITRAMFS IS MOUNTED READ-ONLY AT /BOOT.\n", MUTED);
    }

    fn print_cwd(&mut self) {
        let mut cwd = [0u8; PATH_CAPACITY];
        let len = self.cwd_len;
        cwd[..len].copy_from_slice(&self.cwd[..len]);
        self.print(core::str::from_utf8(&cwd[..len]).unwrap_or("/"), WHITE);
        self.newline();
    }

    fn command_ls(&mut self, input: &str) {
        let mut path_buffer = [0u8; PATH_CAPACITY];
        let path =
            match self.resolve_path(if input.is_empty() { "." } else { input }, &mut path_buffer) {
                Ok(path) => path,
                Err(error) => {
                    self.print_fs_error("LS", input, error);
                    return;
                }
            };
        match self.fs.list(path) {
            Ok(listing) => {
                self.log_fs("LIST", path, listing.entries().len());
                if listing.entries().is_empty() {
                    self.print("<EMPTY>\n", MUTED);
                }
                for entry in listing.entries() {
                    self.print(
                        if entry.kind() == FileKind::Directory {
                            "[DIR]  "
                        } else {
                            "[FILE] "
                        },
                        if entry.kind() == FileKind::Directory {
                            CYAN
                        } else {
                            WHITE
                        },
                    );
                    self.print(entry.name(), WHITE);
                    if entry.is_read_only() {
                        self.print("  RO", MUTED);
                    }
                    self.newline();
                }
            }
            Err(error) => self.print_fs_error("LS", path, error),
        }
    }

    fn command_cat(&mut self, input: &str) {
        if input.is_empty() {
            self.print("USAGE: CAT PATH\n", YELLOW);
            return;
        }
        let mut path_buffer = [0u8; PATH_CAPACITY];
        let path = match self.resolve_path(input, &mut path_buffer) {
            Ok(path) => path,
            Err(error) => {
                self.print_fs_error("READ", input, error);
                return;
            }
        };
        let mut data = [0u8; FILE_CAPACITY];
        match self.fs.read(path, &mut data) {
            Ok(len) => {
                self.log_fs("READ", path, len);
                self.print_file_bytes(&data[..len]);
            }
            Err(error) => self.print_fs_error("READ", path, error),
        }
    }

    fn command_cd(&mut self, input: &str) {
        let input = if input.is_empty() { "/" } else { input };
        let mut path_buffer = [0u8; PATH_CAPACITY];
        let path = match self.resolve_path(input, &mut path_buffer) {
            Ok(path) => path,
            Err(error) => {
                self.print_fs_error("CD", input, error);
                return;
            }
        };
        match self.fs.stat(path) {
            Ok(stat) if stat.kind == FileKind::Directory => {
                self.cwd.fill(0);
                self.cwd[..path.len()].copy_from_slice(path.as_bytes());
                self.cwd_len = path.len();
                self.log_fs("CHDIR", path, 0);
            }
            Ok(_) => self.print_fs_error("CD", path, FsError::NotDirectory),
            Err(error) => self.print_fs_error("CD", path, error),
        }
    }

    fn command_single_path<F>(&mut self, operation: &str, input: &str, action: F)
    where
        F: FnOnce(&mut BootstrapFs, &str) -> Result<(), FsError>,
    {
        if input.is_empty() {
            self.print("PATH REQUIRED\n", YELLOW);
            return;
        }
        let mut path_buffer = [0u8; PATH_CAPACITY];
        let path = match self.resolve_path(input, &mut path_buffer) {
            Ok(path) => path,
            Err(error) => {
                self.print_fs_error(operation, input, error);
                return;
            }
        };
        match action(&mut self.fs, path) {
            Ok(()) => {
                self.log_fs(operation, path, 0);
                self.print("OK\n", GREEN);
            }
            Err(error) => self.print_fs_error(operation, path, error),
        }
    }

    fn command_write(&mut self, arguments: &str, append: bool) {
        let Some((path_input, data)) = arguments.split_once(' ') else {
            self.print(
                if append {
                    "USAGE: APPEND PATH TEXT\n"
                } else {
                    "USAGE: WRITE PATH TEXT\n"
                },
                YELLOW,
            );
            return;
        };
        let mut path_buffer = [0u8; PATH_CAPACITY];
        let path = match self.resolve_path(path_input, &mut path_buffer) {
            Ok(path) => path,
            Err(error) => {
                self.print_fs_error(if append { "APPEND" } else { "WRITE" }, path_input, error);
                return;
            }
        };
        let result = if append {
            self.fs.append(path, data.as_bytes())
        } else {
            self.fs.write(path, data.as_bytes())
        };
        match result {
            Ok(()) => {
                self.log_fs(if append { "APPEND" } else { "WRITE" }, path, data.len());
                self.print("OK: ", GREEN);
                self.print_number(data.len() as u64);
                self.print(" BYTES\n", GREEN);
            }
            Err(error) => self.print_fs_error(if append { "APPEND" } else { "WRITE" }, path, error),
        }
    }

    fn command_stat(&mut self, input: &str) {
        if input.is_empty() {
            self.print("USAGE: STAT PATH\n", YELLOW);
            return;
        }
        let mut path_buffer = [0u8; PATH_CAPACITY];
        let path = match self.resolve_path(input, &mut path_buffer) {
            Ok(path) => path,
            Err(error) => {
                self.print_fs_error("STAT", input, error);
                return;
            }
        };
        match self.fs.stat(path) {
            Ok(stat) => {
                self.log_fs("STAT", path, stat.size);
                self.print("PATH: ", CYAN);
                self.print(path, WHITE);
                self.print("\nTYPE: ", CYAN);
                self.print(
                    if stat.kind == FileKind::Directory {
                        "DIRECTORY"
                    } else {
                        "FILE"
                    },
                    WHITE,
                );
                self.print("\nSIZE: ", CYAN);
                self.print_number(stat.size as u64);
                self.print(" BYTES\nMODE: ", WHITE);
                self.print(
                    if stat.read_only {
                        "READ-ONLY\n"
                    } else {
                        "READ-WRITE\n"
                    },
                    WHITE,
                );
            }
            Err(error) => self.print_fs_error("STAT", path, error),
        }
    }

    /// Разрешает относительный путь пользователя относительно cwd через
    /// [`BootstrapFs::normalize`], возвращая заимствованную `&str` в
    /// выданном буфере (без аллокаций — ядро без heap).
    fn resolve_path<'a>(
        &self,
        input: &str,
        output: &'a mut [u8; PATH_CAPACITY],
    ) -> Result<&'a str, FsError> {
        let cwd = core::str::from_utf8(&self.cwd[..self.cwd_len]).unwrap_or("/");
        let len = self.fs.normalize(cwd, input, output)?;
        core::str::from_utf8(&output[..len]).map_err(|_| FsError::InvalidPath)
    }

    fn print_file_bytes(&mut self, bytes: &[u8]) {
        if let Ok(text) = core::str::from_utf8(bytes) {
            self.print(text, WHITE);
            if !text.ends_with('\n') {
                self.newline();
            }
            return;
        }
        for byte in bytes.iter().copied() {
            match byte {
                b'\n' => self.newline(),
                b'\t' => {
                    for _ in 0..4 {
                        self.put(' ', WHITE);
                    }
                }
                byte if byte.is_ascii_graphic() || byte == b' ' => {
                    self.put(char::from(byte), WHITE)
                }
                _ => self.put('�', MUTED),
            }
        }
        if !bytes.ends_with(b"\n") {
            self.newline();
        }
    }

    fn print_fs_error(&mut self, operation: &str, path: &str, error: FsError) {
        self.print(operation, RED);
        self.print(" ", RED);
        self.print(path, WHITE);
        self.print(": ", RED);
        self.print(error.message(), RED);
        self.newline();
    }

    fn log_fs(&self, operation: &str, path: &str, value: usize) {
        serial::put_str("[vfs] ");
        serial::put_str(operation);
        serial::put_str(" path=");
        serial::put_str(path);
        serial::put_str(" value=");
        serial::put_u32(value as u32);
        serial::put_str("\n");
    }

    fn clear(&mut self) {
        self.cells.fill(EMPTY_CELL);
        self.column = 0;
        self.row = 0;
    }

    fn print_number(&mut self, mut number: u64) {
        if number == 0 {
            self.put('0', GREEN);
            return;
        }
        let mut digits = [0u8; 20];
        let mut count = 0;
        while number > 0 {
            digits[count] = b'0' + (number % 10) as u8;
            number /= 10;
            count += 1;
        }
        while count > 0 {
            count -= 1;
            self.put(char::from(digits[count]), GREEN);
        }
    }

    fn print(&mut self, text: &str, color: Color) {
        for character in text.chars() {
            if character == '\n' {
                self.newline();
            } else {
                self.put(character, color);
            }
        }
    }

    fn put(&mut self, character: char, color: Color) {
        if self.column >= COLS {
            self.newline();
        }
        self.cells[self.row * COLS + self.column] = Cell {
            character: u16::try_from(u32::from(character)).unwrap_or('�' as u16),
            color: color_index(color),
        };
        self.column += 1;
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            self.column -= 1;
            self.cells[self.row * COLS + self.column] = EMPTY_CELL;
        }
    }

    fn newline(&mut self) {
        self.column = 0;
        if self.row + 1 < ROWS {
            self.row += 1;
            return;
        }
        for row in 1..ROWS {
            for column in 0..COLS {
                self.cells[(row - 1) * COLS + column] = self.cells[row * COLS + column];
            }
        }
        for column in 0..COLS {
            self.cells[(ROWS - 1) * COLS + column] = EMPTY_CELL;
        }
    }
}

fn strip_prefix_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let candidate = value.get(..prefix.len())?;
    candidate
        .eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

fn parse_resolution(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once(['x', 'X'])?;
    let width = width.parse::<u32>().ok()?;
    let height = height.parse::<u32>().ok()?;
    (width >= 640 && height >= 480).then_some((width, height))
}

fn parse_cursor(value: &str) -> Option<PointerCursor> {
    if value.eq_ignore_ascii_case("arrow") {
        Some(PointerCursor::Arrow)
    } else if value.eq_ignore_ascii_case("text") || value.eq_ignore_ascii_case("ibeam") {
        Some(PointerCursor::Text)
    } else if value.eq_ignore_ascii_case("link") || value.eq_ignore_ascii_case("hand") {
        Some(PointerCursor::Link)
    } else if value.eq_ignore_ascii_case("grab") {
        Some(PointerCursor::Grab)
    } else if value.eq_ignore_ascii_case("grabbing") {
        Some(PointerCursor::Grabbing)
    } else if value.eq_ignore_ascii_case("busy") || value.eq_ignore_ascii_case("loader") {
        Some(PointerCursor::Busy)
    } else if value.eq_ignore_ascii_case("crosshair") {
        Some(PointerCursor::Crosshair)
    } else if value.eq_ignore_ascii_case("forbidden") {
        Some(PointerCursor::NotAllowed)
    } else if value.eq_ignore_ascii_case("hresize") {
        Some(PointerCursor::ResizeHorizontal)
    } else if value.eq_ignore_ascii_case("vresize") {
        Some(PointerCursor::ResizeVertical)
    } else if value.eq_ignore_ascii_case("nwse") {
        Some(PointerCursor::ResizeNwSe)
    } else if value.eq_ignore_ascii_case("nesw") {
        Some(PointerCursor::ResizeNeSw)
    } else {
        None
    }
}

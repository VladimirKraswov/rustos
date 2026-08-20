//! Первый GUI-клиент RustOS: цветной системный terminal.
//!
//! До появления ring 3 terminal содержит встроенный shell и вызывает
//! bootstrap VFS напрямую. Команды и VFS-семантика уже совпадают с будущими
//! `shell` + `fs` процессами; позже прямой вызов заменит `vfs.dll`/IPC.

use crate::{
    arch, font,
    fs::{BootstrapFs, FileKind, FsError, FILE_CAPACITY, PATH_CAPACITY},
    graphics::{Color, Framebuffer, Rect},
    input::Key,
    serial,
};
use rustos_abi::bootinfo::BootInitramfs;

/// Размер буфера терминала: подогнан под дефолтное окно desktop
/// (шире/выше окна буфер просто не умещается целиком).
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

/// Клетка экрана терминала: ASCII-символ + цвет. Отдельные глифы не
/// хранятся — отрисовка идёт из `font` по коду символа.
#[derive(Clone, Copy)]
struct Cell {
    character: u8,
    color: Color,
}

const EMPTY_CELL: Cell = Cell {
    character: b' ',
    color: WHITE,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalAction {
    /// Состояние экрана не изменилось.
    None,
    /// Изменилась только текущая строка ввода.
    RedrawInputLine,
    /// Команда изменила несколько строк или прокрутила терминал.
    RedrawAll,
    Shutdown,
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
            fs: BootstrapFs::new(initramfs),
            cwd,
            cwd_len: 1,
        };
        terminal.print("RUSTOS GRAPHICAL TERMINAL 0.1\n", CYAN);
        terminal.print("MICROKERNEL DEVELOPMENT SESSION\n", MUTED);
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
                    self.put(byte, WHITE);
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
                        self.put(b' ', WHITE);
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
        let visible_cols =
            ((rect.width.saturating_sub(12)) / font::GLYPH_WIDTH as u32).min(COLS as u32) as usize;
        let visible_rows = ((rect.height.saturating_sub(12)) / font::GLYPH_HEIGHT as u32)
            .min(ROWS as u32) as usize;
        let first_row = self.row.saturating_add(1).saturating_sub(visible_rows);
        for screen_row in 0..visible_rows {
            let source_row = first_row + screen_row;
            if source_row >= ROWS {
                break;
            }
            for column in 0..visible_cols {
                let cell = self.cells[source_row * COLS + column];
                if cell.character != b' ' {
                    font::draw_char(
                        fb,
                        rect.x + 6 + column as i32 * font::GLYPH_WIDTH,
                        rect.y + 6 + screen_row as i32 * font::GLYPH_HEIGHT,
                        cell.character,
                        cell.color,
                        1,
                    );
                }
            }
        }

        // Тонкий caret в активной позиции.
        if self.row >= first_row && self.row < first_row + visible_rows {
            let caret_x = rect.x + 6 + self.column as i32 * font::GLYPH_WIDTH;
            let caret_y = rect.y + 6 + (self.row - first_row) as i32 * font::GLYPH_HEIGHT + 7;
            fb.fill_rect(Rect::new(caret_x, caret_y, 5, 1), CYAN);
        }
    }

    /// Перерисовывает только видимую строку с caret и возвращает её dirty
    /// rectangle. Это критично для раннего polling input: полный software
    /// redraw на каждую букву надолго оставлял виртуальный 8042 без чтения.
    pub fn draw_input_line(&self, fb: &mut Framebuffer, rect: Rect) -> Option<Rect> {
        let visible_cols =
            ((rect.width.saturating_sub(12)) / font::GLYPH_WIDTH as u32).min(COLS as u32) as usize;
        let visible_rows = ((rect.height.saturating_sub(12)) / font::GLYPH_HEIGHT as u32)
            .min(ROWS as u32) as usize;
        let first_row = self.row.saturating_add(1).saturating_sub(visible_rows);
        if self.row < first_row || self.row >= first_row + visible_rows {
            return None;
        }

        let screen_row = self.row - first_row;
        let line = Rect::new(
            rect.x,
            rect.y + 6 + screen_row as i32 * font::GLYPH_HEIGHT,
            rect.width,
            font::GLYPH_HEIGHT as u32,
        );
        fb.fill_rect(line, BACKGROUND);
        for column in 0..visible_cols {
            let cell = self.cells[self.row * COLS + column];
            if cell.character != b' ' {
                font::draw_char(
                    fb,
                    rect.x + 6 + column as i32 * font::GLYPH_WIDTH,
                    line.y,
                    cell.character,
                    cell.color,
                    1,
                );
            }
        }
        let caret_x = rect.x + 6 + self.column as i32 * font::GLYPH_WIDTH;
        fb.fill_rect(Rect::new(caret_x, line.y + 7, 5, 1), CYAN);
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
            self.print("  ECHO TEXT PRINT TEXT\n", WHITE);
            self.print("  PWD/CD    CURRENT DIRECTORY\n", WHITE);
            self.print("  LS/CAT    LIST OR READ FILES\n", WHITE);
            self.print("  MKDIR     CREATE DIRECTORY\n", WHITE);
            self.print("  WRITE     REPLACE RAM FILE\n", WHITE);
            self.print("  APPEND    APPEND RAM FILE\n", WHITE);
            self.print("  TOUCH/RM  CREATE OR REMOVE\n", WHITE);
            self.print("  STAT      FILE INFORMATION\n", WHITE);
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
            self.print("RUST, UEFI GOP, CPU SOFTWARE COMPOSITOR\n", WHITE);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("mem") {
            self.print("USABLE RAM: ", GREEN);
            self.print_number(self.usable_ram_mib);
            self.print(" MIB\n", GREEN);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("gui") {
            self.print("DISPLAYD: ONLINE\n", GREEN);
            self.print("COMPOSITOR: SOFTWARE / GOP 32-BIT\n", WHITE);
            self.print("WINDOW MANAGER: ONLINE\n", GREEN);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("shutdown") {
            self.print("POWERING OFF...\n", YELLOW);
            TerminalAction::Shutdown
        } else if command.len() >= 5 && command[..5].eq_ignore_ascii_case("echo ") {
            self.print(&command[5..], WHITE);
            self.newline();
            TerminalAction::None
        } else if self.execute_fs_command(command) {
            TerminalAction::None
        } else {
            self.print("COMMAND NOT FOUND: ", RED);
            self.print(command, WHITE);
            self.newline();
            TerminalAction::None
        };
        if action != TerminalAction::Shutdown {
            self.prompt();
        }
        if action == TerminalAction::Shutdown {
            TerminalAction::Shutdown
        } else {
            TerminalAction::RedrawAll
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
        for byte in bytes.iter().copied() {
            match byte {
                b'\n' => self.newline(),
                b'\t' => {
                    for _ in 0..4 {
                        self.put(b' ', WHITE);
                    }
                }
                byte if byte.is_ascii_graphic() || byte == b' ' => self.put(byte, WHITE),
                _ => self.put(b'.', MUTED),
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
            self.put(b'0', GREEN);
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
            self.put(digits[count], GREEN);
        }
    }

    fn print(&mut self, text: &str, color: Color) {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.newline();
            } else {
                self.put(byte, color);
            }
        }
    }

    fn put(&mut self, byte: u8, color: Color) {
        if self.column >= COLS {
            self.newline();
        }
        self.cells[self.row * COLS + self.column] = Cell {
            character: byte,
            color,
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

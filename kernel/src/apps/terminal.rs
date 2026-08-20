//! Первый GUI-клиент RustOS: цветной системный terminal.
//!
//! Пока VFS и user-space runtime ещё не подключены, terminal содержит
//! минимальную встроенную command loop. Его интерфейс отделён от window
//! manager, поэтому позже команды будут пересылаться shell-процессу по IPC.

use crate::{
    font,
    graphics::{Color, Framebuffer, Rect},
    input::Key,
    serial,
};

const COLS: usize = 118;
const ROWS: usize = 52;
const INPUT_CAPACITY: usize = 96;

const WHITE: Color = Color::rgb(225, 234, 242);
const MUTED: Color = Color::rgb(139, 158, 178);
const CYAN: Color = Color::rgb(83, 207, 222);
const GREEN: Color = Color::rgb(116, 216, 153);
const YELLOW: Color = Color::rgb(238, 198, 108);
const RED: Color = Color::rgb(239, 110, 118);

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
    None,
    Shutdown,
}

pub struct Terminal {
    cells: [Cell; COLS * ROWS],
    column: usize,
    row: usize,
    input: [u8; INPUT_CAPACITY],
    input_len: usize,
    usable_ram_mib: u64,
}

impl Terminal {
    pub fn new(usable_ram_mib: u64) -> Self {
        let mut terminal = Self {
            cells: [EMPTY_CELL; COLS * ROWS],
            column: 0,
            row: 0,
            input: [0; INPUT_CAPACITY],
            input_len: 0,
            usable_ram_mib,
        };
        terminal.print("RUSTOS GRAPHICAL TERMINAL 0.1\n", CYAN);
        terminal.print("MICROKERNEL DEVELOPMENT SESSION\n", MUTED);
        terminal.print("TYPE HELP TO SEE AVAILABLE COMMANDS.\n\n", WHITE);
        terminal.prompt();
        terminal
    }

    pub fn handle_key(&mut self, key: Key) -> TerminalAction {
        match key {
            Key::Character(byte) if byte.is_ascii_graphic() || byte == b' ' => {
                if self.input_len < INPUT_CAPACITY {
                    self.input[self.input_len] = byte;
                    self.input_len += 1;
                    self.put(byte, WHITE);
                }
            }
            Key::Backspace => {
                if self.input_len > 0 {
                    self.input_len -= 1;
                    self.input[self.input_len] = 0;
                    self.backspace();
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
            }
            Key::Escape | Key::Character(_) => {}
        }
        TerminalAction::None
    }

    pub fn draw(&self, fb: &mut Framebuffer, rect: Rect) {
        fb.fill_rect(rect, Color::rgb(7, 12, 20));
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
            self.print("  SHUTDOWN  POWER OFF VM\n", WHITE);
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("clear") {
            self.clear();
            TerminalAction::None
        } else if command.eq_ignore_ascii_case("about") {
            self.print("RUSTOS 0.1.0 X86-64\n", CYAN);
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
        } else {
            self.print("COMMAND NOT FOUND: ", RED);
            self.print(command, WHITE);
            self.newline();
            TerminalAction::None
        };
        if action == TerminalAction::None {
            self.prompt();
        }
        action
    }

    fn prompt(&mut self) {
        self.print("RUSTOS:/ > ", CYAN);
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

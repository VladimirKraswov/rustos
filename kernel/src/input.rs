//! Ранний PS/2 input service в polling-режиме.
//!
//! Драйвер не привязан к GUI: он выдаёт нормализованные события клавиатуры
//! и мыши. После появления scheduler тот же интерфейс будет обслуживаться
//! отдельным user-space процессом и IRQ notifications.

use crate::arch;

/// Порт данных PS/2: общие для клавиатуры и мыши.
const DATA: u16 = 0x60;
/// Порт статуса/команд контроллера PS/2 (бит 0 — данные клавиатуры,
/// бит 1 — очередь ввода пуста, бит 5 — данные мыши).
const STATUS_COMMAND: u16 = 0x64;

/// Нормализованное нажатие клавиши (только US-раскладка, без повторений).
#[derive(Clone, Copy, Debug)]
pub enum Key {
    Character(u8),
    Enter,
    Backspace,
    Tab,
    Escape,
}

/// Событие мыши: относительный сдвиг за один пакет (Y — вниз, в
/// GUI-конвенции) + состояние кнопок. Абсолютных координат нет — GUI
/// сам интегрирует сдвиги в позицию курсора.
#[derive(Clone, Copy, Debug)]
pub struct MouseEvent {
    pub dx: i16,
    pub dy: i16,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
}

/// Нормализованное событие ввода, которое понимает GUI-сеанс.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Key(Key),
    Mouse(MouseEvent),
}

/// State-машина PS/2 контроллера: декодирует scancodes клавиатуры и
/// 3-байтные пакеты мыши из общего потока байтов. Работает только в
/// polling-режиме (см. модуль).
pub struct Ps2Input {
    shift: bool,
    caps_lock: bool,
    extended: bool,
    mouse_packet: [u8; 3],
    mouse_index: usize,
}

impl Ps2Input {
    /// Создаёт сервис и инициализирует PS/2-мышь (enable, defaults,
    /// включение reporting). Ошибки инициализации игнорируются: клавиатура
    /// продолжает работать, а мышь может появиться позже.
    pub fn new() -> Self {
        let mut input = Self {
            shift: false,
            caps_lock: false,
            extended: false,
            mouse_packet: [0; 3],
            mouse_index: 0,
        };
        input.initialize_mouse();
        input
    }

    /// Возвращает не более одного высокоуровневого события за вызов.
    pub fn poll(&mut self) -> Option<Event> {
        let status = unsafe { arch::inb(STATUS_COMMAND) };
        if status & 1 == 0 {
            return None;
        }
        let byte = unsafe { arch::inb(DATA) };
        if status & (1 << 5) != 0 {
            self.feed_mouse(byte)
        } else {
            self.feed_keyboard(byte).map(Event::Key)
        }
    }

    fn initialize_mouse(&mut self) {
        // Сбрасываем старые байты firmware, иначе первый пакет часто
        // начинается с середины и курсор совершает большой рывок.
        for _ in 0..32 {
            let status = unsafe { arch::inb(STATUS_COMMAND) };
            if status & 1 == 0 {
                break;
            }
            let _ = unsafe { arch::inb(DATA) };
        }

        if !wait_input_empty() {
            return;
        }
        unsafe { arch::outb(STATUS_COMMAND, 0xA8) }; // enable auxiliary device

        // Defaults + enable data reporting. ACK читаем синхронно, пока GUI
        // ещё не начал принимать пользовательский ввод.
        let _ = mouse_command(0xF6);
        let _ = mouse_command(0xF4);
    }

    /// Обрабатывает один scancode клавиатуры (Set 1). Возвращает нажатие;
    /// отпускания и служебные коды (Shift/CapsLock/extended-префикс)
    /// глотаются — GUI-сеансу нужны только нажатия.
    fn feed_keyboard(&mut self, scancode: u8) -> Option<Key> {
        if scancode == 0xE0 {
            self.extended = true;
            return None;
        }
        let released = scancode & 0x80 != 0;
        let code = scancode & 0x7f;
        if matches!(code, 0x2A | 0x36) {
            self.shift = !released;
            return None;
        }
        if released {
            return None;
        }
        if code == 0x3A {
            self.caps_lock = !self.caps_lock;
            return None;
        }
        if self.extended {
            self.extended = false;
            return None;
        }
        match code {
            0x01 => Some(Key::Escape),
            0x0E => Some(Key::Backspace),
            0x0F => Some(Key::Tab),
            0x1C => Some(Key::Enter),
            0x39 => Some(Key::Character(b' ')),
            _ => scancode_ascii(code, self.shift, self.caps_lock).map(Key::Character),
        }
    }

    fn feed_mouse(&mut self, byte: u8) -> Option<Event> {
        // Первый байт PS/2-пакета всегда содержит установленный bit 3.
        if self.mouse_index == 0 && byte & 0x08 == 0 {
            return None;
        }
        self.mouse_packet[self.mouse_index] = byte;
        self.mouse_index += 1;
        if self.mouse_index != 3 {
            return None;
        }
        self.mouse_index = 0;
        let flags = self.mouse_packet[0];
        if flags & 0xC0 != 0 {
            return None; // overflow — пакет нельзя интерпретировать точно
        }
        Some(Event::Mouse(MouseEvent {
            dx: self.mouse_packet[1] as i8 as i16,
            // PS/2: положительный Y направлен вверх; GUI — вниз.
            dy: -(self.mouse_packet[2] as i8 as i16),
            left: flags & 1 != 0,
            right: flags & 2 != 0,
            middle: flags & 4 != 0,
        }))
    }
}

/// US QWERTY-раскладка: scancode → ASCII с учётом Shift и CapsLock.
/// Числа в match — коды Set 1; другие раскладки — вопрос к user-space shell,
/// а не к драйверу.
fn scancode_ascii(code: u8, shift: bool, caps: bool) -> Option<u8> {
    let letter = match code {
        0x1E => Some(b'a'),
        0x30 => Some(b'b'),
        0x2E => Some(b'c'),
        0x20 => Some(b'd'),
        0x12 => Some(b'e'),
        0x21 => Some(b'f'),
        0x22 => Some(b'g'),
        0x23 => Some(b'h'),
        0x17 => Some(b'i'),
        0x24 => Some(b'j'),
        0x25 => Some(b'k'),
        0x26 => Some(b'l'),
        0x32 => Some(b'm'),
        0x31 => Some(b'n'),
        0x18 => Some(b'o'),
        0x19 => Some(b'p'),
        0x10 => Some(b'q'),
        0x13 => Some(b'r'),
        0x1F => Some(b's'),
        0x14 => Some(b't'),
        0x16 => Some(b'u'),
        0x2F => Some(b'v'),
        0x11 => Some(b'w'),
        0x2D => Some(b'x'),
        0x15 => Some(b'y'),
        0x2C => Some(b'z'),
        _ => None,
    };
    if let Some(mut value) = letter {
        if shift ^ caps {
            value = value.to_ascii_uppercase();
        }
        return Some(value);
    }
    let value = match code {
        0x02 => {
            if shift {
                b'!'
            } else {
                b'1'
            }
        }
        0x03 => {
            if shift {
                b'@'
            } else {
                b'2'
            }
        }
        0x04 => {
            if shift {
                b'#'
            } else {
                b'3'
            }
        }
        0x05 => {
            if shift {
                b'$'
            } else {
                b'4'
            }
        }
        0x06 => {
            if shift {
                b'%'
            } else {
                b'5'
            }
        }
        0x07 => {
            if shift {
                b'^'
            } else {
                b'6'
            }
        }
        0x08 => {
            if shift {
                b'&'
            } else {
                b'7'
            }
        }
        0x09 => {
            if shift {
                b'*'
            } else {
                b'8'
            }
        }
        0x0A => {
            if shift {
                b'('
            } else {
                b'9'
            }
        }
        0x0B => {
            if shift {
                b')'
            } else {
                b'0'
            }
        }
        0x0C => {
            if shift {
                b'_'
            } else {
                b'-'
            }
        }
        0x0D => {
            if shift {
                b'+'
            } else {
                b'='
            }
        }
        0x1A => {
            if shift {
                b'{'
            } else {
                b'['
            }
        }
        0x1B => {
            if shift {
                b'}'
            } else {
                b']'
            }
        }
        0x27 => {
            if shift {
                b':'
            } else {
                b';'
            }
        }
        0x28 => {
            if shift {
                b'"'
            } else {
                b'\''
            }
        }
        0x29 => {
            if shift {
                b'~'
            } else {
                b'`'
            }
        }
        0x2B => {
            if shift {
                b'|'
            } else {
                b'\\'
            }
        }
        0x33 => {
            if shift {
                b'<'
            } else {
                b','
            }
        }
        0x34 => {
            if shift {
                b'>'
            } else {
                b'.'
            }
        }
        0x35 => {
            if shift {
                b'?'
            } else {
                b'/'
            }
        }
        _ => return None,
    };
    Some(value)
}

/// Ждёт, пока очередь ввода контроллера опустеет (бит 1 статуса).
/// Таймаут ~100 000 итераций: зависший контроллер не должен вешать boot.
fn wait_input_empty() -> bool {
    for _ in 0..100_000 {
        if unsafe { arch::inb(STATUS_COMMAND) } & 2 == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Ждёт байт от устройства (бит 0 статуса) и возвращает его.
fn wait_output_full() -> Option<u8> {
    for _ in 0..100_000 {
        if unsafe { arch::inb(STATUS_COMMAND) } & 1 != 0 {
            return Some(unsafe { arch::inb(DATA) });
        }
        core::hint::spin_loop();
    }
    None
}

/// Отправляет команду мыши через контроллер (`0xD4` = «следующий байт —
/// для auxiliary-устройства»); успех = ACK `0xFA`.
fn mouse_command(command: u8) -> bool {
    if !wait_input_empty() {
        return false;
    }
    unsafe { arch::outb(STATUS_COMMAND, 0xD4) };
    if !wait_input_empty() {
        return false;
    }
    unsafe { arch::outb(DATA, command) };
    wait_output_full() == Some(0xFA)
}

//! Ранний PS/2 input service в polling-режиме.
//!
//! Драйвер не привязан к GUI: он выдаёт нормализованные события клавиатуры
//! и мыши. После появления scheduler тот же интерфейс будет обслуживаться
//! отдельным user-space процессом и IRQ notifications.

use crate::{
    arch,
    input::{Event, Key, MouseEvent},
};
use rustos_abi::input::{MouseCapabilities, MouseSettings};

/// Порт данных PS/2: общие для клавиатуры и мыши.
const DATA: u16 = 0x60;
/// Порт статуса/команд контроллера PS/2 (бит 0 — данные клавиатуры,
/// бит 1 — очередь ввода пуста, бит 5 — данные мыши).
const STATUS_COMMAND: u16 = 0x64;

/// State-машина PS/2 контроллера: декодирует scancodes клавиатуры и
/// 3- или 4-байтные пакеты мыши из общего потока байтов. Работает только в
/// polling-режиме (см. модуль).
pub struct Ps2Input {
    shift: bool,
    caps_lock: bool,
    extended: bool,
    mouse_packet: [u8; 4],
    mouse_index: usize,
    mouse_packet_size: usize,
    mouse_device_id: u8,
    pending: Option<Event>,
    reported_mouse_buttons: u8,
    settings: MouseSettings,
    /// Остаток fixed-point scaling: медленное движение не теряется даже при
    /// чувствительности ниже 100%.
    remainder_x: i32,
    remainder_y: i32,
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
            mouse_packet: [0; 4],
            mouse_index: 0,
            mouse_packet_size: 3,
            mouse_device_id: 0,
            pending: None,
            reported_mouse_buttons: 0,
            settings: MouseSettings::DEFAULT,
            remainder_x: 0,
            remainder_y: 0,
        };
        input.initialize_mouse();
        input
    }

    /// Текущий профиль, включая реально выбранную стандартную PS/2-частоту.
    pub const fn mouse_settings(&self) -> MouseSettings {
        self.settings
    }

    /// Возможности PS/2 backend. Чувствительность, ускорение и click timing
    /// доступны поверх любого устройства и потому не перечисляются флагами.
    pub const fn mouse_capabilities(&self) -> MouseCapabilities {
        MouseCapabilities {
            configurable_sample_rate: 1,
            configurable_resolution: 1,
            wheel: if self.mouse_packet_size == 4 { 1 } else { 0 },
            extra_buttons: if self.mouse_device_id == 4 { 1 } else { 0 },
            minimum_rate_hz: 10,
            maximum_rate_hz: 200,
            resolution_levels: 4,
            reserved: [0; 7],
        }
    }

    /// Применяет настройки. Программные поля вступают в силу всегда;
    /// `false` означает лишь, что контроллер не подтвердил hardware rate или
    /// resolution и оставил собственные значения.
    pub fn set_mouse_settings(&mut self, requested: MouseSettings) -> bool {
        self.settings = requested.sanitized();
        self.remainder_x = 0;
        self.remainder_y = 0;
        self.program_mouse_settings()
    }

    /// Возвращает не более одного высокоуровневого события за вызов.
    ///
    /// Последовательные движения мыши с неизменными кнопками объединяются.
    /// Это критично для software compositor: пока он публикует кадр, 8042
    /// успевает накопить новые пакеты. Рисовать каждый старый пакет означало
    /// бы постоянно отставать от реального курсора. Изменения кнопок никогда
    /// не объединяются, поэтому mouse-down/up сохраняют точную семантику.
    pub fn poll(&mut self) -> Option<Event> {
        if let Some(event) = self.pending.take() {
            self.remember_buttons(event);
            return Some(event);
        }

        let mut mouse: Option<MouseEvent> = None;
        // Bounded drain не позволяет потоку мыши навсегда вытеснить остальную
        // работу GUI, но за один кадр поглощает до 32 полных PS/2-пакетов.
        for _ in 0..96 {
            let status = unsafe { arch::inb(STATUS_COMMAND) };
            if status & 1 == 0 {
                break;
            }
            let byte = unsafe { arch::inb(DATA) };
            let event = if status & (1 << 5) != 0 {
                self.feed_mouse(byte)
            } else {
                self.feed_keyboard(byte).map(Event::Key)
            };
            let Some(event) = event else {
                continue;
            };
            match event {
                Event::Mouse(next) => {
                    let next_buttons = mouse_buttons(next);
                    if mouse.is_none() && next_buttons != self.reported_mouse_buttons {
                        self.reported_mouse_buttons = next_buttons;
                        return Some(Event::Mouse(next));
                    }
                    if let Some(accumulated) = mouse.as_mut() {
                        if mouse_buttons(*accumulated) != next_buttons {
                            self.pending = Some(Event::Mouse(next));
                            break;
                        }
                        accumulated.dx = accumulated.dx.saturating_add(next.dx);
                        accumulated.dy = accumulated.dy.saturating_add(next.dy);
                        accumulated.wheel_x = accumulated.wheel_x.saturating_add(next.wheel_x);
                        accumulated.wheel_y = accumulated.wheel_y.saturating_add(next.wheel_y);
                        accumulated.packets = accumulated.packets.saturating_add(next.packets);
                    } else {
                        mouse = Some(next);
                    }
                }
                Event::Key(key) => {
                    if mouse.is_some() {
                        self.pending = Some(Event::Key(key));
                        break;
                    }
                    return Some(Event::Key(key));
                }
            }
        }
        let event = Event::Mouse(mouse?);
        self.remember_buttons(event);
        Some(event)
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

        // IntelliMouse-compatible устройства включают колёсико только после
        // стандартной последовательности частот 200/100/80. Затем F2
        // возвращает ID 3 (wheel) либо 4 (wheel + дополнительные кнопки).
        // Обычная трёхбайтовая мышь остаётся рабочим безопасным fallback.
        let _ = self.mouse_command(0xF6);
        self.detect_extended_mouse();
        let _ = self.program_mouse_settings();
    }

    fn detect_extended_mouse(&mut self) {
        let magic = self.mouse_command_with_data(0xF3, 200)
            && self.mouse_command_with_data(0xF3, 100)
            && self.mouse_command_with_data(0xF3, 80);
        if !magic || !self.mouse_command(0xF2) {
            return;
        }
        let Some(id) = self.wait_mouse_reply() else {
            return;
        };
        self.mouse_device_id = id;
        if matches!(id, 3 | 4) {
            self.mouse_packet_size = 4;
        }
    }

    fn program_mouse_settings(&mut self) -> bool {
        // Удаляем уже накопленные movement packets: первый их байт тоже имеет
        // auxiliary-флаг, но не является ACK команды F5.
        for _ in 0..96 {
            let status = unsafe { arch::inb(STATUS_COMMAND) };
            if status & 1 == 0 {
                break;
            }
            let byte = unsafe { arch::inb(DATA) };
            if status & (1 << 5) == 0 {
                self.preserve_keyboard_byte(byte);
            }
        }
        // Останавливаем поток пакетов: иначе ACK можно спутать с байтом
        // движения. Неудача не делает input service неработоспособным.
        let disabled = self.mouse_command(0xF5);
        let rate = self.mouse_command_with_data(0xF3, self.settings.sample_rate_hz as u8);
        let resolution = self.mouse_command_with_data(0xE8, self.settings.resolution_level);
        let enabled = self.mouse_command(0xF4);
        disabled && rate && resolution && enabled
    }

    fn mouse_command_with_data(&mut self, command: u8, data: u8) -> bool {
        self.mouse_command(command) && self.mouse_command(data)
    }

    /// Отправляет команду через 8042 (`D4` означает auxiliary device).
    fn mouse_command(&mut self, command: u8) -> bool {
        if !wait_input_empty() {
            return false;
        }
        unsafe { arch::outb(STATUS_COMMAND, 0xD4) };
        if !wait_input_empty() {
            return false;
        }
        unsafe { arch::outb(DATA, command) };
        self.wait_mouse_reply() == Some(0xFA)
    }

    /// Keyboard release может прийти между shell-командой и ACK мыши. Мы
    /// обязательно прогоняем его через state machine, иначе Shift способен
    /// остаться «зажатым» после команды, набранной заглавными буквами.
    fn wait_mouse_reply(&mut self) -> Option<u8> {
        for _ in 0..100_000 {
            let status = unsafe { arch::inb(STATUS_COMMAND) };
            if status & 1 != 0 {
                let byte = unsafe { arch::inb(DATA) };
                if status & (1 << 5) != 0 {
                    return Some(byte);
                }
                self.preserve_keyboard_byte(byte);
            }
            core::hint::spin_loop();
        }
        None
    }

    fn preserve_keyboard_byte(&mut self, byte: u8) {
        if let Some(key) = self.feed_keyboard(byte) {
            if self.pending.is_none() {
                self.pending = Some(Event::Key(key));
            }
        }
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
        let extended = self.extended;
        self.extended = false;
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
        if extended {
            return match code {
                0x47 => Some(Key::Home),
                0x48 => Some(Key::Up),
                0x49 => Some(Key::PageUp),
                0x4B => Some(Key::Left),
                0x4D => Some(Key::Right),
                0x4F => Some(Key::End),
                0x50 => Some(Key::Down),
                0x51 => Some(Key::PageDown),
                _ => None,
            };
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
        if self.mouse_index != self.mouse_packet_size {
            return None;
        }
        self.mouse_index = 0;
        let flags = self.mouse_packet[0];
        if flags & 0xC0 != 0 {
            return None; // overflow — пакет нельзя интерпретировать точно
        }
        let raw_x = self.mouse_packet[1] as i8 as i16;
        // PS/2: положительный Y направлен вверх; GUI — вниз.
        let raw_y = -(self.mouse_packet[2] as i8 as i16);
        let (dx, dy) = self.scale_motion(raw_x, raw_y);
        let wheel_y = if self.mouse_packet_size == 4 {
            // Z — signed 4-bit two's-complement. У PS/2 положительный Z
            // означает wheel-up, а общий UI-контракт использует плюс вниз.
            -sign_extend_nibble(self.mouse_packet[3])
        } else {
            0
        };
        Some(Event::Mouse(MouseEvent {
            dx,
            // PS/2: положительный Y направлен вверх; GUI — вниз.
            dy,
            wheel_x: 0,
            wheel_y,
            left: flags & 1 != 0,
            right: flags & 2 != 0,
            middle: flags & 4 != 0,
            packets: 1,
        }))
    }

    /// Software gain одинаков на PS/2, USB HID и virtio-input. Fixed-point
    /// остаток особенно важен при sensitivity=25%: четыре малых отчёта всё
    /// равно дают один пиксель, курсор не «залипает».
    fn scale_motion(&mut self, dx: i16, dy: i16) -> (i16, i16) {
        let speed = i32::from(dx)
            .abs()
            .saturating_add(i32::from(dy).abs())
            .min(32);
        let gain = i32::from(self.settings.sensitivity_percent)
            + i32::from(self.settings.acceleration_percent) * speed / 32;
        let x = i32::from(dx)
            .saturating_mul(gain)
            .saturating_add(self.remainder_x);
        let y = i32::from(dy)
            .saturating_mul(gain)
            .saturating_add(self.remainder_y);
        self.remainder_x = x % 100;
        self.remainder_y = y % 100;
        (
            (x / 100).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            (y / 100).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )
    }

    fn remember_buttons(&mut self, event: Event) {
        if let Event::Mouse(mouse) = event {
            self.reported_mouse_buttons = mouse_buttons(mouse);
        }
    }
}

const fn sign_extend_nibble(byte: u8) -> i16 {
    ((byte << 4) as i8 >> 4) as i16
}

fn mouse_buttons(event: MouseEvent) -> u8 {
    u8::from(event.left) | (u8::from(event.right) << 1) | (u8::from(event.middle) << 2)
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

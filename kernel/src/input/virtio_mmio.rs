//! Virtio-input keyboard/mouse для AArch64 QEMU `virt`.
//!
//! Устройства обнаруживаются по стандартному modern virtio-mmio header и
//! собственным capability bits, а не по порядку слотов. Event queue заранее
//! заполнена writable descriptors: host никогда не ждёт выделения памяти в
//! горячем input path. Polling — временная bootstrap-модель; формат очереди и
//! нормализованные [`Event`] останутся теми же после перехода на GIC IRQ.

use core::ptr;

use rustos_abi::input::{MouseCapabilities, MouseSettings};

use crate::{
    arch,
    input::{Event, Key, MouseEvent, PointerMotion},
    memory::{self, FrameBlock},
};

const MMIO_FIRST: u64 = 0x0a00_0000;
const MMIO_STRIDE: u64 = 0x200;
const MMIO_SLOTS: u64 = 32;
const MAGIC: u32 = 0x7472_6976;
const VERSION_MODERN: u32 = 2;
const DEVICE_INPUT: u32 = 18;

const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_AVAIL_LOW: u64 = 0x090;
const REG_QUEUE_USED_LOW: u64 = 0x0a0;
const REG_CONFIG: u64 = 0x100;

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_FAILED: u32 = 128;
const VIRTIO_F_VERSION_1_HIGH: u32 = 1;
const DESC_WRITE: u16 = 2;
const EVENT_QUEUE: u16 = 0;
const MAX_QUEUE_SIZE: u16 = 64;

const CFG_EV_BITS: u8 = 0x11;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawEvent {
    kind: u16,
    code: u16,
    value: u32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DeviceKind {
    Keyboard,
    Mouse,
}

struct Device {
    base: u64,
    queue: FrameBlock,
    events: FrameBlock,
    queue_size: u16,
    available_offset: u64,
    used_offset: u64,
    last_used: u16,
}

impl Device {
    fn initialize(kind: DeviceKind) -> Option<Self> {
        let base = find_device(kind)?;
        write32(base, REG_STATUS, 0);
        if read32(base, REG_STATUS) != 0 {
            return None;
        }
        write32(base, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        write32(base, REG_DEVICE_FEATURES_SEL, 1);
        if read32(base, REG_DEVICE_FEATURES) & VIRTIO_F_VERSION_1_HIGH == 0 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return None;
        }
        // Virtio-input не требует optional feature для базовой event queue.
        write32(base, REG_DRIVER_FEATURES_SEL, 0);
        write32(base, REG_DRIVER_FEATURES, 0);
        write32(base, REG_DRIVER_FEATURES_SEL, 1);
        write32(base, REG_DRIVER_FEATURES, VIRTIO_F_VERSION_1_HIGH);
        let negotiated = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
        write32(base, REG_STATUS, negotiated);
        if read32(base, REG_STATUS) & STATUS_FEATURES_OK == 0 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return None;
        }

        write32(base, REG_QUEUE_SEL, u32::from(EVENT_QUEUE));
        if read32(base, REG_QUEUE_READY) != 0 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return None;
        }
        let maximum = read32(base, REG_QUEUE_NUM_MAX).min(u32::from(MAX_QUEUE_SIZE));
        if maximum < 4 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return None;
        }
        let queue_size = maximum as u16;
        let descriptor_bytes = u64::from(queue_size) * 16;
        let available_offset = descriptor_bytes;
        let available_bytes = 6 + u64::from(queue_size) * 2;
        let used_offset = align_up(available_offset + available_bytes, 4);
        let queue_bytes = used_offset + 6 + u64::from(queue_size) * 8;
        let queue = memory::allocate(queue_bytes.div_ceil(4096), 1).ok()?;
        let events = match memory::allocate(1, 1) {
            Ok(block) => block,
            Err(_) => {
                let _ = memory::free(queue);
                write32(base, REG_STATUS, STATUS_FAILED);
                return None;
            }
        };
        unsafe {
            ptr::write_bytes(queue.phys as *mut u8, 0, (queue.frames * 4096) as usize);
            ptr::write_bytes(events.phys as *mut u8, 0, 4096);
            for index in 0..queue_size {
                (queue.phys as *mut Descriptor)
                    .add(usize::from(index))
                    .write_volatile(Descriptor {
                        address: events.phys + u64::from(index) * 8,
                        length: 8,
                        flags: DESC_WRITE,
                        next: 0,
                    });
                ((queue.phys + available_offset + 4) as *mut u16)
                    .add(usize::from(index))
                    .write_volatile(index);
            }
            ((queue.phys + available_offset + 2) as *mut u16).write_volatile(queue_size);
        }
        arch::dma_write_barrier();
        write32(base, REG_QUEUE_NUM, u32::from(queue_size));
        write_address(base, REG_QUEUE_DESC_LOW, queue.phys);
        write_address(base, REG_QUEUE_AVAIL_LOW, queue.phys + available_offset);
        write_address(base, REG_QUEUE_USED_LOW, queue.phys + used_offset);
        write32(base, REG_QUEUE_READY, 1);
        write32(base, REG_STATUS, negotiated | STATUS_DRIVER_OK);
        write32(base, REG_QUEUE_NOTIFY, u32::from(EVENT_QUEUE));

        Some(Self {
            base,
            queue,
            events,
            queue_size,
            available_offset,
            used_offset,
            last_used: 0,
        })
    }

    fn poll(&mut self) -> Option<RawEvent> {
        let used_index =
            unsafe { ((self.queue.phys + self.used_offset + 2) as *const u16).read_volatile() };
        if used_index == self.last_used {
            return None;
        }
        arch::dma_read_barrier();
        let slot = usize::from(self.last_used % self.queue_size);
        let used_element = self.queue.phys + self.used_offset + 4 + slot as u64 * 8;
        let descriptor = unsafe { (used_element as *const u32).read_volatile() };
        let length = unsafe { ((used_element + 4) as *const u32).read_volatile() };
        self.last_used = self.last_used.wrapping_add(1);
        if descriptor >= u32::from(self.queue_size) || length < 8 {
            return None;
        }
        let event = unsafe {
            ((self.events.phys + u64::from(descriptor) * 8) as *const RawEvent).read_volatile()
        };

        // Возвращаем использованный descriptor host'у. Индекс avail ring
        // независим от used index и читается прямо из shared queue.
        let available = self.queue.phys + self.available_offset;
        let available_index = unsafe { ((available + 2) as *const u16).read_volatile() };
        let available_slot = usize::from(available_index % self.queue_size);
        unsafe {
            ((available + 4) as *mut u16)
                .add(available_slot)
                .write_volatile(descriptor as u16);
        }
        arch::dma_write_barrier();
        unsafe {
            ((available + 2) as *mut u16).write_volatile(available_index.wrapping_add(1));
        }
        arch::dma_write_barrier();
        write32(self.base, REG_QUEUE_NOTIFY, u32::from(EVENT_QUEUE));
        Some(event)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        write32(self.base, REG_STATUS, 0);
        let _ = memory::free(self.queue);
        let _ = memory::free(self.events);
    }
}

/// Общий ARM input backend: независимые virtio keyboard и relative mouse.
pub struct VirtioInput {
    keyboard: Option<Device>,
    mouse: Option<Device>,
    keyboard_turn: bool,
    shift: bool,
    caps_lock: bool,
    buttons: u8,
    pending_dx: i32,
    pending_dy: i32,
    pending_wheel_x: i16,
    pending_wheel_y: i16,
    pending_packets: u16,
    settings: MouseSettings,
    remainder_x: i32,
    remainder_y: i32,
}

impl VirtioInput {
    pub fn new() -> Self {
        Self {
            keyboard: Device::initialize(DeviceKind::Keyboard),
            mouse: Device::initialize(DeviceKind::Mouse),
            keyboard_turn: true,
            shift: false,
            caps_lock: false,
            buttons: 0,
            pending_dx: 0,
            pending_dy: 0,
            pending_wheel_x: 0,
            pending_wheel_y: 0,
            pending_packets: 0,
            settings: MouseSettings::DEFAULT,
            remainder_x: 0,
            remainder_y: 0,
        }
    }

    /// Дренирует bounded число raw events и возвращает первое законченное
    /// keyboard событие либо mouse report, ограниченный `SYN_REPORT`.
    pub fn poll(&mut self) -> Option<Event> {
        for _ in 0..128 {
            let raw = if self.keyboard_turn {
                self.keyboard_turn = false;
                self.keyboard
                    .as_mut()
                    .and_then(Device::poll)
                    .map(|event| (true, event))
            } else {
                self.keyboard_turn = true;
                self.mouse
                    .as_mut()
                    .and_then(Device::poll)
                    .map(|event| (false, event))
            };
            let raw = raw.or_else(|| {
                self.keyboard
                    .as_mut()
                    .and_then(Device::poll)
                    .map(|event| (true, event))
            });
            let raw = raw.or_else(|| {
                self.mouse
                    .as_mut()
                    .and_then(Device::poll)
                    .map(|event| (false, event))
            });
            let (keyboard, event) = raw?;
            if keyboard {
                if let Some(event) = self.keyboard_event(event) {
                    return Some(event);
                }
            } else if let Some(event) = self.mouse_event(event) {
                return Some(event);
            }
        }
        None
    }

    pub const fn mouse_settings(&self) -> MouseSettings {
        self.settings
    }

    pub const fn mouse_capabilities(&self) -> MouseCapabilities {
        MouseCapabilities {
            configurable_sample_rate: 0,
            configurable_resolution: 0,
            wheel: 1,
            extra_buttons: 0,
            minimum_rate_hz: 0,
            maximum_rate_hz: 0,
            resolution_levels: 0,
            reserved: [0; 7],
        }
    }

    /// Virtio не обещает менять host polling rate, но все software-поля
    /// (чувствительность, acceleration, click/drag timing) применяются.
    pub fn set_mouse_settings(&mut self, requested: MouseSettings) -> bool {
        self.settings = requested.sanitized();
        self.remainder_x = 0;
        self.remainder_y = 0;
        true
    }

    fn keyboard_event(&mut self, event: RawEvent) -> Option<Event> {
        if event.kind != EV_KEY {
            return None;
        }
        let pressed = event.value != 0;
        match event.code {
            42 | 54 => {
                self.shift = pressed;
                return None;
            }
            58 if event.value == 1 => {
                self.caps_lock = !self.caps_lock;
                return None;
            }
            _ if !pressed => return None,
            _ => {}
        }
        let key = match event.code {
            1 => Key::Escape,
            14 => Key::Backspace,
            15 => Key::Tab,
            28 => Key::Enter,
            102 => Key::Home,
            103 => Key::Up,
            104 => Key::PageUp,
            105 => Key::Left,
            106 => Key::Right,
            107 => Key::End,
            108 => Key::Down,
            109 => Key::PageDown,
            code => Key::Character(linux_key_ascii(code, self.shift, self.caps_lock)?),
        };
        Some(Event::Key(key))
    }

    fn mouse_event(&mut self, event: RawEvent) -> Option<Event> {
        match (event.kind, event.code) {
            (EV_REL, REL_X) => self.pending_dx = self.pending_dx.saturating_add(event.value as i32),
            (EV_REL, REL_Y) => self.pending_dy = self.pending_dy.saturating_add(event.value as i32),
            (EV_REL, REL_HWHEEL) => {
                self.pending_wheel_x = self.pending_wheel_x.saturating_add(
                    (event.value as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                )
            }
            (EV_REL, REL_WHEEL) => {
                // Linux input: плюс = вверх; UI RustOS: плюс = вниз.
                self.pending_wheel_y = self.pending_wheel_y.saturating_sub(
                    (event.value as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                )
            }
            (EV_KEY, BTN_LEFT) => set_button(&mut self.buttons, 0, event.value != 0),
            (EV_KEY, BTN_RIGHT) => set_button(&mut self.buttons, 1, event.value != 0),
            (EV_KEY, BTN_MIDDLE) => set_button(&mut self.buttons, 2, event.value != 0),
            (EV_SYN, SYN_REPORT) => {
                let raw_x = self.pending_dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                let raw_y = self.pending_dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                let (dx, dy) = self.scale_motion(raw_x, raw_y);
                let event = MouseEvent {
                    motion: PointerMotion::Relative { dx, dy },
                    wheel_x: self.pending_wheel_x,
                    wheel_y: self.pending_wheel_y,
                    left: self.buttons & 1 != 0,
                    right: self.buttons & 2 != 0,
                    middle: self.buttons & 4 != 0,
                    packets: self.pending_packets.saturating_add(1),
                };
                self.pending_dx = 0;
                self.pending_dy = 0;
                self.pending_wheel_x = 0;
                self.pending_wheel_y = 0;
                self.pending_packets = 0;
                return Some(Event::Mouse(event));
            }
            _ => {}
        }
        self.pending_packets = self.pending_packets.saturating_add(1);
        None
    }

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
}

fn find_device(kind: DeviceKind) -> Option<u64> {
    (0..MMIO_SLOTS)
        .map(|slot| MMIO_FIRST + slot * MMIO_STRIDE)
        .find(|base| {
            read32(*base, REG_MAGIC) == MAGIC
                && read32(*base, REG_VERSION) == VERSION_MODERN
                && read32(*base, REG_DEVICE_ID) == DEVICE_INPUT
                && device_kind(*base) == Some(kind)
        })
}

fn device_kind(base: u64) -> Option<DeviceKind> {
    if config_has_bit(base, EV_REL as u8, REL_X) {
        Some(DeviceKind::Mouse)
    } else if config_has_bit(base, EV_KEY as u8, 30) {
        // KEY_A отличает полноценную клавиатуру от кнопок мыши.
        Some(DeviceKind::Keyboard)
    } else {
        None
    }
}

fn config_has_bit(base: u64, event_type: u8, code: u16) -> bool {
    write8(base, REG_CONFIG, CFG_EV_BITS);
    write8(base, REG_CONFIG + 1, event_type);
    let size = read8(base, REG_CONFIG + 2).min(128);
    let byte = (code / 8) as u8;
    byte < size && read8(base, REG_CONFIG + 8 + u64::from(byte)) & (1 << (code % 8)) != 0
}

fn set_button(buttons: &mut u8, bit: u8, pressed: bool) {
    if pressed {
        *buttons |= 1 << bit;
    } else {
        *buttons &= !(1 << bit);
    }
}

/// Linux input keycodes → US QWERTY ASCII. Раскладка и compose/IME позднее
/// переедут в user-space input service; аппаратный transport останется тем же.
fn linux_key_ascii(code: u16, shift: bool, caps: bool) -> Option<u8> {
    let letter = match code {
        30 => b'a',
        48 => b'b',
        46 => b'c',
        32 => b'd',
        18 => b'e',
        33 => b'f',
        34 => b'g',
        35 => b'h',
        23 => b'i',
        36 => b'j',
        37 => b'k',
        38 => b'l',
        50 => b'm',
        49 => b'n',
        24 => b'o',
        25 => b'p',
        16 => b'q',
        19 => b'r',
        31 => b's',
        20 => b't',
        22 => b'u',
        47 => b'v',
        17 => b'w',
        45 => b'x',
        21 => b'y',
        44 => b'z',
        _ => 0,
    };
    if letter != 0 {
        return Some(if shift ^ caps {
            letter.to_ascii_uppercase()
        } else {
            letter
        });
    }
    Some(match code {
        2 => {
            if shift {
                b'!'
            } else {
                b'1'
            }
        }
        3 => {
            if shift {
                b'@'
            } else {
                b'2'
            }
        }
        4 => {
            if shift {
                b'#'
            } else {
                b'3'
            }
        }
        5 => {
            if shift {
                b'$'
            } else {
                b'4'
            }
        }
        6 => {
            if shift {
                b'%'
            } else {
                b'5'
            }
        }
        7 => {
            if shift {
                b'^'
            } else {
                b'6'
            }
        }
        8 => {
            if shift {
                b'&'
            } else {
                b'7'
            }
        }
        9 => {
            if shift {
                b'*'
            } else {
                b'8'
            }
        }
        10 => {
            if shift {
                b'('
            } else {
                b'9'
            }
        }
        11 => {
            if shift {
                b')'
            } else {
                b'0'
            }
        }
        12 => {
            if shift {
                b'_'
            } else {
                b'-'
            }
        }
        13 => {
            if shift {
                b'+'
            } else {
                b'='
            }
        }
        26 => {
            if shift {
                b'{'
            } else {
                b'['
            }
        }
        27 => {
            if shift {
                b'}'
            } else {
                b']'
            }
        }
        39 => {
            if shift {
                b':'
            } else {
                b';'
            }
        }
        40 => {
            if shift {
                b'"'
            } else {
                b'\''
            }
        }
        41 => {
            if shift {
                b'~'
            } else {
                b'`'
            }
        }
        43 => {
            if shift {
                b'|'
            } else {
                b'\\'
            }
        }
        51 => {
            if shift {
                b'<'
            } else {
                b','
            }
        }
        52 => {
            if shift {
                b'>'
            } else {
                b'.'
            }
        }
        53 => {
            if shift {
                b'?'
            } else {
                b'/'
            }
        }
        57 => b' ',
        _ => return None,
    })
}

fn read8(base: u64, offset: u64) -> u8 {
    unsafe { ((base + offset) as *const u8).read_volatile() }
}

fn write8(base: u64, offset: u64, value: u8) {
    unsafe { ((base + offset) as *mut u8).write_volatile(value) }
}

fn read32(base: u64, offset: u64) -> u32 {
    unsafe { ((base + offset) as *const u32).read_volatile() }
}

fn write32(base: u64, offset: u64, value: u32) {
    unsafe { ((base + offset) as *mut u32).write_volatile(value) }
}

fn write_address(base: u64, low_register: u64, address: u64) {
    write32(base, low_register, address as u32);
    write32(base, low_register + 4, (address >> 32) as u32);
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

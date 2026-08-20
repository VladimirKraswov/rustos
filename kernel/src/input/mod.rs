//! Нормализованный ввод и выбор platform driver.
//!
//! GUI получает одинаковые [`Event`] на любой архитектуре. PS/2 — драйвер
//! legacy PC, а не свойство x86 CPU, поэтому он не попадает в `arch`.

#[cfg(target_arch = "aarch64")]
use rustos_abi::input::{MouseCapabilities, MouseSettings};

#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub enum Key {
    Character(u8),
    Enter,
    Backspace,
    Tab,
    Escape,
}

#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub struct MouseEvent {
    pub dx: i16,
    pub dy: i16,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    pub packets: u16,
}

#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Key(Key),
    Mouse(MouseEvent),
}

#[cfg(target_arch = "x86_64")]
mod ps2;
#[cfg(target_arch = "x86_64")]
pub use ps2::Ps2Input as PlatformInput;

/// ARM-платы используют USB HID, GPIO/keyboard controllers или virtio-input.
/// Пока конкретный driver не выбран из DT/ACPI, backend корректно сообщает
/// отсутствие событий и не обращается к выдуманным MMIO-адресам.
#[cfg(target_arch = "aarch64")]
pub struct PlatformInput;

#[cfg(target_arch = "aarch64")]
impl PlatformInput {
    pub const fn new() -> Self {
        Self
    }

    pub const fn poll(&mut self) -> Option<Event> {
        None
    }

    pub const fn mouse_settings(&self) -> MouseSettings {
        MouseSettings::DEFAULT
    }

    pub const fn mouse_capabilities(&self) -> MouseCapabilities {
        MouseCapabilities {
            configurable_sample_rate: 0,
            configurable_resolution: 0,
            wheel: 0,
            extra_buttons: 0,
            minimum_rate_hz: 0,
            maximum_rate_hz: 0,
            resolution_levels: 0,
            reserved: [0; 7],
        }
    }

    pub fn set_mouse_settings(&mut self, _requested: MouseSettings) -> bool {
        false
    }
}

pub const fn backend_name() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "ps2"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "none"
    }
}

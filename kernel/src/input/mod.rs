//! Нормализованный ввод и выбор platform driver.
//!
//! GUI получает одинаковые [`Event`] на любой архитектуре. PS/2 — драйвер
//! legacy PC, а не свойство x86 CPU, поэтому он не попадает в `arch`.

#[derive(Clone, Copy, Debug)]
pub enum Key {
    Character(u8),
    Enter,
    Backspace,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Clone, Copy, Debug)]
pub enum PointerMotion {
    Relative {
        dx: i16,
        dy: i16,
    },
    /// Координаты устройства вместе с их логическим максимумом. Window
    /// server преобразует их после выбора видеорежима, поэтому смена
    /// разрешения не требует перенастройки USB HID устройства.
    Absolute {
        x: u16,
        y: u16,
        maximum_x: u16,
        maximum_y: u16,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct MouseEvent {
    pub motion: PointerMotion,
    /// Горизонтальная прокрутка в аппаратных шагах. Положительное значение
    /// означает движение содержимого вправо.
    pub wheel_x: i16,
    /// Вертикальная прокрутка в аппаратных шагах. Положительное значение
    /// означает движение содержимого вниз.
    pub wheel_y: i16,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    pub packets: u16,
}

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Key(Key),
    Mouse(MouseEvent),
}

#[cfg(target_arch = "x86_64")]
mod ps2;
#[cfg(target_arch = "x86_64")]
use ps2::Ps2Input as FallbackInput;

#[cfg(target_arch = "aarch64")]
mod virtio_mmio;
#[cfg(target_arch = "aarch64")]
use virtio_mmio::VirtioInput as FallbackInput;

mod xhci;

use rustos_abi::input::{MouseCapabilities, MouseSettings};

/// Platform-neutral multiplexor: USB xHCI имеет приоритет отдельно для
/// клавиатуры и мыши, а отсутствующий тип устройства обслуживает PS/2 либо
/// virtio-input. Поэтому подключение только USB-клавиатуры не отключает
/// рабочую legacy-мышь и наоборот.
pub struct PlatformInput {
    usb: Option<xhci::UsbInput>,
    fallback: FallbackInput,
    usb_turn: bool,
}

impl PlatformInput {
    pub fn new() -> Self {
        // Legacy backend создаётся даже при найденном USB: это настоящий
        // hot-unplug fallback, а не решение, принятое один раз при boot.
        let fallback = FallbackInput::new();
        let usb = xhci::UsbInput::new();
        Self {
            usb,
            fallback,
            usb_turn: true,
        }
    }

    pub fn poll(&mut self) -> Option<Event> {
        for _ in 0..2 {
            self.usb_turn = !self.usb_turn;
            if self.usb_turn {
                if let Some(event) = self.usb.as_mut().and_then(xhci::UsbInput::poll) {
                    return Some(event);
                }
            } else if let Some(event) = self.fallback.poll() {
                let shadowed = self.usb.as_ref().is_some_and(|usb| match event {
                    Event::Key(_) => usb.has_keyboard(),
                    Event::Mouse(_) => usb.has_mouse(),
                });
                if !shadowed {
                    return Some(event);
                }
            }
        }
        None
    }

    pub fn mouse_settings(&self) -> MouseSettings {
        self.usb.as_ref().filter(|usb| usb.has_mouse()).map_or_else(
            || self.fallback.mouse_settings(),
            xhci::UsbInput::mouse_settings,
        )
    }

    pub fn mouse_capabilities(&self) -> MouseCapabilities {
        self.usb.as_ref().filter(|usb| usb.has_mouse()).map_or_else(
            || self.fallback.mouse_capabilities(),
            xhci::UsbInput::mouse_capabilities,
        )
    }

    pub fn set_mouse_settings(&mut self, settings: MouseSettings) -> bool {
        let fallback = self.fallback.set_mouse_settings(settings);
        let usb = self
            .usb
            .as_mut()
            .filter(|usb| usb.has_mouse())
            .is_none_or(|usb| usb.set_mouse_settings(settings));
        fallback && usb
    }

    pub fn backend_name(&self) -> &'static str {
        if self
            .usb
            .as_ref()
            .is_some_and(|usb| usb.has_keyboard() && usb.has_mouse())
        {
            "xhci-usb-hid"
        } else {
            fallback_backend_name()
        }
    }
}

const fn fallback_backend_name() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "ps2"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "virtio-input-mmio"
    }
}

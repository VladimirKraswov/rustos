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
    pending: Option<Event>,
    reported_mouse_buttons: u8,
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
            pending: None,
            reported_mouse_buttons: 0,
        }
    }

    /// Возвращает одно семантическое событие, объединяя накопленные motion
    /// reports до последней позиции. Нажатия/отпускания кнопок и клавиши
    /// остаются строгими barriers и никогда не теряются.
    pub fn poll(&mut self) -> Option<Event> {
        if let Some(event) = self.pending.take() {
            self.remember_mouse_buttons(event);
            return Some(event);
        }
        let first = self.poll_one()?;
        let Event::Mouse(mut accumulated) = first else {
            return Some(first);
        };
        let buttons = mouse_buttons(accumulated);
        if buttons != self.reported_mouse_buttons {
            self.reported_mouse_buttons = buttons;
            return Some(Event::Mouse(accumulated));
        }

        // 64 reports значительно больше одного UTM/xHCI burst, но сохраняют
        // bounded latency для клавиатуры и системных сервисов. Абсолютный HID
        // tablet берёт последнюю координату; относительная мышь суммирует
        // displacement. Поэтому окно следует за рукой, а не воспроизводит
        // историю устаревших точек после медленного GPU кадра.
        for _ in 1..64 {
            let Some(next) = self.poll_one() else {
                break;
            };
            match next {
                Event::Key(_) => {
                    self.pending = Some(next);
                    break;
                }
                Event::Mouse(mouse) if mouse_buttons(mouse) != buttons => {
                    self.pending = Some(Event::Mouse(mouse));
                    break;
                }
                Event::Mouse(mouse) => {
                    if !merge_mouse_motion(&mut accumulated, mouse) {
                        self.pending = Some(Event::Mouse(mouse));
                        break;
                    }
                }
            }
        }
        self.reported_mouse_buttons = buttons;
        Some(Event::Mouse(accumulated))
    }

    fn poll_one(&mut self) -> Option<Event> {
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

    fn remember_mouse_buttons(&mut self, event: Event) {
        if let Event::Mouse(mouse) = event {
            self.reported_mouse_buttons = mouse_buttons(mouse);
        }
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

fn merge_mouse_motion(accumulated: &mut MouseEvent, next: MouseEvent) -> bool {
    match (&mut accumulated.motion, next.motion) {
        (
            PointerMotion::Relative { dx, dy },
            PointerMotion::Relative {
                dx: next_dx,
                dy: next_dy,
            },
        ) => {
            *dx = dx.saturating_add(next_dx);
            *dy = dy.saturating_add(next_dy);
        }
        (
            PointerMotion::Absolute {
                x,
                y,
                maximum_x,
                maximum_y,
            },
            PointerMotion::Absolute {
                x: next_x,
                y: next_y,
                maximum_x: next_maximum_x,
                maximum_y: next_maximum_y,
            },
        ) if *maximum_x == next_maximum_x && *maximum_y == next_maximum_y => {
            *x = next_x;
            *y = next_y;
        }
        _ => return false,
    }
    accumulated.wheel_x = accumulated.wheel_x.saturating_add(next.wheel_x);
    accumulated.wheel_y = accumulated.wheel_y.saturating_add(next.wheel_y);
    accumulated.packets = accumulated.packets.saturating_add(next.packets);
    true
}

const fn mouse_buttons(event: MouseEvent) -> u8 {
    (event.left as u8) | ((event.right as u8) << 1) | ((event.middle as u8) << 2)
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

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
pub struct MouseEvent {
    pub dx: i16,
    pub dy: i16,
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
pub use ps2::Ps2Input as PlatformInput;

#[cfg(target_arch = "aarch64")]
mod virtio_mmio;
#[cfg(target_arch = "aarch64")]
pub use virtio_mmio::VirtioInput as PlatformInput;

pub const fn backend_name() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "ps2"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "virtio-input-mmio"
    }
}

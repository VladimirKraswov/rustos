//! GUI-подсистема раннего ядра (см. docs/GUI.md):
//! * [`chrome`] — маленький renderer-adapter оформления системных окон;
//! * [`session`] — compositor, desktop, taskbar и window manager
//!   (единственный владелец framebuffer'а и input).
//!
//! Общая библиотека компонентов находится в crate `rustos-system-ui`.
//! Здесь намеренно нет второй модели Checkbox/ListView/TextEdit: иначе
//! приложения начали бы зависеть от framebuffer и дублировать SystemUI.
mod chrome;
mod cursor;
pub(crate) mod gpu_scene;
pub mod session;

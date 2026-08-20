//! GUI-подсистема раннего ядра (см. docs/GUI.md):
//! * [`components`] — theme и базовые widgets (SDK-слой, без unsafe);
//! * [`session`] — compositor, desktop, taskbar и window manager
//!   (единственный владелец framebuffer'а и input).
pub mod components;
pub mod session;

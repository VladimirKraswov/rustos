//! Platform-independent основа видеосистемы RustOS.
//!
//! Crate не знает о GOP, PCI, процессах и allocator'е. Он работает с уже
//! выданными pixel slices и поэтому одинаково пригоден kernel compositor'у,
//! будущему user-space display server, декодеру изображений и software OpenGL.
//! Все операции bounded, не выделяют память и тестируются на host.

#![no_std]

mod compositor;
mod damage;
mod geometry;
mod pixel;
mod scanout;
mod surface;

pub use compositor::{composite, Layer};
pub use damage::DamageRegion;
pub use geometry::{Point, Rect};
pub use pixel::{Color, PixelFormat, Rgba};
pub use scanout::{
    ColorMode, ConnectorInfo, ConnectorKind, DisplayDriver, DisplayMode, ModeSetError,
    PresentStats, Scanout, ScanoutCapabilities, ScanoutError,
};
pub use surface::{Surface, SurfaceError, SurfaceMut};

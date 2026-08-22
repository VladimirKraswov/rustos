//! Platform-independent основа видеосистемы RustOS.
//!
//! Crate не знает о GOP, PCI, процессах и allocator'е. Он работает с уже
//! выданными pixel slices и поэтому одинаково пригоден kernel compositor'у,
//! будущему user-space display server, декодеру изображений и software OpenGL.
//! Все операции bounded, не выделяют память и тестируются на host.

#![no_std]

pub mod buffer;
mod compositor;
mod cpu_surface;
mod damage;
mod geometry;
mod mode_policy;
mod pixel;
pub mod protocol;
mod scanout;
mod surface_queue;
mod surface_scene;
mod window;

pub use compositor::{composite, Layer};
pub use cpu_surface::{CpuSurface, CpuSurfaceError, CpuSurfaceMut};
pub use damage::DamageRegion;
pub use geometry::{Point, Rect};
pub use mode_policy::{select_startup_mode, StartupModePolicy};
pub use pixel::{Color, CpuPixelFormat, Rgba};
pub use protocol::SurfaceId;
pub use scanout::{
    ColorMode, ConnectorInfo, ConnectorKind, DisplayDriver, DisplayMode, ModeSetError,
    PresentStats, Scanout, ScanoutCapabilities, ScanoutError,
};
pub use surface_queue::{
    MailboxSelection, SurfaceQueue, SurfaceQueueError, SurfaceSlotState, SurfaceSlotToken,
};
pub use surface_scene::{SurfaceLayerConfig, SurfaceScene, SurfaceSceneError, VisibleSurface};
pub use window::{
    hit_test_resize, resize_from_edges, ManagedWindow, ResizeEdges, WindowError, WindowEventQueue,
};

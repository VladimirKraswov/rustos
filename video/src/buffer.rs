//! Независимые от renderer'а описания графической памяти.
//!
//! Здесь намеренно нет CPU slice, MMIO или allocator'а. Дескриптор одинаково
//! понимают software renderer, compositor, display driver и будущий Mesa port.

pub use rustos_abi::graphics_buffer::{
    modifier, AlphaMode, BufferUsage, ColorDescription, ColorMatrix, ColorPrimaries, ColorRange,
    GraphicsBufferDesc, GraphicsBufferError, MemoryDomain, PixelFormatCode, PlaneLayout,
    TransferFunction, GRAPHICS_BUFFER_ABI_VERSION, GRAPHICS_BUFFER_MAX_PLANES,
};

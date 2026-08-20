//! Безопасная оболочка над UEFI GOP linear framebuffer.
//!
//! GPU-драйвера пока нет: все примитивы рисуются CPU. В одном месте
//! сосредоточены unsafe-доступы к MMIO framebuffer, clipping и упаковка
//! RGB/BGR — остальная GUI-подсистема работает обычным безопасным Rust.

use rustos_abi::bootinfo::{BootFramebuffer, FRAMEBUFFER_FORMAT_BGR, FRAMEBUFFER_FORMAT_RGB};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn mix(self, other: Self, amount: u8) -> Self {
        let a = amount as u16;
        let inv = 255 - a;
        Self {
            r: ((self.r as u16 * inv + other.r as u16 * a) / 255) as u8,
            g: ((self.g as u16 * inv + other.g as u16 * a) / 255) as u8,
            b: ((self.b as u16 * inv + other.b as u16 * a) / 255) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width as i32)
            && y < self.y.saturating_add(self.height as i32)
    }
}

pub struct Framebuffer {
    base: *mut u8,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

impl Framebuffer {
    /// Создаёт renderer после проверки BootInfo.
    pub fn from_boot(info: &BootFramebuffer) -> Option<Self> {
        if info.phys_addr == 0
            || info.width == 0
            || info.height == 0
            || info.bpp != 32
            || info.stride < info.width.saturating_mul(4)
            || !matches!(info.format, FRAMEBUFFER_FORMAT_RGB | FRAMEBUFFER_FORMAT_BGR)
        {
            return None;
        }
        Some(Self {
            base: info.phys_addr as *mut u8,
            width: info.width,
            height: info.height,
            stride: info.stride,
            format: info.format,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn pack(&self, color: Color) -> u32 {
        match self.format {
            // В little-endian первый компонент занимает младший байт.
            FRAMEBUFFER_FORMAT_RGB => {
                color.r as u32 | ((color.g as u32) << 8) | ((color.b as u32) << 16)
            }
            _ => color.b as u32 | ((color.g as u32) << 8) | ((color.r as u32) << 16),
        }
    }

    pub fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.write_raw(x as u32, y as u32, self.pack(color));
    }

    pub fn fill(&mut self, color: Color) {
        self.fill_rect(Rect::new(0, 0, self.width, self.height), color);
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        let x0 = rect.x.max(0) as u32;
        let y0 = rect.y.max(0) as u32;
        let x1 = rect
            .x
            .saturating_add(rect.width as i32)
            .clamp(0, self.width as i32) as u32;
        let y1 = rect
            .y
            .saturating_add(rect.height as i32)
            .clamp(0, self.height as i32) as u32;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let raw = self.pack(color);
        for y in y0..y1 {
            for x in x0..x1 {
                self.write_raw(x, y, raw);
            }
        }
    }

    pub fn border(&mut self, rect: Rect, color: Color) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.fill_rect(Rect::new(rect.x, rect.y, rect.width, 1), color);
        self.fill_rect(
            Rect::new(rect.x, rect.y + rect.height as i32 - 1, rect.width, 1),
            color,
        );
        self.fill_rect(Rect::new(rect.x, rect.y, 1, rect.height), color);
        self.fill_rect(
            Rect::new(rect.x + rect.width as i32 - 1, rect.y, 1, rect.height),
            color,
        );
    }

    pub fn horizontal_gradient(&mut self, rect: Rect, left: Color, right: Color) {
        let width = rect.width.max(1);
        for offset in 0..width {
            let amount = ((offset as u64 * 255) / width as u64) as u8;
            self.fill_rect(
                Rect::new(rect.x + offset as i32, rect.y, 1, rect.height),
                left.mix(right, amount),
            );
        }
    }

    pub fn vertical_gradient(&mut self, rect: Rect, top: Color, bottom: Color) {
        let height = rect.height.max(1);
        for offset in 0..height {
            let amount = ((offset as u64 * 255) / height as u64) as u8;
            self.fill_rect(
                Rect::new(rect.x, rect.y + offset as i32, rect.width, 1),
                top.mix(bottom, amount),
            );
        }
    }

    pub fn read_raw(&self, x: u32, y: u32) -> u32 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        let offset = y as usize * self.stride as usize + x as usize * 4;
        // SAFETY: clipping выше гарантирует, что offset лежит в GOP buffer.
        unsafe { self.base.add(offset).cast::<u32>().read_volatile() }
    }

    pub fn write_raw(&mut self, x: u32, y: u32, value: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y as usize * self.stride as usize + x as usize * 4;
        // SAFETY: clipping выше гарантирует, что offset лежит в GOP buffer.
        unsafe { self.base.add(offset).cast::<u32>().write_volatile(value) };
    }
}

// MMIO framebuffer принадлежит одному GUI-сеансу CPU0; между потоками этот
// объект не передаётся. Явные Send/Sync здесь намеренно не реализованы.

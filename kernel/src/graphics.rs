//! Безопасная оболочка над UEFI GOP linear framebuffer.
//!
//! GPU-драйвера пока нет: все примитивы рисуются CPU в обычный RAM back
//! buffer. Видимый GOP framebuffer обновляется только методом [`present`],
//! когда кадр уже полностью готов. Благодаря этому пользователь не видит,
//! как compositor по частям стирает старое и рисует новое положение окна.
//!
//! В одном месте сосредоточены unsafe-доступы к framebuffer, выбор памяти,
//! clipping и упаковка RGB/BGR — остальная GUI-подсистема работает обычным
//! безопасным Rust.

use core::sync::atomic::{compiler_fence, Ordering};

use rustos_abi::{
    bootinfo::{FRAMEBUFFER_FORMAT_BGR, FRAMEBUFFER_FORMAT_RGB},
    BootInfo, PAGE_SIZE,
};

use crate::memory;

/// Цвет 8-бит на канал; упаковка в байты framebuffer'а — в [`Framebuffer::pack`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    /// Краткий конструктор `Color::rgb(r, g, b)`.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Линейное смешивание: `amount = 0` → `self`, `amount = 255` → `other`.
    /// Используется градиентами рабочего стола.
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

/// Прямоугольник в пиксельных координатах кадра: левый-верхний угол
/// `(x, y)` (могут быть отрицательными — всё, что вне кадра, отсекается)
/// и размеры `width × height`.
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

    /// Попадает ли точка внутрь (левая/верхняя границы включены,
    /// правая/нижняя — нет).
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width as i32)
            && y < self.y.saturating_add(self.height as i32)
    }
}

/// Double-buffered renderer кадра (см. модуль и docs/GUI.md).
///
/// Все методы рисования работают только с невидимым back buffer; видимый
/// GOP-буфер обновляет `present`/`present_rect` построчным копированием.
pub struct Framebuffer {
    /// Видимый linear framebuffer GOP. В него пишет только `present*`.
    front: *mut u8,
    /// Невидимый программный кадр в обычной usable RAM. Плотно упакован
    /// (stride = width), в отличие от GOP со своим stride.
    back: *mut u32,
    back_phys: u64,
    back_bytes: u64,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

impl Framebuffer {
    /// Создаёт double-buffered renderer после проверки [`BootInfo`].
    ///
    /// Back buffer резервируется общим frame allocator'ом как непрерывный
    /// диапазон. Поэтому будущие процессы/DMA mappings не смогут получить
    /// те же физические страницы.
    pub fn from_boot(boot: &BootInfo) -> Option<Self> {
        let info = &boot.framebuffer;
        if info.phys_addr == 0
            || info.width == 0
            || info.height == 0
            || info.bpp != 32
            || !info.stride.is_multiple_of(4)
            || info.stride < info.width.checked_mul(4)?
            || !matches!(info.format, FRAMEBUFFER_FORMAT_RGB | FRAMEBUFFER_FORMAT_BGR)
        {
            return None;
        }

        let back_bytes = u64::from(info.width)
            .checked_mul(u64::from(info.height))?
            .checked_mul(4)?;
        let back_phys = reserve_back_buffer(back_bytes)?;

        Some(Self {
            front: info.phys_addr as *mut u8,
            back: back_phys as *mut u32,
            back_phys,
            back_bytes,
            width: info.width,
            height: info.height,
            stride: info.stride,
            format: info.format,
        })
    }

    /// Ширина кадра в пикселях (из GOP mode).
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Высота кадра в пикселях (из GOP mode).
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Физический адрес back buffer'а (для диагностики и будущих page tables).
    pub const fn backbuffer_phys(&self) -> u64 {
        self.back_phys
    }

    /// Размер back buffer'а в байтах.
    pub const fn backbuffer_bytes(&self) -> u64 {
        self.back_bytes
    }

    /// Упаковка `Color` в 32-битный пиксель текущего формата framebuffer'а
    /// (RGB или BGR по `BootInfo.framebuffer.format`).
    pub fn pack(&self, color: Color) -> u32 {
        match self.format {
            // В little-endian первый компонент занимает младший байт.
            FRAMEBUFFER_FORMAT_RGB => {
                color.r as u32 | ((color.g as u32) << 8) | ((color.b as u32) << 16)
            }
            _ => color.b as u32 | ((color.g as u32) << 8) | ((color.r as u32) << 16),
        }
    }

    /// Ставит один пиксель в back buffer; точки вне кадра молча отбрасываются.
    pub fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.write_raw(x as u32, y as u32, self.pack(color));
    }

    /// Заливает весь кадр одним цветом (очистка сцены).
    pub fn fill(&mut self, color: Color) {
        self.fill_rect(Rect::new(0, 0, self.width, self.height), color);
    }

    /// Заливает прямоугольник цветом; выход за границы кадра обрезается.
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

    /// Рисует рамку толщиной 1 px по периметру `rect`.
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

    /// Горизонтальный градиент в `rect`: столбец за столбцом от `left` к `right`.
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

    /// Вертикальный градиент в `rect`: строка за строкой от `top` к `bottom`.
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

    /// Читает уже упакованный пиксель back buffer'а (вне кадра — 0).
    pub fn read_raw(&self, x: u32, y: u32) -> u32 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        let index = y as usize * self.width as usize + x as usize;
        // SAFETY: clipping выше гарантирует index < width * height, а back
        // buffer выделен ровно под это число u32-пикселей.
        unsafe { self.back.add(index).read() }
    }

    /// Записывает уже упакованный пиксель в back buffer (вне кадра — no-op).
    pub fn write_raw(&mut self, x: u32, y: u32, value: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let index = y as usize * self.width as usize + x as usize;
        // SAFETY: clipping выше гарантирует index < width * height, а back
        // buffer выделен ровно под это число u32-пикселей.
        unsafe { self.back.add(index).write(value) };
    }

    /// Публикует целиком уже готовый кадр в GOP.
    pub fn present(&mut self) {
        self.present_rect(Rect::new(0, 0, self.width, self.height));
    }

    /// Публикует прямоугольную dirty-область готового кадра.
    ///
    /// GOP — linear framebuffer с обычной x86 memory semantics. Поэтому
    /// построчный `copy_nonoverlapping` существенно быстрее миллионов
    /// отдельных volatile store и минимизирует время, когда scanout может
    /// пересечь копируемый кадр. Рисование компонентов никогда не происходит
    /// в видимой памяти.
    pub fn present_rect(&mut self, rect: Rect) {
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

        let count = (x1 - x0) as usize;
        for y in y0..y1 {
            let back_index = y as usize * self.width as usize + x0 as usize;
            let front_offset = y as usize * self.stride as usize + x0 as usize * 4;
            // SAFETY: оба диапазона проверены clipping'ом. Back и GOP не
            // пересекаются: первый выбран из Usable RAM, второй отмечен MMIO.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.back.add(back_index),
                    self.front.add(front_offset).cast::<u32>(),
                    count,
                );
            }
        }
        compiler_fence(Ordering::Release);
    }
}

/// Выбирает page-aligned диапазон RAM под кадр, не вводя фиксированного
/// ограничения на разрешение. Для 4K потребуется около 32 MiB, для
/// 1280x800 — около 4 MiB; размер автоматически следует GOP mode.
fn reserve_back_buffer(bytes: u64) -> Option<u64> {
    let frames = bytes.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE;
    memory::allocate(frames, 1).ok().map(|block| block.phys)
}

// MMIO framebuffer принадлежит одному GUI-сеансу CPU0; между потоками этот
// объект не передаётся. Явные Send/Sync здесь намеренно не реализованы.

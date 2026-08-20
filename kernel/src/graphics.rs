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
pub use rustos_video::{Color, Rect};
use rustos_video::{
    DamageRegion, DisplayMode, PixelFormat, PresentStats, Scanout, ScanoutCapabilities,
    ScanoutError, Surface, SurfaceMut,
};

use crate::memory;

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
    /// Снимок статического desktop-слоя без окон и курсора. Он позволяет
    /// при drag восстановить только старое место окна, не перерисовывая
    /// миллион пикселей обоев на каждый пакет мыши.
    background: *mut u32,
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
    present_sequence: u64,
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
        let format = match info.format {
            FRAMEBUFFER_FORMAT_RGB => PixelFormat::Rgb888,
            FRAMEBUFFER_FORMAT_BGR => PixelFormat::Bgr888,
            _ => return None,
        };

        let back_bytes = u64::from(info.width)
            .checked_mul(u64::from(info.height))?
            .checked_mul(4)?;
        let back_phys = reserve_back_buffer(back_bytes)?;
        // Кэш — оптимизация, а не условие работоспособности. На очень
        // большом GOP при минимуме RAM compositor корректно откатится к
        // полному redraw, если второй непрерывный диапазон получить нельзя.
        let background = reserve_back_buffer(back_bytes)
            .map(|block| block as *mut u32)
            .unwrap_or_default();

        Some(Self {
            front: info.phys_addr as *mut u8,
            back: back_phys as *mut u32,
            back_phys,
            back_bytes,
            background,
            width: info.width,
            height: info.height,
            stride: info.stride,
            format,
            present_sequence: 0,
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

    /// Доступен ли статический desktop-слой для damage-only compositor'а.
    pub fn has_background_cache(&self) -> bool {
        !self.background.is_null()
    }

    /// Упаковка `Color` в 32-битный пиксель текущего формата framebuffer'а
    /// (RGB или BGR по `BootInfo.framebuffer.format`).
    pub fn pack(&self, color: Color) -> u32 {
        self.format.pack_color(color)
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
        if let Some(mut surface) = self.back_surface() {
            let _ = surface.fill(rect, color);
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

    fn back_surface(&mut self) -> Option<SurfaceMut<'_>> {
        let pixels = (self.back_bytes / 4) as usize;
        // SAFETY: `back` принадлежит этому Framebuffer на весь срок GUI;
        // &mut self гарантирует единственное mutable представление.
        let storage = unsafe { core::slice::from_raw_parts_mut(self.back, pixels) };
        SurfaceMut::new(storage, self.width, self.height, self.width, self.format).ok()
    }

    /// Сохраняет готовый desktop без окон/курсора в отдельный RAM-слой.
    /// Вызывается только после изменения обоев, иконок или taskbar, а не на
    /// каждом движении мыши.
    pub fn cache_background(&mut self) -> bool {
        if self.background.is_null() {
            return false;
        }
        let pixels = (self.back_bytes / 4) as usize;
        // SAFETY: оба буфера выделены frame allocator'ом на back_bytes,
        // не пересекаются и принадлежат единственной GUI-сессии CPU0.
        unsafe { core::ptr::copy_nonoverlapping(self.back, self.background, pixels) };
        true
    }

    /// Восстанавливает прямоугольник из статического desktop-слоя в back
    /// buffer. Возвращает false, если memory pressure отключил кэш.
    pub fn restore_background(&mut self, rect: Rect) -> bool {
        if self.background.is_null() {
            return false;
        }
        let Some((x0, y0, x1, y1)) = self.clipped(rect) else {
            return true;
        };
        let count = (x1 - x0) as usize;
        for y in y0..y1 {
            let index = y as usize * self.width as usize + x0 as usize;
            // SAFETY: clipped гарантирует, что обе строки целиком находятся
            // в одинаково sized back/background buffers.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.background.add(index),
                    self.back.add(index),
                    count,
                )
            };
        }
        true
    }

    /// Публикует целиком уже готовый кадр в GOP.
    pub fn present(&mut self) {
        self.present_rect(Rect::new(0, 0, self.width, self.height));
    }

    /// Публикует bounded набор damage rectangles. Tracker заранее clipping'ит
    /// и объединяет пересечения, поэтому scanout не копирует один участок
    /// много раз даже при десятках окон.
    pub fn present_damage<const CAPACITY: usize>(&mut self, damage: &DamageRegion<CAPACITY>) {
        self.present_regions(damage.as_slice());
    }

    /// Публикует прямоугольную dirty-область готового кадра.
    ///
    /// GOP — linear framebuffer с обычной x86 memory semantics. Поэтому
    /// построчный `copy_nonoverlapping` существенно быстрее миллионов
    /// отдельных volatile store и минимизирует время, когда scanout может
    /// пересечь копируемый кадр. Рисование компонентов никогда не происходит
    /// в видимой памяти.
    pub fn present_rect(&mut self, rect: Rect) {
        self.present_regions(core::slice::from_ref(&rect));
    }

    fn present_regions(&mut self, damage: &[Rect]) {
        let pixels = (self.back_bytes / 4) as usize;
        // SAFETY: source читает только back, а Scanout::present пишет только
        // отдельный GOP front. Эксклюзивный &mut self не покидает вызов.
        let storage = unsafe { core::slice::from_raw_parts(self.back, pixels) };
        let Ok(source) = Surface::new(storage, self.width, self.height, self.width, self.format)
        else {
            return;
        };
        self.present_sequence = self.present_sequence.wrapping_add(1);
        let _ = <Self as Scanout>::present(self, source, damage, self.present_sequence);
    }

    fn clipped(&self, rect: Rect) -> Option<(u32, u32, u32, u32)> {
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
        (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
    }
}

impl Scanout for Framebuffer {
    fn mode(&self) -> DisplayMode {
        DisplayMode {
            width: self.width,
            height: self.height,
            stride_pixels: self.stride / 4,
            format: self.format,
            refresh_millihertz: 0,
        }
    }

    fn capabilities(&self) -> ScanoutCapabilities {
        ScanoutCapabilities {
            page_flip: false,
            vsync_event: false,
            hardware_cursor: false,
            multiple_outputs: false,
        }
    }

    fn present(
        &mut self,
        source: Surface<'_>,
        damage: &[Rect],
        sequence: u64,
    ) -> Result<PresentStats, ScanoutError> {
        if source.width() != self.width || source.height() != self.height {
            return Err(ScanoutError::InvalidSurface);
        }
        let bounds = Rect::new(0, 0, self.width, self.height);
        let mut rectangles = 0u32;
        let mut pixels = 0u64;
        for rect in damage.iter().copied() {
            let clipped = rect.intersection(bounds);
            if clipped.is_empty() {
                continue;
            }
            rectangles = rectangles.saturating_add(1);
            pixels = pixels.saturating_add(clipped.area());
            for y in clipped.y as u32..clipped.bottom() as u32 {
                let source_row = source
                    .row(y, clipped.x as u32, clipped.width)
                    .ok_or(ScanoutError::InvalidSurface)?;
                let front_offset = y as usize * self.stride as usize
                    + clipped.x as usize * core::mem::size_of::<u32>();
                let destination = self.front.wrapping_add(front_offset).cast::<u32>();
                if source.format() == self.format {
                    // SAFETY: source row валиден; destination находится в
                    // mapped GOP строке, RAM back и MMIO front не пересекаются.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            source_row.as_ptr(),
                            destination,
                            source_row.len(),
                        )
                    };
                } else {
                    for (offset, raw) in source_row.iter().copied().enumerate() {
                        let converted = self.format.pack(source.format().unpack(raw));
                        // SAFETY: offset < clipped.width; GOP row проверена mode.
                        unsafe { destination.add(offset).write(converted) };
                    }
                }
            }
        }
        compiler_fence(Ordering::Release);
        Ok(PresentStats {
            sequence,
            rectangles,
            pixels,
        })
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

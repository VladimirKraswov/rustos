//! Software renderer поверх virtio-gpu либо GRUB/firmware framebuffer.
//!
//! Все примитивы рисуются CPU в обычный RAM back buffer. Virtio-gpu получает
//! только damage через 2D transfer/flush; firmware fallback — готовые строки
//! scanout. Благодаря этому пользователь не видит промежуточные стадии кадра,
//! а mode-set не связан с rasterizer'ом или оконными компонентами.
//!
//! В одном месте сосредоточены unsafe-доступы к framebuffer, выбор памяти,
//! clipping и упаковка RGB/BGR — остальная GUI-подсистема работает обычным
//! безопасным Rust.

use core::sync::atomic::{compiler_fence, Ordering};

use rustos_abi::{
    bootinfo::{FRAMEBUFFER_FORMAT_BGR, FRAMEBUFFER_FORMAT_RGB, FRAMEBUFFER_SOURCE_GRUB},
    BootInfo, PAGE_SIZE,
};
use rustos_system_assets::{IconTarget, Wallpaper};
pub use rustos_video::{Color, Rect};
use rustos_video::{
    ColorMode, ConnectorInfo, ConnectorKind, DamageRegion, DisplayDriver, DisplayMode,
    ModeSetError, PixelFormat, PresentStats, Scanout, ScanoutCapabilities, ScanoutError, Surface,
    SurfaceMut,
};

use crate::{
    display::VirtioGpu,
    memory::{self, FrameBlock},
    serial,
};

/// Double-buffered renderer кадра (см. модуль и docs/GUI.md).
///
/// Все методы рисования работают только с невидимым back buffer; видимый
/// Scanout обновляется `present`/`present_rect` линейным копированием.
pub struct Framebuffer {
    /// Видимый linear framebuffer. В него пишет только `present*`.
    front: *mut u8,
    /// Native virtio scanout. `None` означает firmware framebuffer fallback.
    gpu: Option<VirtioGpu>,
    /// Невидимый программный кадр в обычной usable RAM. Плотно упакован
    /// (stride = width), в отличие от scanout со своим stride.
    back: *mut u32,
    back_block: FrameBlock,
    back_phys: u64,
    back_bytes: u64,
    /// Снимок статического desktop-слоя без окон и курсора. Он позволяет
    /// при drag восстановить только старое место окна, не перерисовывая
    /// миллион пикселей обоев на каждый пакет мыши.
    background: *mut u32,
    background_block: Option<FrameBlock>,
    width: u32,
    height: u32,
    stride: u32,
    /// Формат физической scanout-памяти, выбранный GRUB/firmware.
    scanout_format: PixelFormat,
    /// Формат software surface. Может переключаться между true-color,
    /// RGB565 и grayscale без packed/unaligned framebuffer writes.
    render_format: PixelFormat,
    source: u32,
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
        let firmware_format = match info.format {
            FRAMEBUFFER_FORMAT_RGB => Some(PixelFormat::Rgb888),
            FRAMEBUFFER_FORMAT_BGR => Some(PixelFormat::Bgr888),
            _ => None,
        };
        let firmware_valid = info.phys_addr != 0
            && info.width != 0
            && info.height != 0
            && info.bpp == 32
            && info.stride.is_multiple_of(4)
            && info.stride >= info.width.checked_mul(4)?
            && firmware_format.is_some();
        let fallback = DisplayMode {
            width: if firmware_valid { info.width } else { 1280 },
            height: if firmware_valid { info.height } else { 720 },
            stride_pixels: if firmware_valid {
                info.stride / 4
            } else {
                1280
            },
            format: firmware_format.unwrap_or(PixelFormat::Bgr888),
            refresh_millihertz: 0,
        };
        let gpu = match VirtioGpu::initialize(fallback) {
            Ok(gpu) => {
                serial::put_str("[video] virtio-gpu modern PCI controlq ready\n");
                Some(gpu)
            }
            Err(_) => {
                serial::put_str("[video] virtio-gpu unavailable; using firmware framebuffer\n");
                None
            }
        };
        if gpu.is_none() && !firmware_valid {
            return None;
        }
        let selected = gpu.as_ref().map_or(fallback, VirtioGpu::mode);
        let width = selected.width;
        let height = selected.height;
        let scanout_format = if gpu.is_some() {
            PixelFormat::Bgr888
        } else {
            firmware_format?
        };
        let stride = if gpu.is_some() {
            width.checked_mul(4)?
        } else {
            info.stride
        };
        let back_bytes = frame_bytes(width, height)?;
        let back_block = reserve_back_buffer(back_bytes)?;
        let back = back_block.phys as *mut u32;
        // Кэш — оптимизация, а не условие работоспособности. На очень
        // большом framebuffer при минимуме RAM compositor корректно откатится к
        // полному redraw, если второй непрерывный диапазон получить нельзя.
        let background_block = reserve_back_buffer(back_bytes);
        let background = background_block
            .map(|block| block.phys as *mut u32)
            .unwrap_or_default();
        unsafe {
            core::ptr::write_bytes(
                back.cast::<u8>(),
                0,
                (back_block.frames * PAGE_SIZE) as usize,
            );
            if let Some(block) = background_block {
                core::ptr::write_bytes(
                    block.phys as *mut u8,
                    0,
                    (block.frames * PAGE_SIZE) as usize,
                );
            }
        }

        Some(Self {
            front: info.phys_addr as *mut u8,
            gpu,
            back,
            back_block,
            back_phys: back_block.phys,
            back_bytes,
            background,
            background_block,
            width,
            height,
            stride,
            scanout_format,
            render_format: scanout_format,
            source: info._reserved,
            present_sequence: 0,
        })
    }

    /// Ширина кадра в пикселях (из monitor mode).
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Высота кадра в пикселях (из monitor mode).
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

    /// Имя bootstrap display driver'а для диагностики и GUI.
    pub const fn driver_name(&self) -> &'static str {
        if self.gpu.is_some() {
            "virtio-gpu"
        } else if self.source == FRAMEBUFFER_SOURCE_GRUB {
            "grub-fb"
        } else {
            "uefi-gop"
        }
    }

    pub const fn color_mode(&self) -> ColorMode {
        match self.render_format {
            PixelFormat::Rgb565 => ColorMode::HighColor16,
            PixelFormat::Grayscale8 => ColorMode::Grayscale8,
            _ => ColorMode::TrueColor24,
        }
    }

    /// Меняет цветовой профиль renderer'а. Backbuffer остаётся u32-aligned,
    /// а present при необходимости конвертирует его в физический XRGB/BGRX.
    /// Caller обязан полностью перерисовать сцену после смены профиля.
    pub fn set_color_mode(&mut self, mode: ColorMode) {
        self.render_format = match mode {
            ColorMode::TrueColor24 => self.scanout_format,
            ColorMode::HighColor16 => PixelFormat::Rgb565,
            ColorMode::Grayscale8 => PixelFormat::Grayscale8,
        };
    }

    /// Упаковка `Color` в 32-битный пиксель текущего формата framebuffer'а
    /// (RGB или BGR по `BootInfo.framebuffer.format`).
    pub fn pack(&self, color: Color) -> u32 {
        self.render_format.pack_color(color)
    }

    /// Ставит один пиксель в back buffer; точки вне кадра молча отбрасываются.
    pub fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.write_raw(x as u32, y as u32, self.pack(color));
    }

    /// Смешивает пиксель текста/иконки с уже нарисованным фоном.
    ///
    /// `coverage=0` не меняет framebuffer, `coverage=255` полностью заменяет
    /// пиксель. Метод нужен сглаженным системным шрифтам и намеренно живёт
    /// здесь: только framebuffer знает фактический RGB/BGR/565/gray формат.
    pub fn blend_pixel(&mut self, x: i32, y: i32, foreground: Color, coverage: u8) {
        if coverage == 0 || x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        if coverage == u8::MAX {
            self.put_pixel(x, y, foreground);
            return;
        }
        let raw = self.read_raw(x as u32, y as u32);
        let background = self.render_format.unpack(raw);
        let background = Color::rgb(background.r, background.g, background.b);
        self.put_pixel(x, y, background.mix(foreground, coverage));
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

    /// Масштабирует RGB565-обои в `rect` по правилу cover, сохраняя пропорции.
    /// Декодер не требует heap: исходник memory-mapped в read-only секцию, а
    /// каждый пиксель сразу преобразуется в активный render format.
    pub fn draw_wallpaper(&mut self, rect: Rect, image: Wallpaper) {
        if rect.width == 0 || rect.height == 0 || image.width == 0 || image.height == 0 {
            return;
        }
        let destination_ratio_wide = u64::from(rect.width) * u64::from(image.height)
            > u64::from(rect.height) * u64::from(image.width);
        let (sample_width, sample_height) = if destination_ratio_wide {
            (
                image.width,
                (u64::from(image.width) * u64::from(rect.height) / u64::from(rect.width)).max(1)
                    as u32,
            )
        } else {
            (
                (u64::from(image.height) * u64::from(rect.width) / u64::from(rect.height)).max(1)
                    as u32,
                image.height,
            )
        };
        let crop_x = image.width.saturating_sub(sample_width) / 2;
        let crop_y = image.height.saturating_sub(sample_height) / 2;
        for destination_y in 0..rect.height {
            let screen_y = rect.y + destination_y as i32;
            if screen_y < 0 || screen_y >= self.height as i32 {
                continue;
            }
            let source_y = crop_y
                + (u64::from(destination_y) * u64::from(sample_height) / u64::from(rect.height))
                    as u32;
            for destination_x in 0..rect.width {
                let screen_x = rect.x + destination_x as i32;
                if screen_x < 0 || screen_x >= self.width as i32 {
                    continue;
                }
                let source_x = crop_x
                    + (u64::from(destination_x) * u64::from(sample_width) / u64::from(rect.width))
                        as u32;
                let raw = self.pack(image.pixel(source_x, source_y));
                self.write_raw(screen_x as u32, screen_y as u32, raw);
            }
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
        SurfaceMut::new(
            storage,
            self.width,
            self.height,
            self.width,
            self.render_format,
        )
        .ok()
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

    /// Публикует целиком уже готовый кадр в scanout.
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
    /// GRUB/firmware framebuffer — linear scanout memory. Поэтому
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
        // отдельный front. Эксклюзивный &mut self не покидает вызов.
        let storage = unsafe { core::slice::from_raw_parts(self.back, pixels) };
        let Ok(source) = Surface::new(
            storage,
            self.width,
            self.height,
            self.width,
            self.render_format,
        ) else {
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

impl IconTarget for Framebuffer {
    fn fill(&mut self, rect: Rect, color: Color) {
        self.fill_rect(rect, color);
    }

    fn stroke(&mut self, rect: Rect, color: Color) {
        self.border(rect, color);
    }
}

impl Scanout for Framebuffer {
    fn mode(&self) -> DisplayMode {
        DisplayMode {
            width: self.width,
            height: self.height,
            stride_pixels: self.stride / 4,
            format: self.scanout_format,
            refresh_millihertz: 0,
        }
    }

    fn capabilities(&self) -> ScanoutCapabilities {
        self.gpu.as_ref().map_or(
            ScanoutCapabilities {
                page_flip: false,
                vsync_event: false,
                hardware_cursor: false,
                multiple_outputs: false,
            },
            VirtioGpu::capabilities,
        )
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
        if let Some(gpu) = self.gpu.as_mut() {
            return gpu.present(source, damage, sequence);
        }
        let bounds = Rect::new(0, 0, self.width, self.height);
        if damage.len() == 1
            && damage[0].intersection(bounds) == bounds
            && source.format() == self.scanout_format
            && self.stride / 4 == self.width
        {
            if let Some(frame) = source.contiguous_pixels() {
                // Самый частый полный commit: одна последовательная запись
                // вместо height отдельных копирований. Framebuffer никогда
                // не читается обратно, поэтому CPU cache не загрязняется
                // лишним read-modify-write на стороне compositor'а.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        frame.as_ptr(),
                        self.front.cast::<u32>(),
                        frame.len(),
                    )
                };
                compiler_fence(Ordering::Release);
                return Ok(PresentStats {
                    sequence,
                    rectangles: 1,
                    pixels: u64::from(self.width) * u64::from(self.height),
                });
            }
        }
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
                if source.format() == self.scanout_format {
                    // SAFETY: source row валиден; destination находится в
                    // mapped scanout-строке, RAM back и MMIO front не пересекаются.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            source_row.as_ptr(),
                            destination,
                            source_row.len(),
                        )
                    };
                } else {
                    for (offset, raw) in source_row.iter().copied().enumerate() {
                        let converted = self.scanout_format.pack(source.format().unpack(raw));
                        // SAFETY: offset < clipped.width; scanout row проверена mode.
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

impl DisplayDriver for Framebuffer {
    fn connector(&self) -> ConnectorInfo {
        if let Some(gpu) = self.gpu.as_ref() {
            return gpu.connector();
        }
        ConnectorInfo {
            kind: ConnectorKind::FirmwareFramebuffer,
            connected: true,
            preferred_mode: self.mode(),
            width_mm: 0,
            height_mm: 0,
        }
    }

    fn modes(&self, output: &mut [DisplayMode]) -> usize {
        if let Some(gpu) = self.gpu.as_ref() {
            return gpu.modes(output);
        }
        if let Some(first) = output.first_mut() {
            *first = self.mode();
            1
        } else {
            0
        }
    }

    fn set_mode(&mut self, requested: DisplayMode) -> Result<DisplayMode, ModeSetError> {
        let current = self.mode();
        if requested.width == current.width
            && requested.height == current.height
            && requested.format == current.format
        {
            return Ok(current);
        }
        let Some(gpu) = self.gpu.as_mut() else {
            return Err(ModeSetError::RequiresReboot);
        };
        let bytes =
            frame_bytes(requested.width, requested.height).ok_or(ModeSetError::UnsupportedMode)?;
        let new_back = reserve_back_buffer(bytes).ok_or(ModeSetError::OutOfMemory)?;
        let new_background = reserve_back_buffer(bytes);
        if let Err(error) = gpu.set_mode(requested) {
            let _ = memory::free(new_back);
            if let Some(block) = new_background {
                let _ = memory::free(block);
            }
            return Err(error);
        }
        unsafe {
            core::ptr::write_bytes(
                new_back.phys as *mut u8,
                0,
                (new_back.frames * PAGE_SIZE) as usize,
            );
            if let Some(block) = new_background {
                core::ptr::write_bytes(
                    block.phys as *mut u8,
                    0,
                    (block.frames * PAGE_SIZE) as usize,
                );
            }
        }
        let old_back = self.back_block;
        let old_background = self.background_block;
        self.back_block = new_back;
        self.back = new_back.phys as *mut u32;
        self.back_phys = new_back.phys;
        self.back_bytes = bytes;
        self.background_block = new_background;
        self.background = new_background
            .map(|block| block.phys as *mut u32)
            .unwrap_or_default();
        self.width = requested.width;
        self.height = requested.height;
        self.stride = requested.width * 4;
        self.scanout_format = PixelFormat::Bgr888;
        if matches!(
            self.render_format,
            PixelFormat::Rgb888 | PixelFormat::Bgr888
        ) {
            self.render_format = PixelFormat::Bgr888;
        }
        let _ = memory::free(old_back);
        if let Some(block) = old_background {
            let _ = memory::free(block);
        }
        Ok(self.mode())
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        // GUI session обычно живёт до shutdown, но владение должно
        // оставаться корректным и для будущего перезапуска displayd.
        let _ = memory::free(self.back_block);
        self.back_block = FrameBlock { phys: 0, frames: 0 };
        if let Some(block) = self.background_block.take() {
            let _ = memory::free(block);
        }
        self.back = core::ptr::null_mut();
        self.background = core::ptr::null_mut();
    }
}

/// Выбирает page-aligned диапазон RAM под кадр, не вводя фиксированного
/// ограничения на разрешение. Для 4K потребуется около 32 MiB, для
/// 1280x800 — около 4 MiB; размер автоматически следует monitor mode.
fn reserve_back_buffer(bytes: u64) -> Option<FrameBlock> {
    let frames = bytes.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE;
    memory::allocate(frames, 1).ok()
}

fn frame_bytes(width: u32, height: u32) -> Option<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)
}

// MMIO framebuffer принадлежит одному GUI-сеансу CPU0; между потоками этот
// объект не передаётся. Явные Send/Sync здесь намеренно не реализованы.

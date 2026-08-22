//! Renderer facade поверх GPU SystemUI либо GRUB/firmware CPU framebuffer.
//!
//! При доступном VirGL те же безопасные методы записывают renderer-neutral
//! quads для ring-3 `renderd`: rasterization, blend и scanout не обходят GPU.
//! При отсутствии/падении GPU facade переключается на обычный RAM back buffer
//! и firmware/virtio 2D present без изменения кода приложений.
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
    ColorMode, ConnectorInfo, ConnectorKind, CpuPixelFormat, CpuSurface, CpuSurfaceMut,
    DamageRegion, DisplayDriver, DisplayMode, ModeSetError, PresentStats, Scanout,
    ScanoutCapabilities, ScanoutError,
};

use crate::{
    display::scanout,
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
    /// Native virtio scanout живёт в единственном kernel broker. Поле хранит
    /// только выбор backend'а и не дублирует владение устройством.
    native_scanout: bool,
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
    /// Полностью отрисованное окно на время move gesture. Буфер имеет размер
    /// только window damage, а не всего экрана: content копируется как слой и
    /// не растеризуется заново на каждый mouse packet.
    drag_layer: *mut u32,
    drag_layer_block: Option<FrameBlock>,
    drag_layer_width: u32,
    drag_layer_height: u32,
    width: u32,
    height: u32,
    stride: u32,
    /// Формат физической scanout-памяти, выбранный GRUB/firmware.
    scanout_format: CpuPixelFormat,
    /// Формат software surface. Может переключаться между true-color,
    /// RGB565 и grayscale без packed/unaligned framebuffer writes.
    render_format: CpuPixelFormat,
    source: u32,
    present_sequence: u64,
    present_failure_logged: bool,
    /// Transitional kernel adapter записывает UI-команды вместо CPU pixels.
    gpu_recording: bool,
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
            FRAMEBUFFER_FORMAT_RGB => Some(CpuPixelFormat::Rgb888),
            FRAMEBUFFER_FORMAT_BGR => Some(CpuPixelFormat::Bgr888),
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
            format: firmware_format.unwrap_or(CpuPixelFormat::Bgr888),
            refresh_millihertz: 0,
        };
        let native_mode = match scanout::initialize(fallback) {
            Ok(mode) => {
                #[cfg(target_arch = "x86_64")]
                serial::put_str("[video] virtio-gpu modern PCI controlq ready\n");
                #[cfg(target_arch = "aarch64")]
                serial::put_str("[video] virtio-gpu modern MMIO controlq ready\n");
                Some(mode)
            }
            Err(error) => {
                serial::put_str("[video] virtio-gpu unavailable; using firmware framebuffer\n");
                serial::put_str("[hardware] display-probe=virtio-gpu result=unavailable reason=");
                serial::put_str(error.diagnostic_name());
                serial::put_str(" fallback=");
                if !firmware_valid {
                    serial::put_str("none");
                } else if info._reserved == FRAMEBUFFER_SOURCE_GRUB {
                    serial::put_str("grub-fb");
                } else {
                    serial::put_str("uefi-gop");
                }
                serial::put_str(" renderer=cpu mode=");
                serial::put_u32(fallback.width);
                serial::put_str("x");
                serial::put_u32(fallback.height);
                serial::put_str("\n");
                None
            }
        };
        if native_mode.is_none() && !firmware_valid {
            return None;
        }
        let selected = native_mode.unwrap_or(fallback);
        let width = selected.width;
        let height = selected.height;
        let scanout_format = if native_mode.is_some() {
            CpuPixelFormat::Bgr888
        } else {
            firmware_format?
        };
        let stride = if native_mode.is_some() {
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
            native_scanout: native_mode.is_some(),
            back,
            back_block,
            back_phys: back_block.phys,
            back_bytes,
            background,
            background_block,
            drag_layer: core::ptr::null_mut(),
            drag_layer_block: None,
            drag_layer_width: 0,
            drag_layer_height: 0,
            width,
            height,
            stride,
            scanout_format,
            render_format: scanout_format,
            source: info._reserved,
            present_sequence: 0,
            present_failure_logged: false,
            gpu_recording: false,
        })
    }

    /// Переключает только системный backend; API приложений не меняется.
    pub fn set_gpu_recording(&mut self, enabled: bool) {
        self.gpu_recording = enabled;
    }

    /// Активен ли штатный GPU SystemUI backend.
    pub const fn gpu_recording(&self) -> bool {
        self.gpu_recording
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

    /// Сохраняет полностью сформированный window layer для live move.
    /// Allocation происходит один раз на mouse-down, никогда в hot path
    /// движения. Resize не использует этот кэш, потому что меняет layout.
    pub fn cache_drag_layer(&mut self, rect: Rect) -> bool {
        if rect.is_empty() {
            return false;
        }
        let Some(bytes) = frame_bytes(rect.width, rect.height) else {
            return false;
        };
        let required_frames = bytes.div_ceil(PAGE_SIZE);
        let reusable = self
            .drag_layer_block
            .is_some_and(|block| block.frames >= required_frames);
        if !reusable {
            if let Some(block) = self.drag_layer_block.take() {
                let _ = memory::free(block);
            }
            let Some(block) = reserve_back_buffer(bytes) else {
                self.drag_layer = core::ptr::null_mut();
                self.drag_layer_width = 0;
                self.drag_layer_height = 0;
                return false;
            };
            self.drag_layer = block.phys as *mut u32;
            self.drag_layer_block = Some(block);
        }
        self.drag_layer_width = rect.width;
        self.drag_layer_height = rect.height;
        for destination_y in 0..rect.height {
            for destination_x in 0..rect.width {
                let source_x = rect.x.saturating_add(destination_x as i32);
                let source_y = rect.y.saturating_add(destination_y as i32);
                let value = if source_x >= 0
                    && source_y >= 0
                    && source_x < self.width as i32
                    && source_y < self.height as i32
                {
                    self.read_raw(source_x as u32, source_y as u32)
                } else {
                    0
                };
                let index = destination_y as usize * rect.width as usize + destination_x as usize;
                // SAFETY: storage содержит не меньше rect.width*rect.height
                // u32, что проверено через frame_bytes/allocation выше.
                unsafe { self.drag_layer.add(index).write(value) };
            }
        }
        true
    }

    /// Композитит сохранённый layer в новое положение. Source и destination
    /// имеют одинаковые размеры; clipping сдвигает source offset, поэтому
    /// окно корректно уходит частично за левую/верхнюю границу экрана.
    pub fn draw_cached_drag_layer(&mut self, rect: Rect) -> bool {
        if self.drag_layer.is_null()
            || rect.width != self.drag_layer_width
            || rect.height != self.drag_layer_height
        {
            return false;
        }
        let Some((x0, y0, x1, y1)) = self.clipped(rect) else {
            return true;
        };
        let source_x = (x0 as i32 - rect.x).max(0) as u32;
        let source_y = (y0 as i32 - rect.y).max(0) as u32;
        let copy_width = x1.saturating_sub(x0) as usize;
        for row in 0..y1.saturating_sub(y0) {
            let source_index = (source_y + row) as usize * rect.width as usize + source_x as usize;
            let destination_index = (y0 + row) as usize * self.width as usize + x0 as usize;
            // SAFETY: оба row диапазона предварительно clipped; буферы разные
            // и принадлежат одному Framebuffer.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.drag_layer.add(source_index),
                    self.back.add(destination_index),
                    copy_width,
                )
            };
        }
        true
    }

    /// Имя bootstrap display driver'а для диагностики и GUI.
    pub const fn driver_name(&self) -> &'static str {
        if self.native_scanout {
            "virtio-gpu"
        } else if self.source == FRAMEBUFFER_SOURCE_GRUB {
            "grub-fb"
        } else {
            "uefi-gop"
        }
    }

    /// Hardware cursor доступен только у native driver с отдельной plane.
    /// Firmware framebuffer никогда не выдаётся за ускоренный cursor path.
    pub fn hardware_cursor_supported(&self) -> bool {
        self.native_scanout && self.capabilities().hardware_cursor
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_hardware_cursor(
        &mut self,
        pixels: &[u32],
        width: u32,
        height: u32,
        hotspot_x: u32,
        hotspot_y: u32,
        pointer_x: i32,
        pointer_y: i32,
    ) -> bool {
        self.native_scanout
            && scanout::update_cursor(
                pixels, width, height, hotspot_x, hotspot_y, pointer_x, pointer_y,
            )
            .is_ok()
    }

    pub fn move_hardware_cursor(&mut self, pointer_x: i32, pointer_y: i32) -> bool {
        self.native_scanout && scanout::move_cursor(pointer_x, pointer_y).is_ok()
    }

    pub const fn color_mode(&self) -> ColorMode {
        match self.render_format {
            CpuPixelFormat::Rgb565 => ColorMode::HighColor16,
            CpuPixelFormat::Grayscale8 => ColorMode::Grayscale8,
            _ => ColorMode::TrueColor24,
        }
    }

    /// Меняет цветовой профиль renderer'а. Backbuffer остаётся u32-aligned,
    /// а present при необходимости конвертирует его в физический XRGB/BGRX.
    /// Caller обязан полностью перерисовать сцену после смены профиля.
    pub fn set_color_mode(&mut self, mode: ColorMode) {
        self.render_format = match mode {
            ColorMode::TrueColor24 => self.scanout_format,
            ColorMode::HighColor16 => CpuPixelFormat::Rgb565,
            ColorMode::Grayscale8 => CpuPixelFormat::Grayscale8,
        };
    }

    /// Упаковка `Color` в 32-битный пиксель текущего формата framebuffer'а
    /// (RGB или BGR по `BootInfo.framebuffer.format`).
    pub fn pack(&self, color: Color) -> u32 {
        self.render_format.pack_color(color)
    }

    /// Ставит один пиксель в back buffer; точки вне кадра молча отбрасываются.
    pub fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if self.gpu_recording {
            crate::gui::gpu_scene::solid(Rect::new(x, y, 1, 1), color, u8::MAX);
            return;
        }
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.write_raw(x as u32, y as u32, self.pack(color));
    }

    /// Масштабирует linear B8G8R8X8 surface прямо в backbuffer. Координаты
    /// вычисляются до растеризации, а четыре соседних texel смешиваются в
    /// fixed-point — окно не превращается в крупные квадраты при resize.
    pub fn blit_bgrx_bilinear(
        &mut self,
        source: &[u32],
        source_width: u32,
        source_height: u32,
        destination: Rect,
    ) {
        if self.gpu_recording {
            self.record_bgrx_mesh(source, source_width, source_height, destination);
            return;
        }
        if source_width == 0
            || source_height == 0
            || destination.is_empty()
            || source.len()
                != usize::try_from(u64::from(source_width) * u64::from(source_height))
                    .unwrap_or(usize::MAX)
        {
            return;
        }
        let Some((x0, y0, x1, y1)) = self.clipped(destination) else {
            return;
        };
        if destination.width == source_width && destination.height == source_height {
            let count = (x1 - x0) as usize;
            for y in y0..y1 {
                let source_y = (y as i32 - destination.y) as usize;
                let source_x = (x0 as i32 - destination.x) as usize;
                let source_offset = source_y * source_width as usize + source_x;
                let destination_offset = y as usize * self.width as usize + x0 as usize;
                if self.render_format == CpuPixelFormat::Bgr888 {
                    // SAFETY: clipped гарантирует полный destination span в
                    // backbuffer, а размеры source проверены выше. Массивы
                    // принадлежат разным retained surfaces и не пересекаются.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            source.as_ptr().add(source_offset),
                            self.back.add(destination_offset),
                            count,
                        )
                    };
                } else {
                    for column in 0..count {
                        let rgba = CpuPixelFormat::Bgr888.unpack(source[source_offset + column]);
                        // SAFETY: destination_offset + count проверен clipped.
                        unsafe {
                            self.back
                                .add(destination_offset + column)
                                .write(self.render_format.pack(rgba))
                        };
                    }
                }
            }
            return;
        }
        let max_source_x = source_width.saturating_sub(1);
        let max_source_y = source_height.saturating_sub(1);
        for y in y0..y1 {
            let local_y = (y as i32 - destination.y).max(0) as u32;
            let source_y_16 = scale_coordinate_16(local_y, destination.height, source_height);
            let sy0 = ((source_y_16 >> 16) as u32).min(max_source_y);
            let sy1 = sy0.saturating_add(1).min(max_source_y);
            let fy = source_y_16 as u16;
            for x in x0..x1 {
                let local_x = (x as i32 - destination.x).max(0) as u32;
                let source_x_16 = scale_coordinate_16(local_x, destination.width, source_width);
                let sx0 = ((source_x_16 >> 16) as u32).min(max_source_x);
                let sx1 = sx0.saturating_add(1).min(max_source_x);
                let fx = source_x_16 as u16;
                let row0 = sy0 as usize * source_width as usize;
                let row1 = sy1 as usize * source_width as usize;
                let top = mix_bgrx(source[row0 + sx0 as usize], source[row0 + sx1 as usize], fx);
                let bottom = mix_bgrx(source[row1 + sx0 as usize], source[row1 + sx1 as usize], fx);
                let pixel = mix_bgrx(top, bottom, fy);
                let rgba = CpuPixelFormat::Bgr888.unpack(pixel);
                self.write_raw(x, y, self.render_format.pack(rgba));
            }
        }
    }

    /// Представляет bitmap как небольшую сетку GPU-gradient quads. Это
    /// recovery bridge для legacy Canvas pixels (включая раннюю Aurora 3D):
    /// CPU только читает редкие control points, но не растеризует destination.
    fn record_bgrx_mesh(
        &mut self,
        source: &[u32],
        source_width: u32,
        source_height: u32,
        destination: Rect,
    ) {
        if source_width == 0 || source_height == 0 || destination.is_empty() {
            return;
        }
        const COLUMNS: u32 = 32;
        const ROWS: u32 = 18;
        let sample = |dx: u32, dy: u32| {
            let sx = (u64::from(dx) * u64::from(source_width.saturating_sub(1))
                / u64::from(destination.width.max(1))) as u32;
            let sy = (u64::from(dy) * u64::from(source_height.saturating_sub(1))
                / u64::from(destination.height.max(1))) as u32;
            let raw = source
                .get((sy * source_width + sx) as usize)
                .copied()
                .unwrap_or(0);
            let rgba = CpuPixelFormat::Bgr888.unpack(raw);
            Color::rgb(rgba.r, rgba.g, rgba.b)
        };
        for row in 0..ROWS {
            let top = destination.height * row / ROWS;
            let bottom = destination.height * (row + 1) / ROWS;
            for column in 0..COLUMNS {
                let left = destination.width * column / COLUMNS;
                let right = destination.width * (column + 1) / COLUMNS;
                crate::gui::gpu_scene::gradient(
                    Rect::new(
                        destination.x + left as i32,
                        destination.y + top as i32,
                        right.saturating_sub(left).max(1),
                        bottom.saturating_sub(top).max(1),
                    ),
                    [
                        sample(left, top),
                        sample(right, top),
                        sample(right, bottom),
                        sample(left, bottom),
                    ],
                );
            }
        }
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
        let coverage = if self.gpu_recording {
            quantize_gpu_coverage(coverage)
        } else {
            coverage
        };
        if coverage == 0 {
            return;
        }
        if coverage == u8::MAX {
            self.put_pixel(x, y, foreground);
            return;
        }
        if self.gpu_recording {
            crate::gui::gpu_scene::solid(Rect::new(x, y, 1, 1), foreground, coverage);
            return;
        }
        let raw = self.read_raw(x as u32, y as u32);
        let background = self.render_format.unpack(raw);
        let background = Color::rgb(background.r, background.g, background.b);
        self.put_pixel(x, y, background.mix(foreground, coverage));
    }

    /// Смешивает горизонтальный coverage-span одним семантическим primitive.
    ///
    /// Шрифт и векторные иконки часто состоят из десятков соседних пикселей
    /// с одинаковой прозрачностью. Для CPU fallback результат остаётся
    /// побитно тем же, а GPU backend получает один quad вместо серии команд
    /// по одному пикселю. Это особенно важно для текста при перетаскивании
    /// окна: сложность command stream зависит от числа spans, а не пикселей.
    pub fn blend_span(&mut self, x: i32, y: i32, width: u32, foreground: Color, coverage: u8) {
        if coverage == 0 || width == 0 || y < 0 || y >= self.height as i32 {
            return;
        }
        let coverage = if self.gpu_recording {
            quantize_gpu_coverage(coverage)
        } else {
            coverage
        };
        if coverage == 0 {
            return;
        }
        let left = x.max(0);
        let right = x
            .saturating_add(i32::try_from(width).unwrap_or(i32::MAX))
            .min(self.width as i32);
        if left >= right {
            return;
        }
        let visible_width = (right - left) as u32;
        if self.gpu_recording {
            crate::gui::gpu_scene::solid(
                Rect::new(left, y, visible_width, 1),
                foreground,
                coverage,
            );
            return;
        }
        for current_x in left..right {
            self.blend_pixel(current_x, y, foreground, coverage);
        }
    }

    /// Заливает прямоугольник цветом; выход за границы кадра обрезается.
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        if self.gpu_recording {
            crate::gui::gpu_scene::solid(rect, color, u8::MAX);
            return;
        }
        if let Some(mut surface) = self.back_surface() {
            let _ = surface.fill(rect, color);
        }
    }

    /// Размещает управляемый системой аппаратный 3D Canvas.
    ///
    /// Приложение не получает GPU capability и не знает выбранный backend.
    /// В GPU-сеансе в display list попадает только semantic primitive; CPU
    /// fallback вызывающий код рисует обычным framebuffer API.
    pub fn draw_aurora_canvas(&mut self, rect: Rect, instance_id: u32, scene_frame: u32) -> bool {
        if !self.gpu_recording || rect.is_empty() || instance_id == 0 {
            return false;
        }
        crate::gui::gpu_scene::aurora_canvas(rect, instance_id, scene_frame);
        true
    }

    /// Заливает скруглённый прямоугольник. Прямые участки остаются быстрыми
    /// span-fill, а только маленькие corner tiles получают 4×4 coverage
    /// supersampling. Поэтому checkbox/toggle/card имеют гладкий силуэт без
    /// FPU, heap и дорогого supersampling всей площади control.
    pub fn fill_rounded_rect(&mut self, rect: Rect, radius: u8, color: Color) {
        let clip = Rect::new(0, 0, self.width, self.height);
        self.fill_rounded_rect_clipped(rect, radius, color, clip);
    }

    /// Версия [`Self::fill_rounded_rect`] для display-list damage clip. Corner
    /// geometry считается от исходного `rect`, а не от intersection: иначе
    /// частичная перерисовка меняла бы форму control.
    pub fn fill_rounded_rect_clipped(&mut self, rect: Rect, radius: u8, color: Color, clip: Rect) {
        if rect.is_empty() || clip.is_empty() {
            return;
        }
        let radius = rounded_radius(rect, radius);
        if radius == 0 {
            self.fill_rect(rect.intersection(clip), color);
            return;
        }
        if self.gpu_recording {
            // GPU bridge записывает контур строками. Одинаковые middle rows
            // recorder объединит вертикально, поэтому rounded control стоит
            // O(radius) quads вместо O(radius²) corner pixels. Аналитический
            // SDF shader позднее заменит и эти несколько spans без изменения
            // публичного component API.
            for row in 0..rect.height {
                let inset = rounded_inset_for_height(row, rect.height, radius);
                self.fill_rect(
                    Rect::new(
                        rect.x.saturating_add(inset as i32),
                        rect.y.saturating_add(row as i32),
                        rect.width.saturating_sub(inset.saturating_mul(2)),
                        1,
                    )
                    .intersection(clip),
                    color,
                );
            }
            return;
        }

        self.fill_rect(
            Rect::new(
                rect.x.saturating_add(radius as i32),
                rect.y,
                rect.width.saturating_sub(radius.saturating_mul(2)),
                rect.height,
            )
            .intersection(clip),
            color,
        );
        self.fill_rect(
            Rect::new(
                rect.x,
                rect.y.saturating_add(radius as i32),
                rect.width,
                rect.height.saturating_sub(radius.saturating_mul(2)),
            )
            .intersection(clip),
            color,
        );
        for row in 0..radius {
            for column in 0..radius {
                let coverage = rounded_corner_coverage(column as i32, row as i32, radius);
                if coverage == 0 {
                    continue;
                }
                for (x, y) in rounded_corner_pixels(rect, column, row) {
                    if !clip.contains(x, y) {
                        continue;
                    }
                    if coverage == u8::MAX {
                        self.put_pixel(x, y, color);
                    } else {
                        self.blend_pixel(x, y, color, coverage);
                    }
                }
            }
        }
    }

    /// Маскирует углы уже нарисованного прямоугольного bitmap системным
    /// фоном. Это дешёвый путь для wallpaper/image preview без alpha-канала:
    /// само изображение остаётся обычным быстрым blit, а округляются только
    /// несколько строк по углам.
    pub fn mask_rounded_corners(&mut self, rect: Rect, radius: u8, background: Color) {
        let radius = rounded_radius(rect, radius);
        for row in 0..radius {
            let inset = rounded_row_inset(row, radius);
            if inset == 0 {
                continue;
            }
            let top = rect.y.saturating_add(row as i32);
            let bottom = rect.bottom().saturating_sub(row as i32 + 1);
            for y in [top, bottom] {
                self.fill_rect(Rect::new(rect.x, y, inset, 1), background);
                self.fill_rect(
                    Rect::new(rect.right().saturating_sub(inset as i32), y, inset, 1),
                    background,
                );
            }
        }
    }

    /// Скруглённая рамка произвольной толщины. Внутренность не заливается,
    /// поэтому primitive можно накладывать поверх gradient/image surface.
    pub fn rounded_border(&mut self, rect: Rect, radius: u8, width: u8, color: Color) {
        let clip = Rect::new(0, 0, self.width, self.height);
        self.rounded_border_clipped(rect, radius, width, color, clip);
    }

    /// Скруглённая рамка с сохранением исходной corner geometry при damage.
    pub fn rounded_border_clipped(
        &mut self,
        rect: Rect,
        radius: u8,
        width: u8,
        color: Color,
        clip: Rect,
    ) {
        let border = u32::from(width.max(1));
        if rect.is_empty() || clip.is_empty() || rect.width <= border || rect.height <= border {
            return;
        }
        let radius = rounded_radius(rect, radius);
        if radius == 0 {
            for inset in 0..border {
                let current = Rect::new(
                    rect.x.saturating_add(inset as i32),
                    rect.y.saturating_add(inset as i32),
                    rect.width.saturating_sub(inset.saturating_mul(2)),
                    rect.height.saturating_sub(inset.saturating_mul(2)),
                );
                for edge in rectangular_border(current) {
                    self.fill_rect(edge.intersection(clip), color);
                }
            }
            return;
        }
        if self.gpu_recording {
            for row in 0..rect.height {
                let inset = rounded_inset_for_height(row, rect.height, radius);
                let y = rect.y.saturating_add(row as i32);
                let left = rect.x.saturating_add(inset as i32);
                let right = rect.right().saturating_sub(inset as i32);
                if row < border || row + border >= rect.height {
                    self.fill_rect(
                        Rect::new(left, y, (right - left).max(0) as u32, 1).intersection(clip),
                        color,
                    );
                } else {
                    self.fill_rect(Rect::new(left, y, border, 1).intersection(clip), color);
                    self.fill_rect(
                        Rect::new(right.saturating_sub(border as i32), y, border, 1)
                            .intersection(clip),
                        color,
                    );
                }
            }
            return;
        }

        let straight_width = rect.width.saturating_sub(radius.saturating_mul(2));
        let straight_height = rect.height.saturating_sub(radius.saturating_mul(2));
        self.fill_rect(
            Rect::new(
                rect.x.saturating_add(radius as i32),
                rect.y,
                straight_width,
                border,
            )
            .intersection(clip),
            color,
        );
        self.fill_rect(
            Rect::new(
                rect.x.saturating_add(radius as i32),
                rect.bottom().saturating_sub(border as i32),
                straight_width,
                border,
            )
            .intersection(clip),
            color,
        );
        self.fill_rect(
            Rect::new(
                rect.x,
                rect.y.saturating_add(radius as i32),
                border,
                straight_height,
            )
            .intersection(clip),
            color,
        );
        self.fill_rect(
            Rect::new(
                rect.right().saturating_sub(border as i32),
                rect.y.saturating_add(radius as i32),
                border,
                straight_height,
            )
            .intersection(clip),
            color,
        );

        let inner_radius = radius.saturating_sub(border);
        for row in 0..radius {
            for column in 0..radius {
                let outer = rounded_corner_coverage(column as i32, row as i32, radius);
                let inner = inner_corner_coverage(column, row, border, inner_radius);
                let coverage = outer.saturating_sub(inner);
                if coverage == 0 {
                    continue;
                }
                for (x, y) in rounded_corner_pixels(rect, column, row) {
                    if clip.contains(x, y) {
                        self.blend_pixel(x, y, color, coverage);
                    }
                }
            }
        }
    }

    /// Мягкая CPU-тень окна: несколько полупрозрачных скруглённых контуров
    /// вместо прежних сплошных чёрных полос справа и снизу. Стоимость зависит
    /// от периметра, а не от площади окна.
    pub fn soft_shadow(&mut self, rect: Rect, radius: u8, clip: Rect) {
        // Три контура визуально дают достаточную глубину на 24/16-bit
        // framebuffer и ограничивают стоимость первого сложного кадра. Восемь
        // полупрозрачных слоёв не давали заметной пользы, но умножали read/
        // blend/write операции для каждой карточки component tree.
        for (spread, coverage) in [(9u32, 18u8), (6, 28), (3, 42)] {
            let shadow = Rect::new(
                rect.x.saturating_sub(spread as i32),
                rect.y.saturating_sub(spread as i32).saturating_add(3),
                rect.width.saturating_add(spread.saturating_mul(2)),
                rect.height.saturating_add(spread.saturating_mul(2)),
            );
            self.blended_rounded_border_clipped(
                shadow,
                radius.saturating_add(spread as u8),
                Color::rgb(5, 10, 20),
                coverage,
                clip,
            );
        }
    }

    /// Дешёвая тень карточки/меню. Renderer сначала рисует слегка смещённую
    /// семантическую поверхность, затем обычная карточка закрывает её центр.
    /// В результате остаётся единый мягкий силуэт без концентрических линий
    /// и без дорогого alpha read-modify-write для каждого пикселя.
    pub fn surface_shadow(&mut self, rect: Rect, radius: u8, color: Color, clip: Rect) {
        let shadow = Rect::new(
            rect.x.saturating_sub(3),
            rect.y.saturating_add(4),
            rect.width.saturating_add(6),
            rect.height.saturating_add(4),
        );
        self.fill_rounded_rect_clipped(shadow, radius.saturating_add(3), color, clip);
    }

    fn blended_rounded_border_clipped(
        &mut self,
        rect: Rect,
        radius: u8,
        color: Color,
        coverage: u8,
        clip: Rect,
    ) {
        if rect.is_empty() || clip.is_empty() {
            return;
        }
        let radius = rounded_radius(rect, radius);
        for row in 0..rect.height {
            let inset = rounded_inset_for_height(row, rect.height, radius);
            let left = rect.x.saturating_add(inset as i32);
            let right = rect.right().saturating_sub(inset as i32).saturating_sub(1);
            let y = rect.y.saturating_add(row as i32);
            if row == 0 || row + 1 == rect.height {
                self.blend_horizontal_clipped(left, right, y, color, coverage, clip);
            } else {
                if clip.contains(left, y) {
                    self.blend_pixel(left, y, color, coverage);
                }
                if right != left && clip.contains(right, y) {
                    self.blend_pixel(right, y, color, coverage);
                }
            }
        }
    }

    fn blend_horizontal_clipped(
        &mut self,
        left: i32,
        right: i32,
        y: i32,
        color: Color,
        coverage: u8,
        clip: Rect,
    ) {
        if y < clip.y || y >= clip.bottom() {
            return;
        }
        let first = left.max(clip.x);
        let last = right.min(clip.right().saturating_sub(1));
        if first <= last {
            self.blend_span(first, y, (last - first + 1) as u32, color, coverage);
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
        if self.gpu_recording {
            crate::gui::gpu_scene::gradient(rect, [left, right, right, left]);
            return;
        }
        let width = rect.width.max(1);
        for offset in 0..width {
            let amount = ((offset as u64 * 255) / width as u64) as u8;
            self.fill_rect(
                Rect::new(rect.x + offset as i32, rect.y, 1, rect.height),
                left.mix(right, amount),
            );
        }
    }

    /// Масштабирует RGB888-обои в `rect` по правилу cover, сохраняя пропорции.
    /// Декодер не требует heap: исходник memory-mapped в read-only секцию, а
    /// bilinear sample сразу преобразуется в активный render format.
    pub fn draw_wallpaper(&mut self, rect: Rect, image: Wallpaper) {
        self.draw_wallpaper_clipped(rect, rect, image);
    }

    /// Растеризует только `clip`, но сохраняет масштаб и crop полного
    /// `rect`. Иначе локальная перерисовка ярлыка показала бы уменьшенную
    /// копию всех обоев вместо исходных пикселей этой области.
    pub fn draw_wallpaper_clipped(&mut self, rect: Rect, clip: Rect, image: Wallpaper) {
        if rect.width == 0 || rect.height == 0 || image.width == 0 || image.height == 0 {
            return;
        }
        let surface = Rect::new(0, 0, self.width, self.height);
        let clipped = rect.intersection(clip).intersection(surface);
        if clipped.is_empty() {
            return;
        }
        if self.gpu_recording {
            self.record_wallpaper_mesh(rect, clipped, image);
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
        for screen_y in clipped.y..clipped.bottom() {
            let destination_y = screen_y.saturating_sub(rect.y) as u32;
            let source_y_16 = (u64::from(crop_y) << 16)
                + scale_coordinate_16(destination_y, rect.height, sample_height);
            for screen_x in clipped.x..clipped.right() {
                let destination_x = screen_x.saturating_sub(rect.x) as u32;
                let source_x_16 = (u64::from(crop_x) << 16)
                    + scale_coordinate_16(destination_x, rect.width, sample_width);
                let raw = self.pack(image.pixel_bilinear(source_x_16, source_y_16));
                self.write_raw(screen_x as u32, screen_y as u32, raw);
            }
        }
    }

    /// Передаёт semantic wallpaper resource, а не уже растеризованные pixels.
    /// Ring-3 renderd держит полноразмерную GPU texture и применяет cover crop
    /// с linear filtering. Поэтому steady-state кадр содержит один quad, а не
    /// сотни цветных tiles и никогда не читает stale CPU framebuffer.
    fn record_wallpaper_mesh(&mut self, rect: Rect, clip: Rect, image: Wallpaper) {
        if !rect.intersection(clip).is_empty() {
            crate::gui::gpu_scene::wallpaper(rect, image.id as u32);
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

    fn back_surface(&mut self) -> Option<CpuSurfaceMut<'_>> {
        let pixels = (self.back_bytes / 4) as usize;
        // SAFETY: `back` принадлежит этому Framebuffer на весь срок GUI;
        // &mut self гарантирует единственное mutable представление.
        let storage = unsafe { core::slice::from_raw_parts_mut(self.back, pixels) };
        CpuSurfaceMut::new(
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
        if self.gpu_recording {
            // GPU-сеанс пересобирает полный retained display list; копия RAM
            // background не нужна и только тратила бы memory bandwidth.
            return true;
        }
        if self.background.is_null() {
            return false;
        }
        let pixels = (self.back_bytes / 4) as usize;
        // SAFETY: оба буфера выделены frame allocator'ом на back_bytes,
        // не пересекаются и принадлежат единственной GUI-сессии CPU0.
        unsafe { core::ptr::copy_nonoverlapping(self.back, self.background, pixels) };
        true
    }

    /// Обновляет только изменившуюся часть desktop-слоя.
    /// Это не заставляет копировать весь framebuffer при selection одного
    /// ярлыка. Caller обязан вызвать метод до отрисовки окон и cursor.
    pub fn cache_background_rect(&mut self, rect: Rect) -> bool {
        if self.gpu_recording {
            return true;
        }
        if self.background.is_null() {
            return false;
        }
        let Some((x0, y0, x1, y1)) = self.clipped(rect) else {
            return true;
        };
        let count = (x1 - x0) as usize;
        for y in y0..y1 {
            let index = y as usize * self.width as usize + x0 as usize;
            // SAFETY: clipped гарантирует одинаковые valid ranges в обоих
            // непересекающих framebuffer layers.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.back.add(index),
                    self.background.add(index),
                    count,
                )
            };
        }
        true
    }

    /// Восстанавливает прямоугольник из статического desktop-слоя в back
    /// buffer. Возвращает false, если memory pressure отключил кэш.
    pub fn restore_background(&mut self, rect: Rect) -> bool {
        if self.gpu_recording {
            return false;
        }
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

    /// Публикует сохранённый newest mailbox frame после того, как GUI уже
    /// проверил input. Firmware framebuffer не имеет асинхронной очереди.
    pub fn service_scanout(&mut self) {
        if self.native_scanout {
            let _ = scanout::service_present();
        }
    }

    fn present_regions(&mut self, damage: &[Rect]) {
        if self.gpu_recording {
            // GPU frame публикуют только renderd/compositord/displayd. Так
            // legacy incremental present не может перетереть новый scanout.
            return;
        }
        let pixels = (self.back_bytes / 4) as usize;
        // SAFETY: source читает только back, а Scanout::present пишет только
        // отдельный front. Эксклюзивный &mut self не покидает вызов.
        let storage = unsafe { core::slice::from_raw_parts(self.back, pixels) };
        let Ok(source) = CpuSurface::new(
            storage,
            self.width,
            self.height,
            self.width,
            self.render_format,
        ) else {
            return;
        };
        self.present_sequence = self.present_sequence.wrapping_add(1);
        if let Err(error) = <Self as Scanout>::present(self, source, damage, self.present_sequence)
        {
            if !self.present_failure_logged {
                serial::put_str("[video] present failed; frame not published reason=");
                serial::put_str(match error {
                    ScanoutError::InvalidSurface => "invalid-surface",
                    ScanoutError::UnsupportedFormat => "unsupported-format",
                    ScanoutError::DeviceLost => "device-lost",
                });
                serial::put_str("\n");
                self.present_failure_logged = true;
            }
        }
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

/// Преобразует координату центра destination pixel в 16.16 source space.
/// Обе крайние точки совпадают, поэтому sampler не читает за изображением и
/// не создаёт полупиксельную размытую рамку.
fn scale_coordinate_16(destination: u32, destination_size: u32, source_size: u32) -> u64 {
    if destination_size <= 1 || source_size <= 1 {
        return 0;
    }
    u64::from(destination.min(destination_size - 1)) * (u64::from(source_size - 1) << 16)
        / u64::from(destination_size - 1)
}

fn mix_bgrx(left: u32, right: u32, fraction: u16) -> u32 {
    let fraction = u32::from(fraction);
    let inverse = 65_536u32.saturating_sub(fraction);
    let channel = |shift: u32| {
        let a = (left >> shift) & 0xff;
        let b = (right >> shift) & 0xff;
        ((a * inverse + b * fraction + 32_768) >> 16) & 0xff
    };
    channel(0) | (channel(8) << 8) | (channel(16) << 16)
}

/// Временная 4-bit-подобная coverage palette GPU bridge.
///
/// До постоянного glyph atlas одинаковые соседние уровни объединяются в
/// длинные spans. Пять уровней сохраняют сглаживание, но вместо почти
/// уникальной alpha каждого пикселя дают renderer'у крупные batches. CPU
/// fallback и исходные font bitmaps не меняются.
fn quantize_gpu_coverage(coverage: u8) -> u8 {
    match coverage {
        0..=23 => 0,
        24..=79 => 64,
        80..=143 => 128,
        144..=207 => 192,
        _ => u8::MAX,
    }
}

impl IconTarget for Framebuffer {
    fn fill(&mut self, rect: Rect, color: Color) {
        self.fill_rect(rect, color);
    }

    fn stroke(&mut self, rect: Rect, color: Color) {
        self.border(rect, color);
    }

    fn rounded_fill(&mut self, rect: Rect, radius: u8, color: Color) {
        self.fill_rounded_rect(rect, radius, color);
    }

    fn rounded_stroke(&mut self, rect: Rect, radius: u8, color: Color) {
        self.rounded_border(rect, radius, 1, color);
    }
}

fn rounded_radius(rect: Rect, requested: u8) -> u32 {
    u32::from(requested)
        .min(rect.width / 2)
        .min(rect.height / 2)
}

/// Покрытие одного corner pixel по 16 subpixel samples. Координаты sample
/// умножены на восемь, поэтому circle test остаётся полностью целочисленным.
fn rounded_corner_coverage(x: i32, y: i32, radius: u32) -> u8 {
    if radius == 0 {
        return u8::MAX;
    }
    let center = radius as i32 * 8;
    let radius_squared = center.saturating_mul(center);
    let mut inside = 0u16;
    for sample_y in 0..4i32 {
        for sample_x in 0..4i32 {
            let point_x = x.saturating_mul(8).saturating_add(sample_x * 2 + 1);
            let point_y = y.saturating_mul(8).saturating_add(sample_y * 2 + 1);
            let dx = center.saturating_sub(point_x);
            let dy = center.saturating_sub(point_y);
            if dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy)) <= radius_squared {
                inside += 1;
            }
        }
    }
    ((inside * 255 + 8) / 16) as u8
}

fn inner_corner_coverage(column: u32, row: u32, border: u32, radius: u32) -> u8 {
    if column < border || row < border {
        return 0;
    }
    let x = column - border;
    let y = row - border;
    if radius == 0 || x >= radius || y >= radius {
        u8::MAX
    } else {
        rounded_corner_coverage(x as i32, y as i32, radius)
    }
}

fn rounded_corner_pixels(rect: Rect, column: u32, row: u32) -> [(i32, i32); 4] {
    [
        (
            rect.x.saturating_add(column as i32),
            rect.y.saturating_add(row as i32),
        ),
        (
            rect.right().saturating_sub(column as i32 + 1),
            rect.y.saturating_add(row as i32),
        ),
        (
            rect.x.saturating_add(column as i32),
            rect.bottom().saturating_sub(row as i32 + 1),
        ),
        (
            rect.right().saturating_sub(column as i32 + 1),
            rect.bottom().saturating_sub(row as i32 + 1),
        ),
    ]
}

fn rounded_row_inset(row: u32, radius: u32) -> u32 {
    if radius == 0 || row >= radius {
        return 0;
    }
    let distance = radius.saturating_sub(row + 1);
    let remaining = radius
        .saturating_mul(radius)
        .saturating_sub(distance.saturating_mul(distance));
    let mut horizontal = radius;
    while horizontal.saturating_mul(horizontal) > remaining {
        horizontal = horizontal.saturating_sub(1);
    }
    radius.saturating_sub(horizontal)
}

fn rounded_inset_for_height(row: u32, height: u32, radius: u32) -> u32 {
    if radius == 0 || height == 0 {
        return 0;
    }
    let edge = row.min(height.saturating_sub(row + 1));
    rounded_row_inset(edge, radius)
}

fn rectangular_border(rect: Rect) -> [Rect; 4] {
    [
        Rect::new(rect.x, rect.y, rect.width, 1),
        Rect::new(rect.x, rect.bottom().saturating_sub(1), rect.width, 1),
        Rect::new(rect.x, rect.y, 1, rect.height),
        Rect::new(rect.right().saturating_sub(1), rect.y, 1, rect.height),
    ]
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
        if self.native_scanout {
            scanout::capabilities().unwrap_or(ScanoutCapabilities {
                page_flip: false,
                vsync_event: false,
                hardware_cursor: false,
                multiple_outputs: false,
            })
        } else {
            ScanoutCapabilities {
                page_flip: false,
                vsync_event: false,
                hardware_cursor: false,
                multiple_outputs: false,
            }
        }
    }

    fn present(
        &mut self,
        source: CpuSurface<'_>,
        damage: &[Rect],
        sequence: u64,
    ) -> Result<PresentStats, ScanoutError> {
        if source.width() != self.width || source.height() != self.height {
            return Err(ScanoutError::InvalidSurface);
        }
        if self.native_scanout {
            return scanout::present(source, damage, sequence).map_err(|error| match error {
                scanout::DisplayBrokerError::InvalidSurface => ScanoutError::InvalidSurface,
                _ => ScanoutError::DeviceLost,
            });
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
        if self.native_scanout {
            if let Ok(connector) = scanout::connector() {
                return connector;
            }
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
        if self.native_scanout {
            if let Ok(count) = scanout::modes(output) {
                return count;
            }
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
        if !self.native_scanout {
            return Err(ModeSetError::RequiresReboot);
        }
        let bytes =
            frame_bytes(requested.width, requested.height).ok_or(ModeSetError::UnsupportedMode)?;
        let new_back = reserve_back_buffer(bytes).ok_or(ModeSetError::OutOfMemory)?;
        let new_background = reserve_back_buffer(bytes);
        if let Err(error) = scanout::set_mode(requested) {
            let _ = memory::free(new_back);
            if let Some(block) = new_background {
                let _ = memory::free(block);
            }
            return Err(match error {
                scanout::DisplayBrokerError::UnsupportedMode => ModeSetError::UnsupportedMode,
                scanout::DisplayBrokerError::OutOfMemory => ModeSetError::OutOfMemory,
                _ => ModeSetError::DeviceLost,
            });
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
        let old_drag_layer = self.drag_layer_block.take();
        self.back_block = new_back;
        self.back = new_back.phys as *mut u32;
        self.back_phys = new_back.phys;
        self.back_bytes = bytes;
        self.background_block = new_background;
        self.background = new_background
            .map(|block| block.phys as *mut u32)
            .unwrap_or_default();
        self.drag_layer = core::ptr::null_mut();
        self.drag_layer_width = 0;
        self.drag_layer_height = 0;
        self.width = requested.width;
        self.height = requested.height;
        self.stride = requested.width * 4;
        self.scanout_format = CpuPixelFormat::Bgr888;
        if matches!(
            self.render_format,
            CpuPixelFormat::Rgb888 | CpuPixelFormat::Bgr888
        ) {
            self.render_format = CpuPixelFormat::Bgr888;
        }
        let _ = memory::free(old_back);
        if let Some(block) = old_background {
            let _ = memory::free(block);
        }
        if let Some(block) = old_drag_layer {
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
        if let Some(block) = self.drag_layer_block.take() {
            let _ = memory::free(block);
        }
        self.back = core::ptr::null_mut();
        self.background = core::ptr::null_mut();
        self.drag_layer = core::ptr::null_mut();
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

//! Оконное системное приложение «Aurora 3D».
//!
//! Приложение владеет retained surface, но не GPU capability. Каждый кадр
//! запрашивается у изолированного `renderd`; при отсутствии VirGL тот же
//! surface заполняет bounded software renderer. Поэтому отказ GPU не ломает
//! desktop, а окно, taskbar и ввод остаются обычными объектами оконной системы.

use crate::{
    font,
    graphics::{Color, Framebuffer, Rect},
    process, serial,
};

pub const SURFACE_WIDTH: u32 = 800;
pub const SURFACE_HEIGHT: u32 = 450;
const SURFACE_PIXELS: usize = SURFACE_WIDTH as usize * SURFACE_HEIGHT as usize;
const GPU_FRAME_INTERVAL_MS: u64 = 16;
const SOFTWARE_FRAME_INTERVAL_MS: u64 = 33;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererBackend {
    Probing,
    Virgl,
    Software,
}

/// Независимое состояние одного экземпляра демо.
pub struct GpuDemo {
    pixels: [u32; SURFACE_PIXELS],
    frame: u32,
    last_frame_ms: u64,
    fps_epoch_ms: u64,
    fps_frames: u32,
    fps: u16,
    backend: RendererBackend,
    retry_after_frame: u32,
    backend_logged: bool,
}

impl GpuDemo {
    /// Инициализирует большой retained surface непосредственно в физических
    /// кадрах приложения, не создавая временный 1.4 MiB объект на kernel stack.
    ///
    /// # Safety
    /// `destination` указывает на уникальное неинициализированное хранилище
    /// размером `Self` с правильным выравниванием.
    pub unsafe fn initialize_in_place(destination: *mut Self, now_ms: u64) {
        // SAFETY: контракт функции гарантирует весь диапазон Self; pixels —
        // первый полностью принадлежащий destination field.
        unsafe {
            core::ptr::addr_of_mut!((*destination).pixels)
                .cast::<u32>()
                .write_bytes(0, SURFACE_PIXELS);
            core::ptr::addr_of_mut!((*destination).frame).write(0);
            core::ptr::addr_of_mut!((*destination).last_frame_ms)
                .write(now_ms.saturating_sub(GPU_FRAME_INTERVAL_MS));
            core::ptr::addr_of_mut!((*destination).fps_epoch_ms).write(now_ms);
            core::ptr::addr_of_mut!((*destination).fps_frames).write(0);
            core::ptr::addr_of_mut!((*destination).fps).write(0);
            core::ptr::addr_of_mut!((*destination).backend).write(RendererBackend::Probing);
            core::ptr::addr_of_mut!((*destination).retry_after_frame).write(0);
            core::ptr::addr_of_mut!((*destination).backend_logged).write(false);
        }
    }

    /// GPU получает 60 Hz pacing hint, а тяжёлый CPU fallback ограничивается
    /// 30 Hz. Оба пути запускаются только после input polling оконного сервера,
    /// поэтому renderer не может вытеснить уже ожидающее событие мыши.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        let frame_interval_ms = if self.backend == RendererBackend::Software {
            SOFTWARE_FRAME_INTERVAL_MS
        } else {
            GPU_FRAME_INTERVAL_MS
        };
        if now_ms.saturating_sub(self.last_frame_ms) < frame_interval_ms {
            return false;
        }
        self.last_frame_ms = now_ms;
        let should_probe_gpu = self.backend != RendererBackend::Software
            || self.frame.wrapping_sub(self.retry_after_frame) < 0x8000_0000;
        let gpu_rendered = should_probe_gpu
            && process::render_interactive_gpu_demo_frame(
                SURFACE_WIDTH,
                SURFACE_HEIGHT,
                self.frame,
                &mut self.pixels,
            )
            .is_ok();
        if gpu_rendered {
            self.backend = RendererBackend::Virgl;
        } else {
            self.backend = RendererBackend::Software;
            self.retry_after_frame = self.frame.wrapping_add(120);
            self.render_software_frame();
        }
        if !self.backend_logged {
            serial::put_str("[gpu-demo-window] ready backend=");
            serial::put_str(match self.backend {
                RendererBackend::Virgl => "virgl-host-gpu",
                RendererBackend::Software | RendererBackend::Probing => "cpu-fallback",
            });
            serial::put_str(" surface=800x450 windowed=yes pacing=");
            serial::put_str(if self.backend == RendererBackend::Virgl {
                "60hz"
            } else {
                "30hz"
            });
            serial::put_str(" preferred-blit=linear-1:1\n");
            self.backend_logged = true;
        }
        self.frame = self.frame.wrapping_add(1);
        self.fps_frames = self.fps_frames.saturating_add(1);
        let elapsed = now_ms.saturating_sub(self.fps_epoch_ms);
        if elapsed >= 1_000 {
            self.fps = ((u64::from(self.fps_frames) * 1_000) / elapsed).min(999) as u16;
            self.fps_frames = 0;
            self.fps_epoch_ms = now_ms;
        }
        true
    }

    pub fn draw(&self, framebuffer: &mut Framebuffer, content: Rect) {
        framebuffer.fill_rect(content, Color::rgb(8, 13, 24));
        let header_height = 52u32.min(content.height);
        framebuffer.fill_rect(
            Rect::new(content.x, content.y, content.width, header_height),
            Color::rgb(14, 23, 39),
        );
        font::draw_text(
            framebuffer,
            content.x + 16,
            content.y + 10,
            "Aurora 3D · OpenGL / OpenGL ES",
            Color::rgb(235, 244, 255),
            font::UI_SMALL.bold(),
        );
        font::draw_text(
            framebuffer,
            content.x + 16,
            content.y + 31,
            match self.backend {
                RendererBackend::Virgl => "GPU: VirGL → host renderer",
                RendererBackend::Software => "GPU недоступен · CPU fallback",
                RendererBackend::Probing => "Определение GPU…",
            },
            match self.backend {
                RendererBackend::Virgl => Color::rgb(91, 222, 171),
                RendererBackend::Software => Color::rgb(255, 187, 92),
                RendererBackend::Probing => Color::rgb(151, 179, 220),
            },
            font::UI_SMALL,
        );
        let mut fps_text = [0u8; 16];
        let fps = write_fps(&mut fps_text, self.fps);
        let metrics = font::measure_text(fps, font::UI_SMALL.bold());
        font::draw_text(
            framebuffer,
            content.right().saturating_sub(metrics.width as i32 + 18),
            content.y + 17,
            fps,
            Color::rgb(151, 205, 255),
            font::UI_SMALL.bold(),
        );

        let available = Rect::new(
            content.x + 12,
            content.y + header_height as i32 + 12,
            content.width.saturating_sub(24),
            content.height.saturating_sub(header_height + 24),
        );
        let destination = aspect_fit(available, SURFACE_WIDTH, SURFACE_HEIGHT);
        framebuffer.blit_bgrx_bilinear(&self.pixels, SURFACE_WIDTH, SURFACE_HEIGHT, destination);
        framebuffer.rounded_border(destination, 8, 1, Color::rgb(70, 93, 130));
    }

    /// Надёжный fallback: процедурная сцена с анимированным освещением,
    /// checker texture, параллакс-смещением и perturbation нормали. Он нужен
    /// не для подмены Mesa, а чтобы приложение оставалось полезным без GPU.
    fn render_software_frame(&mut self) {
        let width = SURFACE_WIDTH as i32;
        let height = SURFACE_HEIGHT as i32;
        let center_x = width / 2 + triangle_wave(self.frame, 180, 42);
        let center_y = height / 2 - 24;
        let radius = 132i32;
        let radius_squared = radius * radius;
        let light_x = triangle_wave(self.frame.wrapping_add(35), 240, 96);
        for y in 0..height {
            for x in 0..width {
                let horizon = height * 58 / 100;
                let mut red = 8 + y * 18 / height;
                let mut green = 14 + y * 30 / height;
                let mut blue = 35 + y * 70 / height;
                if y >= horizon {
                    let depth = y - horizon + 1;
                    let parallax_x = (x - width / 2) * 18 / depth.max(18);
                    let checker =
                        (((x + parallax_x + self.frame as i32) / 34) ^ ((y + depth * 2) / 22)) & 1;
                    red = 10 + checker * 14;
                    green = 25 + checker * 20;
                    blue = 50 + checker * 34;
                    let grid = (x + parallax_x).rem_euclid(34) < 2 || y.rem_euclid(22) < 2;
                    if grid {
                        green += 36;
                        blue += 55;
                    }
                }
                let dx = x - center_x;
                let dy = y - center_y;
                let distance = dx * dx + dy * dy;
                if distance < radius_squared {
                    let z = integer_sqrt((radius_squared - distance) as u32) as i32;
                    let bump = (((x + self.frame as i32 * 2) / 13) ^ (y / 11)) & 7;
                    let dot = (dx * light_x / 96 - dy * 58 / 96 + z * 82 / 96 + bump * 7).max(0);
                    let lighting = (34 + dot * 205 / radius).clamp(24, 255);
                    let band = ((x + z / 3 + self.frame as i32 * 2) / 22).rem_euclid(6);
                    let (base_r, base_g, base_b) = match band {
                        0 | 1 => (55, 139, 255),
                        2 | 3 => (128, 72, 245),
                        _ => (28, 211, 214),
                    };
                    red = base_r * lighting / 255;
                    green = base_g * lighting / 255;
                    blue = base_b * lighting / 255;
                    if distance > radius_squared - radius * 8 {
                        red = (red + 110).min(255);
                        green = (green + 120).min(255);
                        blue = 255;
                    }
                }
                self.pixels[y as usize * SURFACE_WIDTH as usize + x as usize] =
                    ((red.clamp(0, 255) as u32) << 16)
                        | ((green.clamp(0, 255) as u32) << 8)
                        | blue.clamp(0, 255) as u32;
            }
        }
    }
}

fn aspect_fit(bounds: Rect, width: u32, height: u32) -> Rect {
    if bounds.is_empty() || width == 0 || height == 0 {
        return Rect::EMPTY;
    }
    let fit_width = bounds.width;
    let fit_height = ((u64::from(fit_width) * u64::from(height)) / u64::from(width)) as u32;
    let (result_width, result_height) = if fit_height <= bounds.height {
        (fit_width, fit_height)
    } else {
        (
            ((u64::from(bounds.height) * u64::from(width)) / u64::from(height)) as u32,
            bounds.height,
        )
    };
    Rect::new(
        bounds.x + bounds.width.saturating_sub(result_width) as i32 / 2,
        bounds.y + bounds.height.saturating_sub(result_height) as i32 / 2,
        result_width,
        result_height,
    )
}

fn triangle_wave(frame: u32, period: u32, amplitude: i32) -> i32 {
    let phase = frame % period.max(2);
    let half = period / 2;
    let ramp = if phase < half { phase } else { period - phase };
    (ramp as i32 * amplitude * 2 / half.max(1) as i32) - amplitude
}

fn integer_sqrt(value: u32) -> u32 {
    let mut result = 0u32;
    let mut bit = 1u32 << 30;
    while bit > value {
        bit >>= 2;
    }
    let mut remainder = value;
    while bit != 0 {
        if remainder >= result + bit {
            remainder -= result + bit;
            result = (result >> 1) + bit;
        } else {
            result >>= 1;
        }
        bit >>= 2;
    }
    result
}

fn write_fps(buffer: &mut [u8; 16], value: u16) -> &str {
    buffer[..5].copy_from_slice(b"FPS: ");
    let mut digits = [0u8; 3];
    let mut count = 0usize;
    let mut value = u32::from(value);
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 || count == digits.len() {
            break;
        }
    }
    for index in 0..count {
        buffer[5 + index] = digits[count - index - 1];
    }
    core::str::from_utf8(&buffer[..5 + count]).unwrap_or("FPS")
}

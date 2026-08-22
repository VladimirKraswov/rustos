//! Временный системный adapter `Framebuffer API -> GPU UI stream`.
//!
//! Приложения и SystemUI не знают, какой backend выбран. Пока оконный сервер
//! переносится из bootstrap kernel в `uid`, этот модуль записывает те же
//! высокоуровневые spans/quads, которые CPU fallback отправил бы в RAM. Ни
//! один pixel при активном GPU-сеансе здесь не записывается: rasterization и
//! blend выполняет ring-3 `renderd`, затем готовый GraphicsBuffer напрямую
//! проходит через compositord/displayd.

use rustos_abi::gpu::{gpu_ui_checksum, GpuUiFrameHeader, GpuUiQuad, GPU_UI_STREAM_BYTES};
use rustos_video::{Color, Rect};

const HEADER_BYTES: usize = core::mem::size_of::<GpuUiFrameHeader>();
pub(crate) const MAX_QUADS: usize =
    (GPU_UI_STREAM_BYTES - HEADER_BYTES) / core::mem::size_of::<GpuUiQuad>();

struct Recorder {
    width: u32,
    height: u32,
    frame_id: u64,
    quads: [GpuUiQuad; MAX_QUADS],
    len: usize,
    overflowed: bool,
}

impl Recorder {
    const fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            frame_id: 0,
            // Полностью нулевой массив остаётся в `.bss`, а не раздувает
            // kernel image на мегабайт и всё равно перезаписывается до чтения.
            quads: [GpuUiQuad {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                colors: [0; 4],
                flags: 0,
                reserved: 0,
            }; MAX_QUADS],
            len: 0,
            overflowed: false,
        }
    }
}

// GUI session принадлежит CPU0 и является единственным writer. После
// переноса windowd в ring 3 это статическое хранилище исчезнет вместе с
// kernel adapter'ом; сейчас оно не раздувает маленький kernel stack.
static mut RECORDER: Recorder = Recorder::new();

pub(crate) fn begin(width: u32, height: u32) {
    let recorder = unsafe { &mut *core::ptr::addr_of_mut!(RECORDER) };
    recorder.width = width;
    recorder.height = height;
    recorder.frame_id = recorder.frame_id.wrapping_add(1).max(1);
    recorder.len = 0;
    recorder.overflowed = false;
}

pub(crate) fn solid(rect: Rect, color: Color, alpha: u8) {
    let packed = premultiplied_rgba(color, alpha);
    quad(rect, [packed; 4], 0);
}

pub(crate) fn gradient(rect: Rect, colors: [Color; 4]) {
    quad(
        rect,
        colors.map(|color| premultiplied_rgba(color, u8::MAX)),
        0,
    );
}

/// Ссылка на полноразмерную системную texture. Ring-3 renderer лениво
/// загружает её по стабильному id и переиспользует между кадрами.
pub(crate) fn wallpaper(rect: Rect, id: u32) {
    quad(
        rect,
        [id, 0, 0, 0],
        rustos_abi::gpu::ui_quad_flag::WALLPAPER_TEXTURE,
    );
}

fn quad(rect: Rect, colors: [u32; 4], flags: u32) {
    let recorder = unsafe { &mut *core::ptr::addr_of_mut!(RECORDER) };
    let bounds = Rect::new(0, 0, recorder.width, recorder.height);
    let visible = rect.intersection(bounds);
    if visible.is_empty() {
        return;
    }
    let Ok(x) = u16::try_from(visible.x) else {
        recorder.overflowed = true;
        return;
    };
    let Ok(y) = u16::try_from(visible.y) else {
        recorder.overflowed = true;
        return;
    };
    let Ok(width) = u16::try_from(visible.width) else {
        recorder.overflowed = true;
        return;
    };
    let Ok(height) = u16::try_from(visible.height) else {
        recorder.overflowed = true;
        return;
    };

    // Некоторые bitmap fonts обходят glyph по столбцам, другие по строкам.
    // Ищем соседний span не только в последней команде, но не переносим его
    // через более поздний перекрывающий primitive: painter order сохраняется.
    let search_start = recorder.len.saturating_sub(512);
    for index in (search_start..recorder.len).rev() {
        let candidate = recorder.quads[index];
        if candidate.colors != colors || candidate.flags != flags || flags != 0 {
            continue;
        }
        let horizontal = candidate.y == y
            && candidate.height == height
            && u32::from(candidate.x) + u32::from(candidate.width) == u32::from(x);
        let vertical = candidate.x == x
            && candidate.width == width
            && u32::from(candidate.y) + u32::from(candidate.height) == u32::from(y);
        if !horizontal && !vertical {
            continue;
        }
        let mut merged = candidate;
        if horizontal {
            let Some(value) = u32::from(candidate.width)
                .checked_add(u32::from(width))
                .and_then(|value| u16::try_from(value).ok())
            else {
                continue;
            };
            merged.width = value;
        } else {
            let Some(value) = u32::from(candidate.height)
                .checked_add(u32::from(height))
                .and_then(|value| u16::try_from(value).ok())
            else {
                continue;
            };
            merged.height = value;
        }
        if recorder.quads[index + 1..recorder.len]
            .iter()
            .any(|later| quads_overlap(merged, *later))
        {
            continue;
        }
        recorder.quads[index] = merged;
        return;
    }

    let Some(slot) = recorder.quads.get_mut(recorder.len) else {
        recorder.overflowed = true;
        return;
    };
    *slot = GpuUiQuad {
        x,
        y,
        width,
        height,
        colors,
        flags,
        reserved: 0,
    };
    recorder.len += 1;
}

fn quads_overlap(first: GpuUiQuad, second: GpuUiQuad) -> bool {
    u32::from(first.x) < u32::from(second.x) + u32::from(second.width)
        && u32::from(second.x) < u32::from(first.x) + u32::from(first.width)
        && u32::from(first.y) < u32::from(second.y) + u32::from(second.height)
        && u32::from(second.y) < u32::from(first.y) + u32::from(first.height)
}

pub(crate) fn finish() -> Option<(GpuUiFrameHeader, &'static [GpuUiQuad])> {
    let recorder = unsafe { &*core::ptr::addr_of!(RECORDER) };
    if recorder.overflowed || recorder.len == 0 {
        return None;
    }
    let quads = &recorder.quads[..recorder.len];
    let mut header = GpuUiFrameHeader::new(
        recorder.width,
        recorder.height,
        recorder.frame_id,
        recorder.len as u32,
    );
    header.checksum = gpu_ui_checksum(quads);
    Some((header, quads))
}

fn premultiplied_rgba(color: Color, alpha: u8) -> u32 {
    let scale = |channel: u8| ((u16::from(channel) * u16::from(alpha) + 127) / 255) as u8;
    u32::from(scale(color.r))
        | (u32::from(scale(color.g)) << 8)
        | (u32::from(scale(color.b)) << 16)
        | (u32::from(alpha) << 24)
}

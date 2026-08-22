//! Временный системный adapter `Framebuffer API -> GPU UI stream`.
//!
//! Приложения и SystemUI не знают, какой backend выбран. Пока оконный сервер
//! переносится из bootstrap kernel в `uid`, этот модуль записывает те же
//! высокоуровневые spans/quads, которые CPU fallback отправил бы в RAM. Ни
//! один pixel при активном GPU-сеансе здесь не записывается: rasterization и
//! blend выполняет ring-3 `renderd`, затем готовый GraphicsBuffer напрямую
//! проходит через compositord/displayd.

use rustos_abi::gpu::{
    gpu_ui_checksum, gpu_ui_content_hash, GpuUiFrameHeader, GpuUiLayer, GpuUiQuad,
    GPU_UI_FRAME_STREAM_BYTES,
};
use rustos_video::{Color, Rect};

const HEADER_BYTES: usize = core::mem::size_of::<GpuUiFrameHeader>();
pub(crate) const MAX_LAYERS: usize = 32;
pub(crate) const MAX_QUADS: usize =
    (GPU_UI_FRAME_STREAM_BYTES - HEADER_BYTES - MAX_LAYERS * core::mem::size_of::<GpuUiLayer>())
        / core::mem::size_of::<GpuUiQuad>();

struct Recorder {
    width: u32,
    height: u32,
    frame_id: u64,
    layers: [GpuUiLayer; MAX_LAYERS],
    layer_len: usize,
    current_layer: Option<usize>,
    layer_bounds: Rect,
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
            layers: [GpuUiLayer {
                id: 0,
                content_hash: 0,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                first_quad: 0,
                quad_count: 0,
                flags: 0,
                reserved_header: 0,
                reserved: [0; 2],
            }; MAX_LAYERS],
            layer_len: 0,
            current_layer: None,
            layer_bounds: Rect::new(0, 0, 0, 0),
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
static mut TRANSFORM_LAYERS: [GpuUiLayer; MAX_LAYERS] = [GpuUiLayer {
    id: 0,
    content_hash: 0,
    x: 0,
    y: 0,
    width: 0,
    height: 0,
    first_quad: 0,
    quad_count: 0,
    flags: 0,
    reserved_header: 0,
    reserved: [0; 2],
}; MAX_LAYERS];

pub(crate) fn begin(width: u32, height: u32) {
    let recorder = unsafe { &mut *core::ptr::addr_of_mut!(RECORDER) };
    recorder.width = width;
    recorder.height = height;
    recorder.frame_id = recorder.frame_id.wrapping_add(1).max(1);
    recorder.layer_len = 0;
    recorder.current_layer = None;
    recorder.layer_bounds = Rect::new(0, 0, 0, 0);
    recorder.len = 0;
    recorder.overflowed = false;
}

/// Начинает независимую GPU surface. Bounds сразу обрезаются экраном, а все
/// последующие primitives переводятся из screen-space в локальные координаты
/// слоя. `id` обязан быть устойчивым между кадрами.
pub(crate) fn begin_layer(id: u64, bounds: Rect, flags: u32) {
    finish_layer();
    let recorder = unsafe { &mut *core::ptr::addr_of_mut!(RECORDER) };
    let screen = Rect::new(0, 0, recorder.width, recorder.height);
    let bounds = bounds.intersection(screen);
    if id == 0 || bounds.is_empty() || recorder.layer_len >= MAX_LAYERS {
        recorder.overflowed = true;
        return;
    }
    let index = recorder.layer_len;
    recorder.layers[index] = GpuUiLayer {
        id,
        content_hash: 0,
        x: bounds.x,
        y: bounds.y,
        width: bounds.width,
        height: bounds.height,
        first_quad: recorder.len as u32,
        quad_count: 0,
        flags,
        reserved_header: 0,
        reserved: [0; 2],
    };
    recorder.layer_len += 1;
    recorder.current_layer = Some(index);
    recorder.layer_bounds = bounds;
}

/// Завершает текущий слой и вычисляет hash только его неизменяемого
/// содержимого. Пустой слой удаляется: это удобно для hardware cursor plane.
pub(crate) fn finish_layer() {
    let recorder = unsafe { &mut *core::ptr::addr_of_mut!(RECORDER) };
    let Some(index) = recorder.current_layer.take() else {
        return;
    };
    let first = recorder.layers[index].first_quad as usize;
    let count = recorder.len.saturating_sub(first);
    if count == 0 {
        recorder.layer_len = recorder.layer_len.saturating_sub(1);
        return;
    }
    recorder.layers[index].quad_count = count as u32;
    recorder.layers[index].content_hash =
        gpu_ui_content_hash(&recorder.quads[first..first + count]);
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

/// Записывает системный 3D Canvas без пикселей и GPU handles. Renderd сам
/// выбирает backend, создаёт surface и смешивает готовую сцену в слой окна.
pub(crate) fn aurora_canvas(rect: Rect, instance_id: u32, scene_frame: u32) {
    quad(
        rect,
        [scene_frame, instance_id, 0, 0],
        rustos_abi::gpu::ui_quad_flag::CANVAS_3D,
    );
}

fn quad(rect: Rect, colors: [u32; 4], flags: u32) {
    let recorder = unsafe { &mut *core::ptr::addr_of_mut!(RECORDER) };
    let Some(layer_index) = recorder.current_layer else {
        recorder.overflowed = true;
        return;
    };
    let visible = rect.intersection(recorder.layer_bounds);
    if visible.is_empty() {
        return;
    }
    let Ok(x) = u16::try_from(visible.x.saturating_sub(recorder.layer_bounds.x)) else {
        recorder.overflowed = true;
        return;
    };
    let Ok(y) = u16::try_from(visible.y.saturating_sub(recorder.layer_bounds.y)) else {
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
    let search_start =
        (recorder.layers[layer_index].first_quad as usize).max(recorder.len.saturating_sub(512));
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

pub(crate) fn finish() -> Option<(
    GpuUiFrameHeader,
    &'static [GpuUiLayer],
    &'static [GpuUiQuad],
)> {
    finish_layer();
    let recorder = unsafe { &*core::ptr::addr_of!(RECORDER) };
    if recorder.overflowed || recorder.layer_len == 0 || recorder.len == 0 {
        return None;
    }
    let layers = &recorder.layers[..recorder.layer_len];
    let quads = &recorder.quads[..recorder.len];
    let mut header = GpuUiFrameHeader::new(
        recorder.width,
        recorder.height,
        recorder.frame_id,
        recorder.layer_len as u32,
        recorder.len as u32,
    );
    header.checksum = gpu_ui_checksum(layers, quads);
    Some((header, layers, quads))
}

/// Создаёт следующий кадр, меняя только transform уже готовой surface.
/// Содержимое и quad range остаются побитно неизменными. Если окно оказалось
/// частично за экраном и размер clipping bounds изменился, вызывающий обязан
/// выполнить обычный redraw.
pub(crate) fn transform_layer(
    id: u64,
    bounds: Rect,
) -> Option<(
    GpuUiFrameHeader,
    &'static [GpuUiLayer],
    &'static [GpuUiQuad],
)> {
    let recorder = unsafe { &mut *core::ptr::addr_of_mut!(RECORDER) };
    if recorder.overflowed || recorder.current_layer.is_some() {
        return None;
    }
    let screen = Rect::new(0, 0, recorder.width, recorder.height);
    let bounds = bounds.intersection(screen);
    let layer = recorder.layers[..recorder.layer_len]
        .iter_mut()
        .find(|layer| layer.id == id)?;
    if bounds.is_empty() || layer.width != bounds.width || layer.height != bounds.height {
        return None;
    }
    layer.x = bounds.x;
    layer.y = bounds.y;
    recorder.frame_id = recorder.frame_id.wrapping_add(1).max(1);
    let transform_layers = unsafe { &mut *core::ptr::addr_of_mut!(TRANSFORM_LAYERS) };
    for (target, source) in transform_layers[..recorder.layer_len]
        .iter_mut()
        .zip(&recorder.layers[..recorder.layer_len])
    {
        *target = *source;
        target.first_quad = 0;
        target.quad_count = 0;
    }
    let layers = &transform_layers[..recorder.layer_len];
    let quads = &[];
    let mut header = GpuUiFrameHeader::new_transform(
        recorder.width,
        recorder.height,
        recorder.frame_id,
        layers.len() as u32,
    );
    header.checksum = gpu_ui_checksum(layers, quads);
    Some((header, layers, quads))
}

fn premultiplied_rgba(color: Color, alpha: u8) -> u32 {
    let scale = |channel: u8| ((u16::from(channel) * u16::from(alpha) + 127) / 255) as u8;
    u32::from(scale(color.r))
        | (u32::from(scale(color.g)) << 8)
        | (u32::from(scale(color.b)) << 16)
        | (u32::from(alpha) << 24)
}

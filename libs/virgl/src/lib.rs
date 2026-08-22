//! Минимальный безопасный encoder VirGL command stream.
//!
//! Это низкоуровневый winsys transport, а не реализация OpenGL. Модуль
//! сериализует небольшой, строго ограниченный набор Gallium/VirGL команд;
//! выбор сцены, математика и Mesa-like state tracking живут уровнем выше в
//! `rustos-mesa`. Такое разделение оставляет kernel только валидатором и
//! транспортом команд, а не переносит графическую политику в TCB.

#![no_std]

const CCMD_CREATE_OBJECT: u8 = 1;
const CCMD_BIND_OBJECT: u8 = 2;
const CCMD_SET_VIEWPORT_STATE: u8 = 4;
const CCMD_SET_FRAMEBUFFER_STATE: u8 = 5;
const CCMD_SET_VERTEX_BUFFERS: u8 = 6;
const CCMD_CLEAR: u8 = 7;
const CCMD_DRAW_VBO: u8 = 8;
const CCMD_RESOURCE_INLINE_WRITE: u8 = 9;
const CCMD_BLIT: u8 = 16;
const CCMD_BIND_SHADER: u8 = 31;
const CCMD_LINK_SHADER: u8 = 52;

const OBJECT_BLEND: u8 = 1;
const OBJECT_RASTERIZER: u8 = 2;
const OBJECT_DSA: u8 = 3;
const OBJECT_SHADER: u8 = 4;
const OBJECT_VERTEX_ELEMENTS: u8 = 5;
const OBJECT_SURFACE: u8 = 8;

const SHADER_VERTEX: u32 = 0;
const SHADER_FRAGMENT: u32 = 1;
const PRIM_TRIANGLES: u32 = 4;
/// VirGL format code для непрозрачных BGRX8888 surfaces.
pub const FORMAT_BGRX8888: u32 = 2;
/// VirGL format code для premultiplied BGRA8888 surfaces compositor'а.
pub const FORMAT_BGRA8888: u32 = 1;
/// VirGL format code одноканального glyph atlas.
pub const FORMAT_R8_UNORM: u32 = 64;
const FORMAT_RGBA32_FLOAT: u32 = 31;
const CLEAR_COLOR0: u32 = 1 << 2;

const VERTEX_SHADER: &[u8] = b"VERT\n\
DCL IN[0]\n\
DCL IN[1]\n\
DCL OUT[0], POSITION\n\
DCL OUT[1], COLOR\n\
  0: MOV OUT[1], IN[1]\n\
  1: MOV OUT[0], IN[0]\n\
  2: END\n\0";

const FRAGMENT_SHADER: &[u8] = b"FRAG\n\
DCL IN[0], COLOR, LINEAR\n\
DCL OUT[0], COLOR\n\
IMM[0] FLT32 { 0.9400, 0.9400, 0.9400, 1.0000 }\n\
IMM[1] FLT32 { 0.0150, 0.0200, 0.0450, 0.0000 }\n\
  0: MAD OUT[0], IN[0], IMM[0], IMM[1]\n\
  1: END\n\0";

/// Одна interleaved вершина bootstrap Gallium pipeline.
///
/// Координаты уже находятся в clip space. Цвет содержит результат расчёта
/// освещения state tracker'ом; fragment shader выполняет финальный tone pass.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    /// Homogeneous clip-space position.
    pub position: [f32; 4],
    /// Linear RGBA color.
    pub color: [f32; 4],
}

impl Vertex {
    /// Создаёт вершину без скрытого преобразования координат.
    pub const fn new(position: [f32; 4], color: [f32; 4]) -> Self {
        Self { position, color }
    }
}

/// Ошибка формирования bounded command buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// Выходной массив слишком мал.
    BufferTooSmall,
    /// Размер render target не представим корректным viewport.
    InvalidExtent,
    /// Поток поддерживает только непустой список полных треугольников.
    InvalidVertexCount,
    /// Список compositor layers превышает bounded batch.
    TooManyLayers,
    /// Texture upload не соответствует заявленной геометрии.
    InvalidUpload,
}

/// Неотрицательный physical rectangle для GPU blit/scissor.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlitRect {
    /// Левая координата.
    pub x: u32,
    /// Верхняя координата.
    pub y: u32,
    /// Ширина.
    pub width: u32,
    /// Высота.
    pub height: u32,
}

impl BlitRect {
    /// Создаёт rectangle без скрытого clipping.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn valid_within(self, width: u32, height: u32) -> bool {
        self.width != 0
            && self.height != 0
            && self
                .x
                .checked_add(self.width)
                .is_some_and(|right| right <= width)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|bottom| bottom <= height)
    }
}

/// Один уже готовый GPU surface в композиционном порядке снизу вверх.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompositeLayer {
    /// VirGL resource исходной поверхности.
    pub resource: u32,
    /// Формат resource.
    pub format: u32,
    /// Область source texture.
    pub source: BlitRect,
    /// Область scanout target.
    pub destination: BlitRect,
    /// Использовать bilinear sampling при масштабировании.
    pub linear_filter: bool,
    /// Смешивать premultiplied alpha поверх уже собранных нижних слоёв.
    pub alpha_blend: bool,
}

impl CompositeLayer {
    /// Обычный непрозрачный слой без масштабирования.
    pub const fn opaque(resource: u32, format: u32, rect: BlitRect) -> Self {
        Self {
            resource,
            format,
            source: BlitRect::new(0, 0, rect.width, rect.height),
            destination: rect,
            linear_filter: false,
            alpha_blend: false,
        }
    }
}

/// Кодирует один bounded pass GPU-композиции.
///
/// `damage` становится аппаратным scissor. Команда никогда не читает pixels
/// обратно в guest: каждый source resource смешивается непосредственно в
/// `target_resource`. Полностью закрытые слои должен заранее удалить
/// `compositord`, потому что именно он владеет оконной политикой.
pub fn encode_composite_pass(
    output: &mut [u32],
    target_width: u32,
    target_height: u32,
    target_resource: u32,
    target_format: u32,
    damage: BlitRect,
    layers: &[CompositeLayer],
) -> Result<usize, EncodeError> {
    // 512 BLIT records занимают ~44 KiB и укладываются в negotiated 60 KiB
    // transport. Большой batch критичен для UI: один fence обслуживает сотни
    // spans, а не каждые несколько букв отдельно.
    const MAX_LAYERS_PER_BATCH: usize = 512;
    if target_resource == 0
        || target_width == 0
        || target_height == 0
        || target_width > 16_384
        || target_height > 16_384
        || !damage.valid_within(target_width, target_height)
    {
        return Err(EncodeError::InvalidExtent);
    }
    if layers.is_empty() || layers.len() > MAX_LAYERS_PER_BATCH {
        return Err(EncodeError::TooManyLayers);
    }
    let mut encoder = Encoder::new(output);
    for layer in layers {
        if layer.resource == 0
            || !layer.destination.valid_within(target_width, target_height)
            || layer.source.width == 0
            || layer.source.height == 0
        {
            return Err(EncodeError::InvalidExtent);
        }
        let filter = u32::from(layer.linear_filter);
        let alpha = u32::from(layer.alpha_blend);
        encoder.command(CCMD_BLIT, 0, 21)?;
        encoder.words(&[
            0x0f | (filter << 8) | (1 << 10) | (alpha << 12),
            pack_xy(damage.x, damage.y),
            pack_xy(
                damage.x.saturating_add(damage.width),
                damage.y.saturating_add(damage.height),
            ),
            target_resource,
            0,
            target_format,
            layer.destination.x,
            layer.destination.y,
            0,
            layer.destination.width,
            layer.destination.height,
            1,
            layer.resource,
            0,
            layer.format,
            layer.source.x,
            layer.source.y,
            0,
            layer.source.width,
            layer.source.height,
            1,
        ])?;
    }
    Ok(encoder.finish())
}

/// Загружает tightly-packed часть atlas в device-side texture.
///
/// Большой atlas разбивается вызывающим кодом на строки/tiles, каждый из
/// которых укладывается в `GPU_MAX_COMMAND_BYTES`. Такая схема выполняется
/// при изменении cache и не создаёт full-frame upload в steady state.
pub fn encode_texture_upload(
    output: &mut [u32],
    texture_resource: u32,
    rect: BlitRect,
    bytes_per_pixel: u8,
    data: &[u8],
) -> Result<usize, EncodeError> {
    if texture_resource == 0 || rect.width == 0 || rect.height == 0 {
        return Err(EncodeError::InvalidUpload);
    }
    let stride = rect
        .width
        .checked_mul(u32::from(bytes_per_pixel))
        .filter(|_| matches!(bytes_per_pixel, 1 | 4))
        .ok_or(EncodeError::InvalidUpload)?;
    let expected = stride
        .checked_mul(rect.height)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(EncodeError::InvalidUpload)?;
    if data.len() != expected {
        return Err(EncodeError::InvalidUpload);
    }
    let data_dwords = data.len().div_ceil(4);
    let payload = u16::try_from(11usize.saturating_add(data_dwords))
        .map_err(|_| EncodeError::BufferTooSmall)?;
    let mut encoder = Encoder::new(output);
    encoder.command(CCMD_RESOURCE_INLINE_WRITE, 0, payload)?;
    encoder.words(&[
        texture_resource,
        0,
        0,
        stride,
        stride.saturating_mul(rect.height),
        rect.x,
        rect.y,
        0,
        rect.width,
        rect.height,
        1,
    ])?;
    encoder.bytes(data)?;
    Ok(encoder.finish())
}

fn pack_xy(x: u32, y: u32) -> u32 {
    debug_assert!(x <= u16::MAX.into() && y <= u16::MAX.into());
    (x & 0xffff) | ((y & 0xffff) << 16)
}

/// Строит один полный кадр: тёмный фон и RGB-треугольник.
///
/// `color_resource` — импортированный `GraphicsBuffer`, `vertex_resource` —
/// context-local `PIPE_BUFFER`. Возвращаемое число измеряется в dwords.
pub fn encode_triangle(
    output: &mut [u32],
    width: u32,
    height: u32,
    color_resource: u32,
    vertex_resource: u32,
) -> Result<usize, EncodeError> {
    const VERTICES: [Vertex; 3] = [
        Vertex::new([-0.72, -0.68, 0.0, 1.0], [0.96, 0.24, 0.34, 1.0]),
        Vertex::new([0.72, -0.68, 0.0, 1.0], [0.16, 0.72, 0.98, 1.0]),
        Vertex::new([0.0, 0.72, 0.0, 1.0], [0.40, 0.93, 0.52, 1.0]),
    ];
    encode_mesh(
        output,
        width,
        height,
        color_resource,
        vertex_resource,
        &VERTICES,
        [0.035, 0.055, 0.11, 1.0],
    )
}

/// Кодирует один GPU frame с произвольным bounded triangle mesh.
///
/// Функция не хранит состояние и не разыменовывает GPU addresses. Именно этот
/// узкий API реализует transport-часть winsys для `rustos-mesa`.
pub fn encode_mesh(
    output: &mut [u32],
    width: u32,
    height: u32,
    color_resource: u32,
    vertex_resource: u32,
    vertices: &[Vertex],
    clear: [f32; 4],
) -> Result<usize, EncodeError> {
    encode_mesh_with_pipeline(
        output,
        width,
        height,
        color_resource,
        vertex_resource,
        vertices,
        clear,
        1,
        true,
        true,
        true,
    )
}

/// Кодирует следующий кадр, повторно используя созданные первым submission
/// immutable objects. Повторный `CREATE_OBJECT` с тем же handle запрещён
/// VirGL, поэтому многокадровый renderer обязан явно выбрать этот путь.
pub fn encode_mesh_update(
    output: &mut [u32],
    width: u32,
    height: u32,
    color_resource: u32,
    vertex_resource: u32,
    vertices: &[Vertex],
    clear: [f32; 4],
) -> Result<usize, EncodeError> {
    encode_mesh_with_pipeline(
        output,
        width,
        height,
        color_resource,
        vertex_resource,
        vertices,
        clear,
        1,
        false,
        false,
        true,
    )
}

/// Кодирует кадр swapchain: каждый render target получает собственный
/// VirGL surface handle, а framebuffer binding меняется без пересоздания
/// shaders и immutable pipeline objects.
#[allow(clippy::too_many_arguments)]
pub fn encode_mesh_swapchain(
    output: &mut [u32],
    width: u32,
    height: u32,
    color_resource: u32,
    vertex_resource: u32,
    vertices: &[Vertex],
    clear: [f32; 4],
    surface_handle: u32,
    initialize_surface: bool,
    initialize_pipeline: bool,
) -> Result<usize, EncodeError> {
    encode_mesh_swapchain_pass(
        output,
        width,
        height,
        color_resource,
        vertex_resource,
        vertices,
        clear,
        surface_handle,
        initialize_surface,
        initialize_pipeline,
        true,
    )
}

/// Вариант swapchain encoder для нескольких ordered batches одного кадра.
/// Только первый batch передаёт `clear_target=true`; остальные сохраняют уже
/// нарисованные primitives и дополняют тот же render target.
#[allow(clippy::too_many_arguments)]
pub fn encode_mesh_swapchain_pass(
    output: &mut [u32],
    width: u32,
    height: u32,
    color_resource: u32,
    vertex_resource: u32,
    vertices: &[Vertex],
    clear: [f32; 4],
    surface_handle: u32,
    initialize_surface: bool,
    initialize_pipeline: bool,
    clear_target: bool,
) -> Result<usize, EncodeError> {
    // 2..=7 заняты immutable pipeline objects. Остальные bounded handles
    // позволяют одному context держать независимые desktop и app surfaces.
    if surface_handle == 0 || (2..=7).contains(&surface_handle) || surface_handle > 63 {
        return Err(EncodeError::InvalidExtent);
    }
    encode_mesh_with_pipeline(
        output,
        width,
        height,
        color_resource,
        vertex_resource,
        vertices,
        clear,
        surface_handle,
        initialize_surface,
        initialize_pipeline,
        clear_target,
    )
}

#[allow(clippy::too_many_arguments)]
fn encode_mesh_with_pipeline(
    output: &mut [u32],
    width: u32,
    height: u32,
    color_resource: u32,
    vertex_resource: u32,
    vertices: &[Vertex],
    clear: [f32; 4],
    surface_handle: u32,
    initialize_surface: bool,
    initialize_pipeline: bool,
    clear_target: bool,
) -> Result<usize, EncodeError> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(EncodeError::InvalidExtent);
    }
    if vertices.is_empty() || !vertices.len().is_multiple_of(3) || vertices.len() > 16_320 {
        return Err(EncodeError::InvalidVertexCount);
    }
    let mut encoder = Encoder::new(output);

    // Surface object 1 связывает VirGL render target с capability-backed
    // resource. Уровень и диапазон layers равны нулю: это обычная 2D texture.
    if initialize_surface {
        encoder.command(CCMD_CREATE_OBJECT, OBJECT_SURFACE, 5)?;
        encoder.words(&[surface_handle, color_resource, FORMAT_BGRX8888, 0, 0])?;
    }
    // Binding входит в каждый submission: три surface могут быть in-flight,
    // а последующий кадр выбирает свой back buffer без копии/readback.
    encoder.command(CCMD_SET_FRAMEBUFFER_STATE, 0, 3)?;
    encoder.words(&[1, 0, surface_handle])?;

    // Фон очищает host 3D renderer. Guest CPU не пишет ни одного пикселя.
    if clear_target {
        encoder.command(CCMD_CLEAR, 0, 8)?;
        encoder.words(&[
            CLEAR_COLOR0,
            clear[0].to_bits(),
            clear[1].to_bits(),
            clear[2].to_bits(),
            clear[3].to_bits(),
            0,
            0,
            0,
        ])?;
    }

    // Две RGBA32F вершины на vertex: position и interpolated color.
    if initialize_pipeline {
        encoder.command(CCMD_CREATE_OBJECT, OBJECT_VERTEX_ELEMENTS, 9)?;
        encoder.words(&[
            2,
            0,
            0,
            0,
            FORMAT_RGBA32_FLOAT,
            16,
            0,
            0,
            FORMAT_RGBA32_FLOAT,
        ])?;
        encoder.command(CCMD_BIND_OBJECT, OBJECT_VERTEX_ELEMENTS, 1)?;
        encoder.word(2)?;
    }

    // Inline-write передаёт только 3D vertex input. Это не rasterization:
    // покрытие, интерполяцию и запись framebuffer выполняет VirGL renderer.
    // Длина одной VirGL-команды кодируется u16 dwords. Большой UI batch
    // поэтому загружается несколькими последовательными box writes в один
    // VBO, а DRAW_VBO остаётся единственным. Контекст гарантирует порядок:
    // GPU не увидит частично заполненный vertex buffer.
    const MAX_INLINE_VERTICES: usize = 8_000;
    for (chunk_index, chunk) in vertices.chunks(MAX_INLINE_VERTICES).enumerate() {
        let chunk_dwords =
            u16::try_from(chunk.len() * 8).map_err(|_| EncodeError::BufferTooSmall)?;
        let byte_offset = u32::try_from(chunk_index * MAX_INLINE_VERTICES * 32)
            .map_err(|_| EncodeError::BufferTooSmall)?;
        let chunk_bytes = u32::try_from(core::mem::size_of_val(chunk))
            .map_err(|_| EncodeError::BufferTooSmall)?;
        encoder.command(CCMD_RESOURCE_INLINE_WRITE, 0, 11 + chunk_dwords)?;
        encoder.words(&[
            vertex_resource,
            0,
            0,
            chunk_bytes,
            0,
            byte_offset,
            0,
            0,
            chunk_bytes,
            1,
            1,
        ])?;
        for vertex in chunk {
            for component in vertex.position.into_iter().chain(vertex.color) {
                encoder.word(component.to_bits())?;
            }
        }
    }
    encoder.command(CCMD_SET_VERTEX_BUFFERS, 0, 3)?;
    encoder.words(&[32, 0, vertex_resource])?;

    if initialize_pipeline {
        encoder.shader(3, SHADER_VERTEX, VERTEX_SHADER)?;
        encoder.shader(4, SHADER_FRAGMENT, FRAGMENT_SHADER)?;
        encoder.command(CCMD_LINK_SHADER, 0, 6)?;
        encoder.words(&[3, 4, 0, 0, 0, 0])?;

        // Стандартные immutable pipeline states из reference virglrenderer test.
        encoder.command(CCMD_CREATE_OBJECT, OBJECT_BLEND, 11)?;
        // Premultiplied alpha: src=ONE, dst=ONE_MINUS_SRC_ALPHA для RGB и
        // alpha, ADD, RGBA colormask. Ранее здесь был только colormask
        // 0x7800_0000 с выключенным blend_enable — именно поэтому AA coverage
        // шрифта превращался в чёрные контуры на GPU desktop.
        encoder.words(&[5, 0, 0, 0x7cc2_2611, 0, 0, 0, 0, 0, 0, 0])?;
        encoder.command(CCMD_BIND_OBJECT, OBJECT_BLEND, 1)?;
        encoder.word(5)?;

        encoder.command(CCMD_CREATE_OBJECT, OBJECT_DSA, 5)?;
        encoder.words(&[6, 6, 0, 0, 0])?;
        encoder.command(CCMD_BIND_OBJECT, OBJECT_DSA, 1)?;
        encoder.word(6)?;

        encoder.command(CCMD_CREATE_OBJECT, OBJECT_RASTERIZER, 9)?;
        encoder.words(&[
            7,
            (1 << 1) | (1 << 29) | (1 << 30),
            1.0f32.to_bits(),
            0,
            0,
            1.0f32.to_bits(),
            0,
            0,
            0,
        ])?;
        encoder.command(CCMD_BIND_OBJECT, OBJECT_RASTERIZER, 1)?;
        encoder.word(7)?;
    }

    let half_width = width as f32 * 0.5;
    let half_height = height as f32 * 0.5;
    encoder.command(CCMD_SET_VIEWPORT_STATE, 0, 7)?;
    encoder.words(&[
        0,
        half_width.to_bits(),
        half_height.to_bits(),
        0.5f32.to_bits(),
        half_width.to_bits(),
        half_height.to_bits(),
        0.5f32.to_bits(),
    ])?;

    encoder.command(CCMD_DRAW_VBO, 0, 12)?;
    encoder.words(&[
        0,
        vertices.len() as u32,
        PRIM_TRIANGLES,
        0,
        1,
        0,
        0,
        0,
        0,
        0,
        2,
        0,
    ])?;
    Ok(encoder.finish())
}

struct Encoder<'a> {
    output: &'a mut [u32],
    length: usize,
}

impl<'a> Encoder<'a> {
    fn new(output: &'a mut [u32]) -> Self {
        Self { output, length: 0 }
    }

    fn finish(self) -> usize {
        self.length
    }

    fn command(&mut self, command: u8, object: u8, payload_dwords: u16) -> Result<(), EncodeError> {
        self.word(u32::from(command) | (u32::from(object) << 8) | (u32::from(payload_dwords) << 16))
    }

    fn shader(&mut self, handle: u32, kind: u32, text: &[u8]) -> Result<(), EncodeError> {
        let text_words = text.len().div_ceil(4);
        self.command(
            CCMD_CREATE_OBJECT,
            OBJECT_SHADER,
            u16::try_from(5 + text_words).map_err(|_| EncodeError::BufferTooSmall)?,
        )?;
        self.words(&[handle, kind, text.len() as u32, 300, 0])?;
        self.bytes(text)?;
        self.command(CCMD_BIND_SHADER, 0, 2)?;
        self.words(&[handle, kind])
    }

    fn words(&mut self, values: &[u32]) -> Result<(), EncodeError> {
        for value in values {
            self.word(*value)?;
        }
        Ok(())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        for chunk in bytes.chunks(4) {
            let mut packed = [0u8; 4];
            packed[..chunk.len()].copy_from_slice(chunk);
            self.word(u32::from_le_bytes(packed))?;
        }
        Ok(())
    }

    fn word(&mut self, value: u32) -> Result<(), EncodeError> {
        let Some(slot) = self.output.get_mut(self.length) else {
            return Err(EncodeError::BufferTooSmall);
        };
        *slot = value;
        self.length += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangle_stream_is_bounded_and_structurally_complete() {
        let mut words = [0u32; 768];
        let count = encode_triangle(&mut words, 1280, 800, 41, 42).unwrap();
        assert!(count * 4 <= 3072);
        let mut cursor = 0usize;
        let mut draw_seen = false;
        while cursor < count {
            let header = words[cursor];
            let payload = (header >> 16) as usize;
            assert!(payload != 0);
            draw_seen |= header as u8 == CCMD_DRAW_VBO;
            cursor += payload + 1;
        }
        assert_eq!(cursor, count);
        assert!(draw_seen);
    }

    #[test]
    fn too_small_buffer_fails_without_overwrite() {
        let mut words = [0xdead_beefu32; 8];
        assert_eq!(
            encode_triangle(&mut words, 640, 480, 1, 2),
            Err(EncodeError::BufferTooSmall)
        );
    }

    #[test]
    fn update_stream_reuses_immutable_object_handles() {
        let vertices = [
            Vertex::new([-0.5, -0.5, 0.0, 1.0], [1.0, 0.0, 0.0, 1.0]),
            Vertex::new([0.5, -0.5, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]),
            Vertex::new([0.0, 0.5, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]),
        ];
        let mut words = [0u32; 768];
        let count = encode_mesh_update(
            &mut words,
            1280,
            800,
            41,
            42,
            &vertices,
            [0.0, 0.0, 0.0, 1.0],
        )
        .unwrap();
        let mut cursor = 0;
        while cursor < count {
            let header = words[cursor];
            assert_ne!(header as u8, CCMD_CREATE_OBJECT);
            cursor += (header >> 16) as usize + 1;
        }
        assert_eq!(cursor, count);
    }

    #[test]
    fn compositor_blits_layers_with_damage_and_alpha_without_readback() {
        let layers = [
            CompositeLayer::opaque(10, FORMAT_BGRX8888, BlitRect::new(0, 0, 1280, 800)),
            CompositeLayer {
                resource: 11,
                format: FORMAT_BGRA8888,
                source: BlitRect::new(0, 0, 640, 480),
                destination: BlitRect::new(120, 80, 640, 480),
                linear_filter: false,
                alpha_blend: true,
            },
        ];
        let mut words = [0u32; 128];
        let count = encode_composite_pass(
            &mut words,
            1280,
            800,
            12,
            FORMAT_BGRX8888,
            BlitRect::new(100, 60, 700, 540),
            &layers,
        )
        .unwrap();
        assert_eq!(count, 44);
        assert_eq!(words[0] as u8, CCMD_BLIT);
        assert_ne!(words[1] & (1 << 10), 0, "damage использует scissor");
        assert_eq!(words[22] as u8, CCMD_BLIT);
        assert_ne!(words[23] & (1 << 12), 0, "верхний слой смешивает alpha");
        let mut cursor = 0;
        while cursor < count {
            assert_eq!(words[cursor] as u8, CCMD_BLIT);
            cursor += (words[cursor] >> 16) as usize + 1;
        }
        assert_eq!(cursor, count);
    }

    #[test]
    fn compositor_rejects_partial_or_unbounded_batches() {
        let layer = CompositeLayer::opaque(2, FORMAT_BGRX8888, BlitRect::new(0, 0, 640, 480));
        let mut words = [0u32; 768];
        assert_eq!(
            encode_composite_pass(
                &mut words,
                640,
                480,
                1,
                FORMAT_BGRX8888,
                BlitRect::new(600, 0, 80, 10),
                &[layer],
            ),
            Err(EncodeError::InvalidExtent)
        );
        let layers = [layer; 513];
        assert_eq!(
            encode_composite_pass(
                &mut words,
                640,
                480,
                1,
                FORMAT_BGRX8888,
                BlitRect::new(0, 0, 640, 480),
                &layers,
            ),
            Err(EncodeError::TooManyLayers)
        );
    }

    #[test]
    fn atlas_upload_encodes_exact_patch_and_rejects_size_mismatch() {
        let pixels = [0x80u8; 8 * 4];
        let mut words = [0u32; 64];
        let count =
            encode_texture_upload(&mut words, 9, BlitRect::new(16, 24, 8, 4), 1, &pixels).unwrap();
        assert_eq!(words[0] as u8, CCMD_RESOURCE_INLINE_WRITE);
        assert_eq!(words[4], 8, "stride R8 atlas");
        assert_eq!(words[6], 16);
        assert_eq!(words[7], 24);
        assert_eq!(count, 20);
        assert_eq!(
            encode_texture_upload(&mut words, 9, BlitRect::new(0, 0, 8, 4), 1, &pixels[..31],),
            Err(EncodeError::InvalidUpload)
        );
    }
}

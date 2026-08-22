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
const FORMAT_BGRX8888: u32 = 2;
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
    if !matches!(surface_handle, 1 | 8 | 9) {
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
) -> Result<usize, EncodeError> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(EncodeError::InvalidExtent);
    }
    if vertices.is_empty() || !vertices.len().is_multiple_of(3) || vertices.len() > 48 {
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
    let vertex_bytes =
        u32::try_from(core::mem::size_of_val(vertices)).map_err(|_| EncodeError::BufferTooSmall)?;
    let vertex_dwords =
        u16::try_from(vertices.len() * 8).map_err(|_| EncodeError::BufferTooSmall)?;
    encoder.command(CCMD_RESOURCE_INLINE_WRITE, 0, 11 + vertex_dwords)?;
    encoder.words(&[
        vertex_resource,
        0,
        0,
        vertex_bytes,
        0,
        0,
        0,
        0,
        vertex_bytes,
        1,
        1,
    ])?;
    for vertex in vertices {
        for component in vertex.position.into_iter().chain(vertex.color) {
            encoder.word(component.to_bits())?;
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
        encoder.words(&[5, 0, 0, 0x7800_0000, 0, 0, 0, 0, 0, 0, 0])?;
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
}

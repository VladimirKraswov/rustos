//! Минимальный protocol между compositor и display service.
//!
//! Pixel data не входит в сообщение. IPC переносит три capabilities в строгом
//! порядке: graphics buffer, acquire timeline и release timeline. Payload
//! связывает их значения с одним frame и целиком помещается inline.

use crate::graphics_buffer::{GraphicsBufferDesc, PixelFormatCode};

/// Первая версия compositor → display protocol.
pub const DISPLAY_PROTOCOL_VERSION: u16 = 1;
/// Opcode атомарной публикации frame.
pub const DISPLAY_PRESENT_OPCODE: u16 = 1;
/// Число capabilities запроса: buffer, acquire, release.
pub const DISPLAY_PRESENT_HANDLE_COUNT: u16 = 3;

/// Bounded запрос показа одного готового graphics buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayPresentRequest {
    /// [`DISPLAY_PROTOCOL_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Монотонный frame ID compositor'а.
    pub frame_id: u64,
    /// Ширина scanout source в физических пикселях.
    pub width: u32,
    /// Высота scanout source в физических пикселях.
    pub height: u32,
    /// Шаг первой plane.
    pub stride_bytes: u32,
    /// Packed RGB pixel format первой реализации.
    pub format: PixelFormatCode,
    /// Точный размер содержимого; mapping округляется до страниц.
    pub byte_size: u64,
    /// Display ждёт это значение acquire timeline до чтения.
    pub acquire_value: u64,
    /// Display сигналит это значение release timeline после чтения.
    pub release_value: u64,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved: u64,
}

impl DisplayPresentRequest {
    /// Создаёт запрос из уже проверенного graphics descriptor.
    pub fn from_buffer(
        frame_id: u64,
        descriptor: &GraphicsBufferDesc,
        acquire_value: u64,
        release_value: u64,
    ) -> Self {
        Self {
            version: DISPLAY_PROTOCOL_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            frame_id,
            width: descriptor.width,
            height: descriptor.height,
            stride_bytes: descriptor.planes[0].stride_bytes,
            format: descriptor.format,
            byte_size: descriptor.byte_size,
            acquire_value,
            release_value,
            reserved: 0,
        }
    }

    /// Проверяет заголовок и минимальный диапазон первой plane.
    pub fn validate(self) -> Result<(), DisplayProtocolError> {
        if self.version != DISPLAY_PROTOCOL_VERSION {
            return Err(DisplayProtocolError::UnsupportedVersion);
        }
        if self.size as usize != core::mem::size_of::<Self>() {
            return Err(DisplayProtocolError::UnsupportedSize);
        }
        if self.flags != 0 || self.reserved != 0 {
            return Err(DisplayProtocolError::ReservedNonZero);
        }
        if self.frame_id == 0
            || self.width == 0
            || self.height == 0
            || self.stride_bytes == 0
            || self.byte_size == 0
            || self.acquire_value == 0
            || self.release_value == 0
        {
            return Err(DisplayProtocolError::InvalidDimensions);
        }
        if !matches!(
            self.format,
            PixelFormatCode::R8G8B8X8_UNORM
                | PixelFormatCode::B8G8R8X8_UNORM
                | PixelFormatCode::B8G8R8A8_UNORM
        ) {
            return Err(DisplayProtocolError::UnsupportedFormat);
        }
        let minimum_stride = self
            .width
            .checked_mul(4)
            .ok_or(DisplayProtocolError::InvalidDimensions)?;
        let minimum_size = u64::from(self.stride_bytes)
            .checked_mul(u64::from(self.height))
            .ok_or(DisplayProtocolError::InvalidDimensions)?;
        if self.stride_bytes < minimum_stride || self.byte_size < minimum_size {
            return Err(DisplayProtocolError::InvalidDimensions);
        }
        Ok(())
    }

    /// Кодирует payload без Rust ABI или process-local pointer.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u32(&mut bytes, 4, self.flags);
        put_u64(&mut bytes, 8, self.frame_id);
        put_u32(&mut bytes, 16, self.width);
        put_u32(&mut bytes, 20, self.height);
        put_u32(&mut bytes, 24, self.stride_bytes);
        put_u32(&mut bytes, 28, self.format.0);
        put_u64(&mut bytes, 32, self.byte_size);
        put_u64(&mut bytes, 40, self.acquire_value);
        put_u64(&mut bytes, 48, self.release_value);
        put_u64(&mut bytes, 56, self.reserved);
        bytes
    }

    /// Декодирует ровно 64 байта и сразу валидирует packet.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, DisplayProtocolError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(DisplayProtocolError::Truncated);
        }
        let request = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            flags: get_u32(bytes, 4),
            frame_id: get_u64(bytes, 8),
            width: get_u32(bytes, 16),
            height: get_u32(bytes, 20),
            stride_bytes: get_u32(bytes, 24),
            format: PixelFormatCode(get_u32(bytes, 28)),
            byte_size: get_u64(bytes, 32),
            acquire_value: get_u64(bytes, 40),
            release_value: get_u64(bytes, 48),
            reserved: get_u64(bytes, 56),
        };
        request.validate()?;
        Ok(request)
    }
}

/// Ошибка display protocol до обращения к capability objects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayProtocolError {
    /// Payload короче или длиннее выбранной версии.
    Truncated,
    /// Версия не поддерживается.
    UnsupportedVersion,
    /// Поле size не совпадает с версией.
    UnsupportedSize,
    /// Размеры, stride, ID или timeline values некорректны.
    InvalidDimensions,
    /// Display backend пока не принимает такой pixel format.
    UnsupportedFormat,
    /// Неизвестный флаг или reserved data не равны нулю.
    ReservedNonZero,
}

fn put_u16(bytes: &mut [u8; 64], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8; 64], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8; 64], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

const _: () = assert!(core::mem::size_of::<DisplayPresentRequest>() == 64);
const _: () = assert!(core::mem::align_of::<DisplayPresentRequest>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DisplayPresentRequest {
        let descriptor = GraphicsBufferDesc::linear(
            64,
            48,
            PixelFormatCode::B8G8R8A8_UNORM,
            crate::graphics_buffer::BufferUsage::CPU_READ,
            crate::graphics_buffer::MemoryDomain::SYSTEM
                .union(crate::graphics_buffer::MemoryDomain::HOST_VISIBLE),
        )
        .unwrap();
        DisplayPresentRequest::from_buffer(7, &descriptor, 9, 10)
    }

    #[test]
    fn inline_roundtrip_is_exact() {
        let request = request();
        assert_eq!(
            DisplayPresentRequest::decode_inline(&request.encode_inline()),
            Ok(request)
        );
    }

    #[test]
    fn truncated_reserved_and_short_stride_are_rejected() {
        assert_eq!(
            DisplayPresentRequest::decode_inline(&request().encode_inline()[..63]),
            Err(DisplayProtocolError::Truncated)
        );
        let mut invalid = request();
        invalid.reserved = 1;
        assert_eq!(
            invalid.validate(),
            Err(DisplayProtocolError::ReservedNonZero)
        );
        invalid = request();
        invalid.stride_bytes = 4;
        assert_eq!(
            invalid.validate(),
            Err(DisplayProtocolError::InvalidDimensions)
        );
    }
}

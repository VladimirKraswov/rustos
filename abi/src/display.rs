//! Минимальный protocol между compositor и display service.
//!
//! Pixel data не входит в сообщение. Present IPC переносит четыре capabilities
//! в строгом порядке: graphics buffer, acquire timeline, release timeline и
//! endpoint обратной связи. Payload связывает их значения с одним frame и
//! целиком помещается inline.

use crate::{
    graphics_buffer::{GraphicsBufferDesc, PixelFormatCode},
    surface::{OutputId, PresentationStatus},
};

/// Версия compositor → display protocol с обязательным feedback endpoint.
pub const DISPLAY_PROTOCOL_VERSION: u16 = 2;
/// Opcode атомарной публикации frame.
pub const DISPLAY_PRESENT_OPCODE: u16 = 1;
/// Opcode результата показа frame.
pub const DISPLAY_FEEDBACK_OPCODE: u16 = 2;
/// Opcode запроса active output snapshot.
pub const DISPLAY_QUERY_OPCODE: u16 = 3;
/// Opcode ответа с [`DisplayScanoutInfo`].
pub const DISPLAY_INFO_OPCODE: u16 = 4;
/// Число capabilities запроса: buffer, acquire, release, feedback endpoint.
pub const DISPLAY_PRESENT_HANDLE_COUNT: u16 = 4;
/// Query переносит только endpoint ответа с правом SEND.
pub const DISPLAY_QUERY_HANDLE_COUNT: u16 = 1;

/// Версия syscall ABI display controller object.
pub const DISPLAY_SCANOUT_ABI_VERSION: u16 = 1;

/// Возможности [`DisplayScanoutInfo`].
pub mod scanout_capabilities {
    /// Device принимает атомарный full-frame commit.
    pub const ATOMIC_PRESENT: u32 = 1 << 0;
    /// Время vblank вычисляется из refresh period, а не приходит с IRQ.
    pub const ESTIMATED_VBLANK: u32 = 1 << 1;
    /// Runtime mode-set поддерживается display controller'ом.
    pub const MODE_SET: u32 = 1 << 2;
    /// Все известные биты первой версии.
    pub const KNOWN: u32 = ATOMIC_PRESENT | ESTIMATED_VBLANK | MODE_SET;
}

/// Снимок единственного bootstrap output.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayScanoutInfo {
    /// [`DISPLAY_SCANOUT_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Стабильный ID output.
    pub output: OutputId,
    /// Активная ширина в физических пикселях.
    pub width: u32,
    /// Активная высота в физических пикселях.
    pub height: u32,
    /// Шаг scanout в байтах.
    pub stride_bytes: u32,
    /// Формат, принимаемый direct scanout path.
    pub format: PixelFormatCode,
    /// Частота обновления в миллигерцах.
    pub refresh_millihertz: u32,
    /// Биты [`scanout_capabilities`].
    pub capabilities: u32,
    /// Меняется после успешного mode-set.
    pub mode_generation: u64,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved: [u64; 2],
}

impl DisplayScanoutInfo {
    /// Проверяет kernel snapshot до использования display service.
    pub fn validate(self) -> Result<(), DisplayProtocolError> {
        validate_scanout_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0
            || self.reserved != [0; 2]
            || self.capabilities & !scanout_capabilities::KNOWN != 0
        {
            return Err(DisplayProtocolError::ReservedNonZero);
        }
        let minimum_stride = self
            .width
            .checked_mul(4)
            .ok_or(DisplayProtocolError::InvalidDimensions)?;
        if !self.output.is_valid()
            || self.width == 0
            || self.height == 0
            || self.stride_bytes < minimum_stride
            || self.refresh_millihertz == 0
            || self.mode_generation == 0
        {
            return Err(DisplayProtocolError::InvalidDimensions);
        }
        if !matches!(
            self.format,
            PixelFormatCode::B8G8R8X8_UNORM | PixelFormatCode::B8G8R8A8_UNORM
        ) {
            return Err(DisplayProtocolError::UnsupportedFormat);
        }
        Ok(())
    }

    /// Кодирует active output snapshot в один IPC payload.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u32(&mut bytes, 4, self.flags);
        put_u64(&mut bytes, 8, self.output.0);
        put_u32(&mut bytes, 16, self.width);
        put_u32(&mut bytes, 20, self.height);
        put_u32(&mut bytes, 24, self.stride_bytes);
        put_u32(&mut bytes, 28, self.format.0);
        put_u32(&mut bytes, 32, self.refresh_millihertz);
        put_u32(&mut bytes, 36, self.capabilities);
        put_u64(&mut bytes, 40, self.mode_generation);
        put_u64(&mut bytes, 48, self.reserved[0]);
        put_u64(&mut bytes, 56, self.reserved[1]);
        bytes
    }

    /// Декодирует и проверяет output snapshot от displayd.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, DisplayProtocolError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(DisplayProtocolError::Truncated);
        }
        let info = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            flags: get_u32(bytes, 4),
            output: OutputId(get_u64(bytes, 8)),
            width: get_u32(bytes, 16),
            height: get_u32(bytes, 20),
            stride_bytes: get_u32(bytes, 24),
            format: PixelFormatCode(get_u32(bytes, 28)),
            refresh_millihertz: get_u32(bytes, 32),
            capabilities: get_u32(bytes, 36),
            mode_generation: get_u64(bytes, 40),
            reserved: [get_u64(bytes, 48), get_u64(bytes, 56)],
        };
        info.validate()?;
        Ok(info)
    }
}

/// Атомарная публикация полностью готового graphics buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayAtomicPresent {
    /// [`DISPLAY_SCANOUT_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю; damage появится аддитивным ABI.
    pub flags: u32,
    /// Монотонный frame ID compositor'а.
    pub frame_id: u64,
    /// Generation из [`DisplayScanoutInfo`].
    pub expected_mode_generation: u64,
    /// Желаемое монотонное время показа; ноль означает ближайший refresh.
    pub target_time_ns: u64,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved: [u64; 2],
}

impl DisplayAtomicPresent {
    /// Создаёт full-frame commit для активного mode generation.
    pub const fn new(frame_id: u64, expected_mode_generation: u64) -> Self {
        Self {
            version: DISPLAY_SCANOUT_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            frame_id,
            expected_mode_generation,
            target_time_ns: 0,
            reserved: [0; 2],
        }
    }

    /// Проверяет request до импорта graphics buffer.
    pub fn validate(self) -> Result<(), DisplayProtocolError> {
        validate_scanout_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != [0; 2] {
            return Err(DisplayProtocolError::ReservedNonZero);
        }
        if self.frame_id == 0 || self.expected_mode_generation == 0 {
            return Err(DisplayProtocolError::InvalidDimensions);
        }
        Ok(())
    }
}

/// Блокирующее ожидание refresh boundary после atomic present.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayVblankWait {
    /// [`DISPLAY_SCANOUT_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Sequence, возвращённый `display_atomic_present`.
    pub sequence: u64,
    /// Относительный timeout в наносекундах; `u64::MAX` без timeout.
    pub timeout_ns: u64,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved: [u64; 2],
}

impl DisplayVblankWait {
    /// Создаёт ожидание опубликованной sequence.
    pub const fn new(sequence: u64, timeout_ns: u64) -> Self {
        Self {
            version: DISPLAY_SCANOUT_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            sequence,
            timeout_ns,
            reserved: [0; 2],
        }
    }

    /// Проверяет request до постановки thread в wait queue.
    pub fn validate(self) -> Result<(), DisplayProtocolError> {
        validate_scanout_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != [0; 2] {
            return Err(DisplayProtocolError::ReservedNonZero);
        }
        if self.sequence == 0 {
            return Err(DisplayProtocolError::InvalidDimensions);
        }
        Ok(())
    }
}

/// Inline feedback displayd → compositord после release buffer'а.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayPresentFeedback {
    /// [`DISPLAY_PROTOCOL_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// [`PresentationStatus`].
    pub status: PresentationStatus,
    /// Биты [`feedback_flags`].
    pub flags: u16,
    /// ID исходного frame.
    pub frame_id: u64,
    /// Монотонная display sequence.
    pub sequence: u64,
    /// Время завершения wait, ns.
    pub actual_time_ns: u64,
    /// Период refresh, ns.
    pub refresh_interval_ns: u64,
    /// Output, принявший frame.
    pub output: OutputId,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved: [u64; 2],
}

/// Флаги [`DisplayPresentFeedback`].
pub mod feedback_flags {
    /// Timing рассчитан из refresh period: virtio-gpu 2D не даёт vblank IRQ.
    pub const ESTIMATED_VBLANK: u16 = 1 << 0;
    /// Все известные биты первой версии.
    pub const KNOWN: u16 = ESTIMATED_VBLANK;
}

impl DisplayPresentFeedback {
    /// Создаёт подтверждение успешно показанного frame.
    pub const fn presented(
        frame_id: u64,
        sequence: u64,
        actual_time_ns: u64,
        refresh_interval_ns: u64,
        output: OutputId,
    ) -> Self {
        Self {
            version: DISPLAY_PROTOCOL_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            status: PresentationStatus::PRESENTED,
            flags: feedback_flags::ESTIMATED_VBLANK,
            frame_id,
            sequence,
            actual_time_ns,
            refresh_interval_ns,
            output,
            reserved: [0; 2],
        }
    }

    /// Проверяет feedback до frame pacing compositor'а.
    pub fn validate(self) -> Result<(), DisplayProtocolError> {
        if self.version != DISPLAY_PROTOCOL_VERSION {
            return Err(DisplayProtocolError::UnsupportedVersion);
        }
        if self.size as usize != core::mem::size_of::<Self>() {
            return Err(DisplayProtocolError::UnsupportedSize);
        }
        if !self.status.is_known()
            || self.status != PresentationStatus::PRESENTED
            || self.flags & !feedback_flags::KNOWN != 0
            || self.reserved != [0; 2]
            || self.frame_id == 0
            || self.sequence == 0
            || self.actual_time_ns == 0
            || self.refresh_interval_ns == 0
            || !self.output.is_valid()
        {
            return Err(DisplayProtocolError::InvalidFeedback);
        }
        Ok(())
    }

    /// Кодирует wire payload без зависимости от Rust ABI.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u16(&mut bytes, 4, self.status.0);
        put_u16(&mut bytes, 6, self.flags);
        put_u64(&mut bytes, 8, self.frame_id);
        put_u64(&mut bytes, 16, self.sequence);
        put_u64(&mut bytes, 24, self.actual_time_ns);
        put_u64(&mut bytes, 32, self.refresh_interval_ns);
        put_u64(&mut bytes, 40, self.output.0);
        put_u64(&mut bytes, 48, self.reserved[0]);
        put_u64(&mut bytes, 56, self.reserved[1]);
        bytes
    }

    /// Декодирует и проверяет ровно один inline payload.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, DisplayProtocolError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(DisplayProtocolError::Truncated);
        }
        let feedback = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            status: PresentationStatus(get_u16(bytes, 4)),
            flags: get_u16(bytes, 6),
            frame_id: get_u64(bytes, 8),
            sequence: get_u64(bytes, 16),
            actual_time_ns: get_u64(bytes, 24),
            refresh_interval_ns: get_u64(bytes, 32),
            output: OutputId(get_u64(bytes, 40)),
            reserved: [get_u64(bytes, 48), get_u64(bytes, 56)],
        };
        feedback.validate()?;
        Ok(feedback)
    }
}

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
    /// Presentation feedback внутренне противоречив.
    InvalidFeedback,
}

fn validate_scanout_header(
    version: u16,
    size: u16,
    expected_size: u16,
) -> Result<(), DisplayProtocolError> {
    if version != DISPLAY_SCANOUT_ABI_VERSION {
        return Err(DisplayProtocolError::UnsupportedVersion);
    }
    if size != expected_size {
        return Err(DisplayProtocolError::UnsupportedSize);
    }
    Ok(())
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
const _: () = assert!(core::mem::size_of::<DisplayScanoutInfo>() == 64);
const _: () = assert!(core::mem::size_of::<DisplayAtomicPresent>() == 48);
const _: () = assert!(core::mem::size_of::<DisplayVblankWait>() == 40);
const _: () = assert!(core::mem::size_of::<DisplayPresentFeedback>() == 64);

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

    #[test]
    fn scanout_requests_reject_stale_or_reserved_state() {
        let mut present = DisplayAtomicPresent::new(1, 2);
        assert_eq!(present.validate(), Ok(()));
        present.reserved[0] = 1;
        assert_eq!(
            present.validate(),
            Err(DisplayProtocolError::ReservedNonZero)
        );
        let wait = DisplayVblankWait::new(0, 10);
        assert_eq!(
            wait.validate(),
            Err(DisplayProtocolError::InvalidDimensions)
        );
    }

    #[test]
    fn feedback_roundtrip_preserves_estimated_vblank() {
        let feedback = DisplayPresentFeedback::presented(7, 9, 100, 16_666_667, OutputId(1));
        assert_eq!(
            DisplayPresentFeedback::decode_inline(&feedback.encode_inline()),
            Ok(feedback)
        );
        let mut invalid = feedback;
        invalid.flags = u16::MAX;
        assert_eq!(
            invalid.validate(),
            Err(DisplayProtocolError::InvalidFeedback)
        );
    }

    #[test]
    fn scanout_info_roundtrip_is_wire_stable() {
        let info = DisplayScanoutInfo {
            version: DISPLAY_SCANOUT_ABI_VERSION,
            size: core::mem::size_of::<DisplayScanoutInfo>() as u16,
            flags: 0,
            output: OutputId(1),
            width: 1280,
            height: 800,
            stride_bytes: 5120,
            format: PixelFormatCode::B8G8R8X8_UNORM,
            refresh_millihertz: 60_000,
            capabilities: scanout_capabilities::ATOMIC_PRESENT
                | scanout_capabilities::ESTIMATED_VBLANK,
            mode_generation: 1,
            reserved: [0; 2],
        };
        assert_eq!(
            DisplayScanoutInfo::decode_inline(&info.encode_inline()),
            Ok(info)
        );
    }
}

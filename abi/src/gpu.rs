//! ABI ускоренного рендеринга.
//!
//! Приложения не получают MMIO/PCI и не пишут descriptor rings напрямую.
//! Эксклюзивный `renderd` создаёт изолированный контекст, импортирует
//! [`crate::graphics_buffer::GraphicsBufferDesc`] и отправляет bounded VirGL
//! command stream. Завершение публикуется обычным [`crate::sync`] timeline.

use crate::Handle;

/// Версия render ABI.
pub const GPU_ABI_VERSION: u16 = 1;
/// Максимальный command buffer одного submission.
pub const GPU_MAX_COMMAND_BYTES: u32 = 3072;
/// compositord → renderd: запрос одного кадра.
pub const GPU_RENDER_REQUEST_OPCODE: u16 = 0x5100;
/// renderd → compositord: GraphicsBuffer + acquire timeline готовы.
pub const GPU_RENDERED_FRAME_OPCODE: u16 = 0x5101;
/// Число handles в `GPU_RENDERED_FRAME_OPCODE`.
pub const GPU_RENDERED_FRAME_HANDLE_COUNT: u16 = 2;

/// Биты [`GpuDeviceInfo::features`].
pub mod feature {
    /// Virtio GPU предложил feature bit `VIRTIO_GPU_F_VIRGL`.
    pub const VIRGL: u64 = 1 << 0;
    /// Submission возвращает сразу, completion приходит через timeline.
    pub const ASYNC_FENCE: u64 = 1 << 1;
    /// Импортированный render target можно без копирования отдать scanout.
    pub const ZERO_COPY_SCANOUT: u64 = 1 << 2;
}

/// Неизменяемые возможности render device.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuDeviceInfo {
    /// [`GPU_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// Зарезервировано; равно нулю.
    pub reserved_header: u32,
    /// Набор `feature::*`.
    pub features: u64,
    /// Максимальный размер одного command buffer.
    pub max_command_bytes: u32,
    /// Число одновременно принятых submissions.
    pub max_inflight: u16,
    /// Число контекстов текущей реализации.
    pub max_contexts: u16,
    /// Выбранный capset (`1` — VirGL, `2` — VirGL2).
    pub capset_id: u32,
    /// Максимальная версия выбранного capset.
    pub capset_version: u32,
    /// Максимальный размер capability blob.
    pub capset_size: u32,
    /// Зарезервировано для будущих engines/queues.
    pub reserved: [u64; 3],
}

impl GpuDeviceInfo {
    /// Проверяет wire record и обязательные зарезервированные поля.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        if self.version != GPU_ABI_VERSION {
            return Err(GpuAbiError::UnsupportedVersion);
        }
        if self.size as usize != core::mem::size_of::<Self>() {
            return Err(GpuAbiError::UnsupportedSize);
        }
        if self.reserved_header != 0 || self.reserved != [0; 3] {
            return Err(GpuAbiError::ReservedNonZero);
        }
        if self.max_command_bytes == 0
            || self.max_command_bytes > GPU_MAX_COMMAND_BYTES
            || self.max_inflight == 0
            || self.max_contexts == 0
        {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }
}

/// Параметры создания изолированного GPU context.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuContextCreate {
    /// [`GPU_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// В версии 1 равно нулю.
    pub flags: u32,
    /// Короткое диагностическое имя; UTF-8, хвост заполнен нулями.
    pub debug_name: [u8; 48],
    /// Зарезервировано.
    pub reserved: [u64; 1],
}

impl GpuContextCreate {
    /// Создаёт запрос и обрезает имя на границе UTF-8 не требуется: имя
    /// диагностическое, а kernel принимает только ASCII subset.
    pub fn new(name: &[u8]) -> Self {
        let mut result = Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            debug_name: [0; 48],
            reserved: [0; 1],
        };
        let count = name.len().min(result.debug_name.len());
        result.debug_name[..count].copy_from_slice(&name[..count]);
        result
    }

    /// Проверяет запрос.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags != 0 || self.reserved != [0; 1] {
            return Err(GpuAbiError::ReservedNonZero);
        }
        if self
            .debug_name
            .iter()
            .copied()
            .take_while(|byte| *byte != 0)
            .any(|byte| !(0x20..=0x7e).contains(&byte))
        {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }
}

/// Назначение импортируемого/создаваемого VirGL resource.
pub mod resource_bind {
    /// Color render target.
    pub const RENDER_TARGET: u32 = 1 << 1;
    /// Vertex buffer.
    pub const VERTEX_BUFFER: u32 = 1 << 4;
    /// Ресурс можно показать на output.
    pub const DISPLAY_TARGET: u32 = 1 << 8;
    /// Разрешённый ABI subset.
    pub const KNOWN: u32 = RENDER_TARGET | VERTEX_BUFFER | DISPLAY_TARGET;
}

/// Тип ресурса Gallium/VirGL.
pub mod resource_target {
    /// Byte buffer.
    pub const BUFFER: u32 = 0;
    /// Двумерная texture.
    pub const TEXTURE_2D: u32 = 2;
}

/// Форматы, необходимые bootstrap renderer'у.
pub mod virgl_format {
    /// XRGB8888/BGR byte order.
    pub const B8G8R8X8_UNORM: u32 = 2;
    /// Одноканальный byte buffer.
    pub const R8_UNORM: u32 = 64;
}

/// Импорт capability-backed GraphicsBuffer как GPU resource.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuResourceImport {
    /// [`GPU_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// В версии 1 равно нулю.
    pub flags: u32,
    /// [`resource_target`].
    pub target: u32,
    /// [`resource_bind`].
    pub bind: u32,
    /// Зарезервировано.
    pub reserved: [u64; 2],
}

impl GpuResourceImport {
    /// Описывает полноэкранный color target.
    pub const fn render_target() -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            target: resource_target::TEXTURE_2D,
            bind: resource_bind::RENDER_TARGET | resource_bind::DISPLAY_TARGET,
            reserved: [0; 2],
        }
    }

    /// Проверяет запрос.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags != 0 || self.reserved != [0; 2] {
            return Err(GpuAbiError::ReservedNonZero);
        }
        if self.target != resource_target::TEXTURE_2D
            || self.bind == 0
            || self.bind & !resource_bind::KNOWN != 0
            || self.bind & resource_bind::RENDER_TARGET == 0
            || self.bind & resource_bind::DISPLAY_TARGET == 0
        {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }
}

/// Создание context-local GPU resource без guest backing.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuResourceCreate {
    /// [`GPU_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// В версии 1 равно нулю.
    pub flags: u32,
    /// [`resource_target`].
    pub target: u32,
    /// VirGL format.
    pub format: u32,
    /// [`resource_bind`].
    pub bind: u32,
    /// Размеры base level.
    pub width: u32,
    /// Высота.
    pub height: u32,
    /// Глубина.
    pub depth: u32,
    /// Число array layers.
    pub array_size: u32,
    /// Зарезервировано.
    pub reserved: [u64; 2],
}

impl GpuResourceCreate {
    /// Создаёт небольшой vertex buffer; данные придут через VirGL inline-write.
    pub const fn vertex_buffer(bytes: u32) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            target: resource_target::BUFFER,
            format: virgl_format::R8_UNORM,
            bind: resource_bind::VERTEX_BUFFER,
            width: bytes,
            height: 1,
            depth: 1,
            array_size: 1,
            reserved: [0; 2],
        }
    }

    /// Проверяет ограниченный bootstrap subset.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags != 0 || self.reserved != [0; 2] {
            return Err(GpuAbiError::ReservedNonZero);
        }
        if self.target != resource_target::BUFFER
            || self.format != virgl_format::R8_UNORM
            || self.bind != resource_bind::VERTEX_BUFFER
            || self.width == 0
            || self.width > 1024 * 1024
            || self.height != 1
            || self.depth != 1
            || self.array_size != 1
        {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }
}

/// Один асинхронный submission.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuSubmit {
    /// [`GPU_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// В версии 1 равно нулю.
    pub flags: u32,
    /// User virtual address массива VirGL dwords.
    pub commands_address: u64,
    /// Размер массива в байтах; кратен четырём.
    pub command_bytes: u32,
    /// Зарезервировано для нескольких command rings.
    pub ring_index: u8,
    /// Зарезервировано.
    pub reserved_small: [u8; 3],
    /// Timeline, который kernel продвинет после device fence.
    pub completion_timeline: Handle,
    /// Зарезервировано для выравнивания.
    pub reserved_handle: u32,
    /// Новое значение timeline.
    pub completion_value: u64,
    /// Зарезервировано.
    pub reserved: [u64; 3],
}

impl GpuSubmit {
    /// Создаёт запрос для control ring 0.
    pub const fn new(
        commands_address: u64,
        command_bytes: u32,
        completion_timeline: Handle,
        completion_value: u64,
    ) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            commands_address,
            command_bytes,
            ring_index: 0,
            reserved_small: [0; 3],
            completion_timeline,
            reserved_handle: 0,
            completion_value,
            reserved: [0; 3],
        }
    }

    /// Проверяет record без разыменования user pointer.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags != 0
            || self.ring_index != 0
            || self.reserved_small != [0; 3]
            || self.reserved_handle != 0
            || self.reserved != [0; 3]
        {
            return Err(GpuAbiError::ReservedNonZero);
        }
        if self.commands_address == 0
            || self.command_bytes == 0
            || self.command_bytes > GPU_MAX_COMMAND_BYTES
            || !self.command_bytes.is_multiple_of(4)
            || self.completion_timeline == Handle::INVALID
            || self.completion_value == 0
        {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }
}

/// Ошибка проверки render ABI record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuAbiError {
    /// Неизвестная версия.
    UnsupportedVersion,
    /// Неверный размер.
    UnsupportedSize,
    /// Зарезервированное поле не равно нулю.
    ReservedNonZero,
    /// Значение вне поддерживаемого bounded subset.
    InvalidValue,
}

/// Inline IPC payload запроса/ответа bootstrap renderer'а.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuRenderFrame {
    /// [`GPU_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// В версии 1 равно нулю.
    pub flags: u32,
    /// Physical render width.
    pub width: u32,
    /// Physical render height.
    pub height: u32,
    /// Идентификатор кадра compositor'а.
    pub frame_id: u64,
    /// Device fence; в запросе равен нулю.
    pub fence_id: u64,
    /// Зарезервировано.
    pub reserved: [u64; 4],
}

impl GpuRenderFrame {
    /// Создаёт compositor request.
    pub const fn request(width: u32, height: u32, frame_id: u64) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            width,
            height,
            frame_id,
            fence_id: 0,
            reserved: [0; 4],
        }
    }

    /// Проверяет общий record.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags != 0 || self.reserved != [0; 4] {
            return Err(GpuAbiError::ReservedNonZero);
        }
        if self.width == 0 || self.height == 0 || self.frame_id == 0 {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }

    /// Копирует record в фиксированный IPC payload без указателей.
    pub fn encode_inline(&self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        let source = unsafe {
            core::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        };
        bytes.copy_from_slice(source);
        bytes
    }

    /// Декодирует и проверяет фиксированный IPC payload.
    pub fn decode_inline(bytes: &[u8; 64]) -> Result<Self, GpuAbiError> {
        let value = unsafe { (bytes.as_ptr() as *const Self).read_unaligned() };
        value.validate()?;
        Ok(value)
    }
}

fn validate_prefix(version: u16, size: u16, expected: usize) -> Result<(), GpuAbiError> {
    if version != GPU_ABI_VERSION {
        return Err(GpuAbiError::UnsupportedVersion);
    }
    if size as usize != expected {
        return Err(GpuAbiError::UnsupportedSize);
    }
    Ok(())
}

const _: () = assert!(core::mem::size_of::<GpuDeviceInfo>() == 64);
const _: () = assert!(core::mem::size_of::<GpuContextCreate>() == 64);
const _: () = assert!(core::mem::size_of::<GpuResourceImport>() == 32);
const _: () = assert!(core::mem::size_of::<GpuResourceCreate>() == 56);
const _: () = assert!(core::mem::size_of::<GpuSubmit>() == 64);
const _: () = assert!(core::mem::size_of::<GpuRenderFrame>() == 64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_rejects_unaligned_and_reserved_data() {
        let mut request = GpuSubmit::new(0x1000, 16, Handle(3), 1);
        assert_eq!(request.validate(), Ok(()));
        request.command_bytes = 15;
        assert_eq!(request.validate(), Err(GpuAbiError::InvalidValue));
        request.command_bytes = 16;
        request.reserved[0] = 1;
        assert_eq!(request.validate(), Err(GpuAbiError::ReservedNonZero));
    }

    #[test]
    fn context_name_is_bounded_ascii() {
        assert_eq!(GpuContextCreate::new(b"renderd").validate(), Ok(()));
        assert_eq!(
            GpuContextCreate::new(&[0xff]).validate(),
            Err(GpuAbiError::InvalidValue)
        );
    }

    #[test]
    fn resource_requests_keep_small_contract() {
        assert_eq!(GpuResourceImport::render_target().validate(), Ok(()));
        assert_eq!(GpuResourceCreate::vertex_buffer(96).validate(), Ok(()));
        assert_eq!(
            GpuResourceCreate::vertex_buffer(0).validate(),
            Err(GpuAbiError::InvalidValue)
        );
    }

    #[test]
    fn render_frame_roundtrip_is_pointer_free() {
        let request = GpuRenderFrame::request(1280, 800, 7);
        assert_eq!(
            GpuRenderFrame::decode_inline(&request.encode_inline()),
            Ok(request)
        );
    }
}

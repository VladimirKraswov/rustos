//! ABI ускоренного рендеринга.
//!
//! Приложения не получают MMIO/PCI и не пишут descriptor rings напрямую.
//! Эксклюзивный `renderd` создаёт изолированный контекст, импортирует
//! [`crate::graphics_buffer::GraphicsBufferDesc`] и отправляет bounded VirGL
//! command stream. Завершение публикуется обычным [`crate::sync`] timeline.

use crate::Handle;

/// Версия render ABI.
pub const GPU_ABI_VERSION: u16 = 2;
/// Верхняя граница одного VirGL submission.
///
/// Для SystemUI важен не только объём данных, но и число переходов через
/// virtqueue/fence. 512 KiB позволяют передать несколько тысяч простых
/// примитивов одним пакетом и всё ещё оставляют жёсткую, легко проверяемую
/// границу памяти для недоверенного ring-3 renderer'а.
pub const GPU_MAX_COMMAND_BYTES: u32 = 512 * 1024;
/// compositord → renderd: запрос одного кадра.
pub const GPU_RENDER_REQUEST_OPCODE: u16 = 0x5100;
/// renderd → compositord: GraphicsBuffer + acquire timeline готовы.
pub const GPU_RENDERED_FRAME_OPCODE: u16 = 0x5101;
/// Число handles в `GPU_RENDERED_FRAME_OPCODE`.
pub const GPU_RENDERED_FRAME_HANDLE_COUNT: u16 = 2;
/// gpu-demo → compositord: запустить bounded полноэкранную демонстрацию.
pub const GPU_DEMO_START_OPCODE: u16 = 0x5110;
/// compositord → future launcher: демонстрация завершила последний present.
pub const GPU_DEMO_DONE_OPCODE: u16 = 0x5111;
/// Размер общей command page `uid -> renderd`.
///
/// Это не публичный Canvas ABI приложения: page принадлежит системному UI
/// renderer'у и выдаётся только доверенному `renderd`.
pub const GPU_UI_STREAM_BYTES: usize = 1024 * 1024;
/// Версия внутреннего SystemUI GPU stream.
pub const GPU_UI_STREAM_VERSION: u16 = 1;

/// Биты [`GpuUiQuad::flags`]. Это внутренние renderer-neutral primitives
/// SystemUI, а не VirGL protocol и не публичный API приложения.
pub mod ui_quad_flag {
    /// Quad показывает одну из встроенных wallpaper textures. Стабильный
    /// идентификатор ресурса передаётся как `colors[0]`.
    pub const WALLPAPER_TEXTURE: u32 = 1 << 0;
    /// Все известные биты версии 1.
    pub const KNOWN: u32 = WALLPAPER_TEXTURE;
}

/// Биты [`GpuRenderFrame::flags`].
pub mod frame_flag {
    /// Renderd должен сформировать анимированную Aurora 3D scene.
    pub const AURORA_SHOWCASE: u32 = 1 << 0;
    /// Renderd должен выполнить диагностический GPU compositor pass из
    /// нескольких sampled surfaces прямо в scanout-compatible target.
    pub const COMPOSITOR_PROBE: u32 = 1 << 1;
    /// Отрисовать renderer-neutral SystemUI stream из общей command page.
    pub const SYSTEM_UI: u32 = 1 << 2;
    /// Все известные флаги.
    pub const KNOWN: u32 = AURORA_SHOWCASE | COMPOSITOR_PROBE | SYSTEM_UI;
}

/// Заголовок одного неизменяемого SystemUI frame в общей command page.
///
/// Kernel публикует record только после полной записи массива [`GpuUiQuad`].
/// Renderd повторно проверяет геометрию и checksum: повреждённый frame
/// отбрасывается целиком и никогда не доходит до GPU command validator.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuUiFrameHeader {
    /// [`GPU_UI_STREAM_VERSION`].
    pub version: u16,
    /// Размер заголовка.
    pub size: u16,
    /// Физическая ширина render target.
    pub width: u32,
    /// Физическая высота render target.
    pub height: u32,
    /// Монотонный идентификатор кадра.
    pub frame_id: u64,
    /// Число следующих за заголовком quad records.
    pub quad_count: u32,
    /// В версии 1 весь кадр всегда полный.
    pub flags: u32,
    /// FNV-1a всех байтов массива quad.
    pub checksum: u64,
    /// Зарезервировано.
    pub reserved: [u64; 3],
}

impl GpuUiFrameHeader {
    /// Флаг полного, самодостаточного кадра.
    pub const FULL_FRAME: u32 = 1;

    /// Создаёт заголовок завершённого кадра.
    pub const fn new(width: u32, height: u32, frame_id: u64, quad_count: u32) -> Self {
        Self {
            version: GPU_UI_STREAM_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            width,
            height,
            frame_id,
            quad_count,
            flags: Self::FULL_FRAME,
            checksum: 0,
            reserved: [0; 3],
        }
    }

    /// Проверяет только metadata; каждый quad проверяется отдельно.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        if self.version != GPU_UI_STREAM_VERSION {
            return Err(GpuAbiError::UnsupportedVersion);
        }
        if self.size as usize != core::mem::size_of::<Self>() {
            return Err(GpuAbiError::UnsupportedSize);
        }
        let bytes = (self.quad_count as usize)
            .checked_mul(core::mem::size_of::<GpuUiQuad>())
            .and_then(|bytes| bytes.checked_add(core::mem::size_of::<Self>()))
            .ok_or(GpuAbiError::InvalidValue)?;
        if self.width == 0
            || self.height == 0
            || self.width > 16_384
            || self.height > 16_384
            || self.frame_id == 0
            || self.quad_count == 0
            || bytes > GPU_UI_STREAM_BYTES
            || self.flags != Self::FULL_FRAME
            || self.reserved != [0; 3]
        {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }
}

/// Один physical quad SystemUI.
///
/// Четыре premultiplied RGBA8 цвета позволяют одной и той же GPU primitive
/// представить сплошную заливку и плавный wallpaper/toolbar gradient. Текст
/// раннего вертикального среза передаётся горизонтальными coverage spans;
/// после подключения glyph atlas wire record останется пригоден как fallback.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GpuUiQuad {
    /// Левая координата.
    pub x: u16,
    /// Верхняя координата.
    pub y: u16,
    /// Ширина.
    pub width: u16,
    /// Высота.
    pub height: u16,
    /// Цвета: top-left, top-right, bottom-right, bottom-left.
    pub colors: [u32; 4],
    /// Комбинация [`ui_quad_flag`] текущей версии.
    pub flags: u32,
    /// Зарезервировано.
    pub reserved: u32,
}

impl GpuUiQuad {
    /// Создаёт одноцветный quad.
    pub const fn solid(x: u16, y: u16, width: u16, height: u16, color: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            colors: [color; 4],
            flags: 0,
            reserved: 0,
        }
    }

    /// Создаёт ссылку на системную texture wallpaper. Сам bitmap принадлежит
    /// `renderd` и не копируется в command stream каждого кадра.
    pub const fn wallpaper(x: u16, y: u16, width: u16, height: u16, id: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            colors: [id, 0, 0, 0],
            flags: ui_quad_flag::WALLPAPER_TEXTURE,
            reserved: 0,
        }
    }

    /// Проверяет bounded physical geometry.
    pub fn validate(&self, frame_width: u32, frame_height: u32) -> Result<(), GpuAbiError> {
        let right = u32::from(self.x)
            .checked_add(u32::from(self.width))
            .ok_or(GpuAbiError::InvalidValue)?;
        let bottom = u32::from(self.y)
            .checked_add(u32::from(self.height))
            .ok_or(GpuAbiError::InvalidValue)?;
        if self.width == 0
            || self.height == 0
            || right > frame_width
            || bottom > frame_height
            || self.flags & !ui_quad_flag::KNOWN != 0
            || self.reserved != 0
            || (self.flags == ui_quad_flag::WALLPAPER_TEXTURE
                && (self.colors[0] > 2 || self.colors[1..] != [0; 3]))
        {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }

    /// Возвращает true для сплошной заливки без интерполяции углов.
    pub const fn is_solid(&self) -> bool {
        self.flags == 0
            && self.colors[0] == self.colors[1]
            && self.colors[0] == self.colors[2]
            && self.colors[0] == self.colors[3]
    }

    /// Возвращает идентификатор системной wallpaper texture.
    pub const fn wallpaper_id(&self) -> Option<u32> {
        if self.flags == ui_quad_flag::WALLPAPER_TEXTURE {
            Some(self.colors[0])
        } else {
            None
        }
    }
}

/// Детерминированный checksum SystemUI command stream.
pub fn gpu_ui_checksum(quads: &[GpuUiQuad]) -> u64 {
    let bytes = unsafe {
        core::slice::from_raw_parts(quads.as_ptr().cast::<u8>(), core::mem::size_of_val(quads))
    };
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

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
    /// [`frame_flag`]; ноль сохраняет диагностический треугольник.
    pub flags: u32,
    /// Короткое диагностическое имя; UTF-8, хвост заполнен нулями.
    pub debug_name: [u8; 48],
    /// `[scene_frame, acquire_value, 0, 0]`; acquire равен нулю в запросе.
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
    /// Texture/sample source.
    pub const SAMPLER_VIEW: u32 = 1 << 3;
    /// Vertex buffer.
    pub const VERTEX_BUFFER: u32 = 1 << 4;
    /// Ресурс можно показать на output.
    pub const DISPLAY_TARGET: u32 = 1 << 8;
    /// Разрешённый ABI subset.
    pub const KNOWN: u32 = RENDER_TARGET | SAMPLER_VIEW | VERTEX_BUFFER | DISPLAY_TARGET;
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
    /// BGRA8888 с настоящим alpha-каналом.
    pub const B8G8R8A8_UNORM: u32 = 1;
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

    /// Описывает поверхность окна: GPU рисует в неё, а compositor затем
    /// читает тот же resource как texture без копирования pixels.
    pub const fn window_surface() -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            target: resource_target::TEXTURE_2D,
            bind: resource_bind::RENDER_TARGET | resource_bind::SAMPLER_VIEW,
            reserved: [0; 2],
        }
    }

    /// Описывает read-only texture, например glyph/icon atlas или готовую
    /// клиентскую поверхность.
    pub const fn sampled_texture() -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            target: resource_target::TEXTURE_2D,
            bind: resource_bind::SAMPLER_VIEW,
            reserved: [0; 2],
        }
    }

    /// Проверяет запрос.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags != 0 || self.reserved != [0; 2] {
            return Err(GpuAbiError::ReservedNonZero);
        }
        let render_target = self.bind & resource_bind::RENDER_TARGET != 0;
        let sampled = self.bind & resource_bind::SAMPLER_VIEW != 0;
        let display = self.bind & resource_bind::DISPLAY_TARGET != 0;
        if self.target != resource_target::TEXTURE_2D
            || self.bind == 0
            || self.bind & !resource_bind::KNOWN != 0
            || self.bind & resource_bind::VERTEX_BUFFER != 0
            || (!render_target && !sampled)
            || (display && !render_target)
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

    /// Создаёт device-side texture. Содержимое загружается отдельными
    /// bounded `RESOURCE_INLINE_WRITE`, поэтому большой atlas не требует
    /// небезопасного process pointer или одного гигантского submission.
    pub const fn sampled_texture(width: u32, height: u32, format: u32) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            target: resource_target::TEXTURE_2D,
            format,
            bind: resource_bind::SAMPLER_VIEW,
            width,
            height,
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
        let vertex_buffer = self.target == resource_target::BUFFER
            && self.format == virgl_format::R8_UNORM
            && self.bind == resource_bind::VERTEX_BUFFER
            && self.width != 0
            && self.width <= 1024 * 1024
            && self.height == 1;
        let sampled_texture = self.target == resource_target::TEXTURE_2D
            && matches!(
                self.format,
                virgl_format::B8G8R8A8_UNORM
                    | virgl_format::B8G8R8X8_UNORM
                    | virgl_format::R8_UNORM
            )
            && self.bind == resource_bind::SAMPLER_VIEW
            && (1..=4096).contains(&self.width)
            && (1..=4096).contains(&self.height);
        if (!vertex_buffer && !sampled_texture) || self.depth != 1 || self.array_size != 1 {
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

    /// Создаёт запрос анимированного showcase frame.
    pub const fn aurora_request(width: u32, height: u32, frame_id: u64, scene_frame: u32) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: frame_flag::AURORA_SHOWCASE,
            width,
            height,
            frame_id,
            fence_id: 0,
            reserved: [scene_frame as u64, 0, 0, 0],
        }
    }

    /// Создаёт аппаратную проверку compositor blit path без CPU pixels.
    pub const fn compositor_probe_request(width: u32, height: u32, frame_id: u64) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: frame_flag::COMPOSITOR_PROBE,
            width,
            height,
            frame_id,
            fence_id: 0,
            reserved: [0; 4],
        }
    }

    /// Создаёт запрос штатного SystemUI frame из общей command page.
    pub const fn system_ui_request(width: u32, height: u32, frame_id: u64) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: frame_flag::SYSTEM_UI,
            width,
            height,
            frame_id,
            fence_id: 0,
            reserved: [0; 4],
        }
    }

    /// Номер анимационного кадра либо ноль diagnostic pipeline.
    pub const fn scene_frame(&self) -> u32 {
        self.reserved[0] as u32
    }

    /// Значение acquire timeline, опубликованное renderd в ответе.
    pub const fn acquire_value(&self) -> u64 {
        self.reserved[1]
    }

    /// Проверяет общий record.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags & !frame_flag::KNOWN != 0
            || self.flags.count_ones() > 1
            || self.reserved[2..] != [0; 2]
            || self.reserved[0] > u64::from(u32::MAX)
            || (self.flags == 0 && self.reserved[0] != 0)
            || (self.fence_id == 0 && self.reserved[1] != 0)
            || (self.fence_id != 0 && self.reserved[1] == 0)
        {
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

/// Bounded запуск системной 3D-демонстрации.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuDemoRequest {
    /// [`GPU_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// Флаги из [`demo_flag`].
    pub flags: u32,
    /// Число кадров, после которого scanout возвращается desktop.
    pub frame_count: u32,
    /// Желаемый интервал; vblank остаётся окончательным pacing source.
    pub frame_interval_ms: u32,
    /// Ширина render target. Ноль означает текущий scanout.
    pub width: u32,
    /// Высота render target. Ноль означает текущий scanout.
    pub height: u32,
    /// Первый кадр анимации. Позволяет оконному серверу запрашивать по одному
    /// кадру без пересоздания GPU context.
    pub first_frame: u32,
    /// Зарезервировано для выравнивания; равно нулю.
    pub reserved0: u32,
    /// Детерминированный seed сцены.
    pub seed: u64,
    /// Зарезервировано.
    pub reserved: [u64; 3],
}

/// Режимы запуска системной 3D-демонстрации.
pub mod demo_flag {
    /// Рендерить в off-screen GraphicsBuffer и вернуть управление desktop,
    /// не передавая буфер displayd. Оконный compositor заберёт готовые pixels
    /// после GPU fence и включит их в обычный damage/present.
    pub const WINDOWED: u32 = 1 << 0;
    /// Выполнить bounded multi-surface compositor pass вместо Aurora scene.
    /// Используется аппаратным integration test и всегда требует WINDOWED.
    pub const COMPOSITOR_PROBE: u32 = 1 << 1;
    /// Показать полный SystemUI frame из общей command page renderd.
    pub const SYSTEM_UI: u32 = 1 << 2;
    /// Все известные биты текущего ABI.
    pub const KNOWN: u32 = WINDOWED | COMPOSITOR_PROBE | SYSTEM_UI;
}

impl GpuDemoRequest {
    /// Создаёт запрос с разумным 60 Hz pacing hint.
    pub const fn new(frame_count: u32) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            frame_count,
            frame_interval_ms: 16,
            width: 0,
            height: 0,
            first_frame: 0,
            reserved0: 0,
            seed: 0x4155_524f_5241_3344,
            reserved: [0; 3],
        }
    }

    /// Создаёт запрос одного кадра для обычного desktop-окна.
    pub const fn windowed(width: u32, height: u32, frame: u32) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: demo_flag::WINDOWED,
            frame_count: 1,
            frame_interval_ms: 16,
            width,
            height,
            first_frame: frame,
            reserved0: 0,
            seed: 0x4155_524f_5241_3344,
            reserved: [0; 3],
        }
    }

    /// Создаёт один GPU compositor probe того же размера, что будущий target.
    pub const fn compositor_probe(width: u32, height: u32) -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: demo_flag::WINDOWED | demo_flag::COMPOSITOR_PROBE,
            frame_count: 1,
            frame_interval_ms: 16,
            width,
            height,
            first_frame: 0,
            reserved0: 0,
            seed: 0x4750_5543_4f4d_5032,
            reserved: [0; 3],
        }
    }

    /// Создаёт один полноэкранный кадр штатного desktop.
    pub const fn system_ui() -> Self {
        Self {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: demo_flag::SYSTEM_UI,
            frame_count: 1,
            frame_interval_ms: 16,
            width: 0,
            height: 0,
            first_frame: 0,
            reserved0: 0,
            seed: 0x5359_5354_454d_5549,
            reserved: [0; 3],
        }
    }

    /// Проверяет bounded duration и все зарезервированные поля.
    pub fn validate(&self) -> Result<(), GpuAbiError> {
        validate_prefix(self.version, self.size, core::mem::size_of::<Self>())?;
        if self.flags & !demo_flag::KNOWN != 0 || self.reserved0 != 0 || self.reserved != [0; 3] {
            return Err(GpuAbiError::ReservedNonZero);
        }
        if self.frame_count == 0
            || self.frame_count > 600
            || !(1..=1000).contains(&self.frame_interval_ms)
        {
            return Err(GpuAbiError::InvalidValue);
        }
        let windowed = self.flags & demo_flag::WINDOWED != 0;
        let compositor_probe = self.flags & demo_flag::COMPOSITOR_PROBE != 0;
        let system_ui = self.flags & demo_flag::SYSTEM_UI != 0;
        if compositor_probe && system_ui {
            return Err(GpuAbiError::InvalidValue);
        }
        if compositor_probe && (!windowed || self.frame_count != 1 || self.first_frame != 0) {
            return Err(GpuAbiError::InvalidValue);
        }
        if system_ui && (windowed || self.frame_count != 1 || self.first_frame != 0) {
            return Err(GpuAbiError::InvalidValue);
        }
        if windowed {
            if !(64..=2048).contains(&self.width) || !(64..=2048).contains(&self.height) {
                return Err(GpuAbiError::InvalidValue);
            }
        } else if self.width != 0 || self.height != 0 || self.first_frame != 0 {
            return Err(GpuAbiError::InvalidValue);
        }
        Ok(())
    }

    /// Копирует wire record в inline IPC payload.
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

    /// Декодирует и валидирует inline request.
    pub fn decode_inline(bytes: &[u8; 64]) -> Result<Self, GpuAbiError> {
        let value = unsafe { (bytes.as_ptr() as *const Self).read_unaligned() };
        value.validate()?;
        Ok(value)
    }
}

const _: () = assert!(core::mem::size_of::<GpuUiFrameHeader>() == 64);
const _: () = assert!(core::mem::size_of::<GpuUiQuad>() == 32);

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
const _: () = assert!(core::mem::size_of::<GpuDemoRequest>() == 64);

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
        assert_eq!(GpuResourceImport::window_surface().validate(), Ok(()));
        assert_eq!(GpuResourceImport::sampled_texture().validate(), Ok(()));
        assert_eq!(GpuResourceCreate::vertex_buffer(96).validate(), Ok(()));
        assert_eq!(
            GpuResourceCreate::sampled_texture(1024, 1024, virgl_format::B8G8R8A8_UNORM).validate(),
            Ok(())
        );
        assert_eq!(
            GpuResourceCreate::vertex_buffer(0).validate(),
            Err(GpuAbiError::InvalidValue)
        );
        assert_eq!(
            GpuResourceCreate::sampled_texture(8192, 1, virgl_format::R8_UNORM).validate(),
            Err(GpuAbiError::InvalidValue)
        );
        let mut invalid = GpuResourceImport::sampled_texture();
        invalid.bind |= resource_bind::DISPLAY_TARGET;
        assert_eq!(invalid.validate(), Err(GpuAbiError::InvalidValue));
    }

    #[test]
    fn render_frame_roundtrip_is_pointer_free() {
        let request = GpuRenderFrame::request(1280, 800, 7);
        assert_eq!(
            GpuRenderFrame::decode_inline(&request.encode_inline()),
            Ok(request)
        );
    }

    #[test]
    fn showcase_and_demo_requests_validate_reserved_fields() {
        let frame = GpuRenderFrame::aurora_request(1280, 800, 9, 37);
        assert_eq!(frame.validate(), Ok(()));
        assert_eq!(frame.scene_frame(), 37);
        assert_eq!(
            GpuRenderFrame::compositor_probe_request(1280, 800, 10).validate(),
            Ok(())
        );
        let request = GpuDemoRequest::new(180);
        assert_eq!(
            GpuDemoRequest::decode_inline(&request.encode_inline()),
            Ok(request)
        );
        let mut invalid = request;
        invalid.frame_count = 0;
        assert_eq!(invalid.validate(), Err(GpuAbiError::InvalidValue));
        let windowed = GpuDemoRequest::windowed(800, 450, 91);
        assert_eq!(windowed.validate(), Ok(()));
        assert_eq!(
            GpuDemoRequest::decode_inline(&windowed.encode_inline()),
            Ok(windowed)
        );
        assert_eq!(
            GpuDemoRequest::compositor_probe(1280, 800).validate(),
            Ok(())
        );
        assert_eq!(GpuDemoRequest::system_ui().validate(), Ok(()));
    }

    #[test]
    fn system_ui_stream_is_bounded_and_checksummed() {
        let quads = [
            GpuUiQuad::solid(0, 0, 1280, 720, 0xff20_1810),
            GpuUiQuad::solid(100, 80, 640, 420, 0xfff8_f8f8),
        ];
        let mut header = GpuUiFrameHeader::new(1280, 720, 7, quads.len() as u32);
        header.checksum = gpu_ui_checksum(&quads);
        assert_eq!(header.validate(), Ok(()));
        assert_eq!(quads[0].validate(header.width, header.height), Ok(()));
        assert_ne!(header.checksum, 0);

        let wallpaper = GpuUiQuad::wallpaper(0, 0, 1280, 720, 2);
        assert_eq!(wallpaper.wallpaper_id(), Some(2));
        assert_eq!(wallpaper.validate(header.width, header.height), Ok(()));
        let invalid_wallpaper = GpuUiQuad::wallpaper(0, 0, 1280, 720, 3);
        assert_eq!(
            invalid_wallpaper.validate(header.width, header.height),
            Err(GpuAbiError::InvalidValue)
        );

        let mut outside = quads[1];
        outside.x = 1200;
        assert_eq!(
            outside.validate(header.width, header.height),
            Err(GpuAbiError::InvalidValue)
        );
    }
}

//! Клиентский protocol очереди surface buffers.
//!
//! Окно и surface — разные объекты. Оконный сервер управляет геометрией и
//! policy, а compositor принимает immutable на время показа buffer commits.
//! Пиксели лежат в graphics buffer capability и не копируются через IPC.

/// Первая версия surface ABI.
pub const SURFACE_ABI_VERSION: u16 = 1;
/// Клиент → compositord: создать surface и buffer queue.
pub const SURFACE_CREATE_OPCODE: u16 = 0x5200;
/// Compositord → клиент: назначенный ID и generation.
pub const SURFACE_CREATED_OPCODE: u16 = 0x5201;
/// Клиент → compositord: атомарно опубликовать buffer.
pub const SURFACE_COMMIT_OPCODE: u16 = 0x5202;
/// Клиент → compositord: закрыть surface.
pub const SURFACE_DESTROY_OPCODE: u16 = 0x5203;
/// Compositord → клиент: buffer снова доступен renderer'у.
pub const SURFACE_BUFFER_RELEASED_OPCODE: u16 = 0x5204;
/// Compositord → клиент: точный результат presentation.
pub const SURFACE_PRESENTATION_FEEDBACK_OPCODE: u16 = 0x5205;
/// Create переносит endpoint событий с правом SEND.
pub const SURFACE_CREATE_HANDLE_COUNT: u16 = 1;
/// Full-damage commit переносит GraphicsBuffer и acquire SyncTimeline.
pub const SURFACE_COMMIT_FULL_HANDLE_COUNT: u16 = 2;
/// Partial-damage commit дополнительно переносит read-only damage memory.
pub const SURFACE_COMMIT_PARTIAL_HANDLE_COUNT: u16 = 3;
/// Buffer release переносит только release SyncTimeline; исходный buffer
/// остаётся у клиента и адресуется стабильным slot index.
pub const SURFACE_RELEASE_HANDLE_COUNT: u16 = 1;
/// Максимальное число damage rectangles в одном commit.
pub const SURFACE_MAX_DAMAGE_RECTS: u16 = 256;
/// Минимальная глубина client buffer queue.
pub const SURFACE_MIN_QUEUE_DEPTH: u16 = 2;
/// Максимальная глубина, принимаемая compositor'ом без отдельной квоты.
pub const SURFACE_MAX_QUEUE_DEPTH: u16 = 8;

/// Stable ID surface внутри соединения клиента с compositord.
///
/// ID не является kernel handle: сервер всегда связывает его с доверенным
/// `sender_pid`, поэтому другой процесс не может обратиться к чужой surface.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceId(pub u64);

impl SurfaceId {
    /// Нулевой ID не назначается.
    pub const INVALID: Self = Self(0);

    /// Проверяет назначенный ID.
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Stable ID физического output внутри display session.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputId(pub u64);

impl OutputId {
    /// Output ещё не выбран или frame не был показан.
    pub const NONE: Self = Self(0);

    /// Проверяет ненулевой ID.
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Прямоугольник damage в физических пикселях surface buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageRect {
    /// Левая координата.
    pub x: u32,
    /// Верхняя координата.
    pub y: u32,
    /// Ширина.
    pub width: u32,
    /// Высота.
    pub height: u32,
}

impl DamageRect {
    /// Создаёт physical damage rectangle.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Проверяет непустую область и отсутствие выхода за surface.
    pub fn validate_within(
        self,
        surface_width: u32,
        surface_height: u32,
    ) -> Result<(), SurfaceAbiError> {
        if self.width == 0 || self.height == 0 {
            return Err(SurfaceAbiError::InvalidDamage);
        }
        let Some(right) = self.x.checked_add(self.width) else {
            return Err(SurfaceAbiError::InvalidDamage);
        };
        let Some(bottom) = self.y.checked_add(self.height) else {
            return Err(SurfaceAbiError::InvalidDamage);
        };
        if right > surface_width || bottom > surface_height {
            return Err(SurfaceAbiError::InvalidDamage);
        }
        Ok(())
    }
}

/// Преобразование buffer'а перед композицией.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceTransform(pub u16);

impl SurfaceTransform {
    /// Без поворота и отражения.
    pub const NORMAL: Self = Self(1);
    /// Поворот по часовой стрелке на 90 градусов.
    pub const ROTATE_90: Self = Self(2);
    /// Поворот на 180 градусов.
    pub const ROTATE_180: Self = Self(3);
    /// Поворот на 270 градусов.
    pub const ROTATE_270: Self = Self(4);
    /// Горизонтальное отражение.
    pub const FLIP_HORIZONTAL: Self = Self(5);
    /// Горизонтальное отражение, затем поворот на 90 градусов.
    pub const FLIP_HORIZONTAL_ROTATE_90: Self = Self(6);
    /// Горизонтальное отражение, затем поворот на 180 градусов.
    pub const FLIP_HORIZONTAL_ROTATE_180: Self = Self(7);
    /// Горизонтальное отражение, затем поворот на 270 градусов.
    pub const FLIP_HORIZONTAL_ROTATE_270: Self = Self(8);

    /// Проверяет известное преобразование.
    pub const fn is_known(self) -> bool {
        matches!(
            self,
            Self::NORMAL
                | Self::ROTATE_90
                | Self::ROTATE_180
                | Self::ROTATE_270
                | Self::FLIP_HORIZONTAL
                | Self::FLIP_HORIZONTAL_ROTATE_90
                | Self::FLIP_HORIZONTAL_ROTATE_180
                | Self::FLIP_HORIZONTAL_ROTATE_270
        )
    }
}

/// Политика очереди и показа кадров.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentMode(pub u16);

impl PresentMode {
    /// Каждый frame показывается по порядку с синхронизацией refresh.
    pub const FIFO: Self = Self(1);
    /// В очереди остаётся только самый новый готовый frame.
    pub const MAILBOX: Self = Self(2);
    /// Показ без ожидания refresh; tearing разрешён.
    pub const IMMEDIATE: Self = Self(3);
    /// Compositor выбирает FIFO/mailbox по текущей нагрузке.
    pub const ADAPTIVE: Self = Self(4);

    /// Проверяет известный present mode.
    pub const fn is_known(self) -> bool {
        matches!(
            self,
            Self::FIFO | Self::MAILBOX | Self::IMMEDIATE | Self::ADAPTIVE
        )
    }
}

/// Связь логического layout и физической raster surface.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceMetrics {
    /// Ширина content в логических единицах.
    pub logical_width: u32,
    /// Высота content в логических единицах.
    pub logical_height: u32,
    /// Ширина buffer в физических пикселях.
    pub physical_width: u32,
    /// Высота buffer в физических пикселях.
    pub physical_height: u32,
    /// Physical pixels на 1000 logical units, например 1600 означает 1.6x.
    pub scale_milli: u32,
}

impl SurfaceMetrics {
    /// Создаёт полностью явные logical/physical metrics.
    pub const fn new(
        logical_width: u32,
        logical_height: u32,
        physical_width: u32,
        physical_height: u32,
        scale_milli: u32,
    ) -> Self {
        Self {
            logical_width,
            logical_height,
            physical_width,
            physical_height,
            scale_milli,
        }
    }

    /// Проверяет непустые размеры и поддерживаемый scale.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        if self.logical_width == 0
            || self.logical_height == 0
            || self.physical_width == 0
            || self.physical_height == 0
            || !(250..=8000).contains(&self.scale_milli)
        {
            return Err(SurfaceAbiError::InvalidDimensions);
        }
        let expected_width = u64::from(self.logical_width)
            .checked_mul(u64::from(self.scale_milli))
            .and_then(|scaled| scaled.checked_add(999))
            .map(|scaled| scaled / 1000);
        let expected_height = u64::from(self.logical_height)
            .checked_mul(u64::from(self.scale_milli))
            .and_then(|scaled| scaled.checked_add(999))
            .map(|scaled| scaled / 1000);
        if expected_width != Some(u64::from(self.physical_width))
            || expected_height != Some(u64::from(self.physical_height))
        {
            return Err(SurfaceAbiError::InvalidDimensions);
        }
        Ok(())
    }
}

/// Запрос создания независимой surface и её buffer queue.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceCreateRequest {
    /// [`SURFACE_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Logical/physical size и fractional device scale.
    pub metrics: SurfaceMetrics,
    /// Желаемое число buffers in-flight.
    pub queue_depth: u16,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved_header: u16,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved_tail: [u64; 2],
}

impl SurfaceCreateRequest {
    /// Создаёт surface request без неинициализированных полей.
    pub const fn new(metrics: SurfaceMetrics, queue_depth: u16) -> Self {
        Self {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            metrics,
            queue_depth,
            reserved_header: 0,
            reserved_tail: [0; 2],
        }
    }

    /// Проверяет размеры, queue depth и зарезервированные поля.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved_header != 0 || self.reserved_tail != [0; 2] {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        self.metrics.validate()?;
        if self.queue_depth < SURFACE_MIN_QUEUE_DEPTH || self.queue_depth > SURFACE_MAX_QUEUE_DEPTH
        {
            return Err(SurfaceAbiError::InvalidQueueDepth);
        }
        Ok(())
    }

    /// Кодирует request в начало inline IPC payload.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u32(&mut bytes, 4, self.flags);
        put_metrics(&mut bytes, 8, self.metrics);
        put_u16(&mut bytes, 28, self.queue_depth);
        put_u16(&mut bytes, 30, self.reserved_header);
        put_u64(&mut bytes, 32, self.reserved_tail[0]);
        put_u64(&mut bytes, 40, self.reserved_tail[1]);
        bytes
    }

    /// Декодирует ровно 48 значимых байт create request.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, SurfaceAbiError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(SurfaceAbiError::UnsupportedSize);
        }
        let request = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            flags: get_u32(bytes, 4),
            metrics: get_metrics(bytes, 8),
            queue_depth: get_u16(bytes, 28),
            reserved_header: get_u16(bytes, 30),
            reserved_tail: [get_u64(bytes, 32), get_u64(bytes, 40)],
        };
        request.validate()?;
        Ok(request)
    }
}

/// Ответ create; всегда помещается в inline payload.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceCreated {
    /// [`SURFACE_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// Зарезервировано; равно нулю.
    pub flags: u32,
    /// Назначенный surface ID.
    pub surface: SurfaceId,
    /// Фактическая глубина очереди.
    pub queue_depth: u16,
    /// Зарезервировано; равно нулю.
    pub reserved_header: [u16; 3],
    /// Generation меняется после resize/recreate.
    pub generation: u64,
    /// Зарезервировано; заполнено нулями.
    pub reserved: [u64; 4],
}

impl SurfaceCreated {
    /// Формирует успешный ответ.
    pub const fn new(surface: SurfaceId, queue_depth: u16, generation: u64) -> Self {
        Self {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            surface,
            queue_depth,
            reserved_header: [0; 3],
            generation,
            reserved: [0; 4],
        }
    }

    /// Проверяет ответ до сохранения ID клиентом.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved_header != [0; 3] || self.reserved != [0; 4] {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() || self.generation == 0 {
            return Err(SurfaceAbiError::InvalidSurface);
        }
        if !(SURFACE_MIN_QUEUE_DEPTH..=SURFACE_MAX_QUEUE_DEPTH).contains(&self.queue_depth) {
            return Err(SurfaceAbiError::InvalidQueueDepth);
        }
        Ok(())
    }

    /// Кодирует ответ в inline IPC payload.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u32(&mut bytes, 4, self.flags);
        put_u64(&mut bytes, 8, self.surface.0);
        put_u16(&mut bytes, 16, self.queue_depth);
        put_u16(&mut bytes, 18, self.reserved_header[0]);
        put_u16(&mut bytes, 20, self.reserved_header[1]);
        put_u16(&mut bytes, 22, self.reserved_header[2]);
        put_u64(&mut bytes, 24, self.generation);
        for (index, value) in self.reserved.into_iter().enumerate() {
            put_u64(&mut bytes, 32 + index * 8, value);
        }
        bytes
    }

    /// Декодирует и проверяет create reply.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, SurfaceAbiError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(SurfaceAbiError::UnsupportedSize);
        }
        let created = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            flags: get_u32(bytes, 4),
            surface: SurfaceId(get_u64(bytes, 8)),
            queue_depth: get_u16(bytes, 16),
            reserved_header: [get_u16(bytes, 18), get_u16(bytes, 20), get_u16(bytes, 22)],
            generation: get_u64(bytes, 24),
            reserved: [
                get_u64(bytes, 32),
                get_u64(bytes, 40),
                get_u64(bytes, 48),
                get_u64(bytes, 56),
            ],
        };
        created.validate()?;
        Ok(created)
    }
}

/// Запрос закрытия surface. Последние buffers освобождаются после GPU fences,
/// а не синхронно с получением этого сообщения.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceDestroyRequest {
    /// [`SURFACE_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры.
    pub size: u16,
    /// Зарезервировано; равно нулю.
    pub flags: u32,
    /// Закрываемая surface.
    pub surface: SurfaceId,
    /// Зарезервировано; заполнено нулями.
    pub reserved: [u64; 6],
}

impl SurfaceDestroyRequest {
    /// Формирует запрос закрытия.
    pub const fn new(surface: SurfaceId) -> Self {
        Self {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            surface,
            reserved: [0; 6],
        }
    }

    /// Проверяет ID и зарезервированные поля.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != [0; 6] {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() {
            return Err(SurfaceAbiError::InvalidSurface);
        }
        Ok(())
    }

    /// Кодирует request в inline payload.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u32(&mut bytes, 4, self.flags);
        put_u64(&mut bytes, 8, self.surface.0);
        for (index, value) in self.reserved.into_iter().enumerate() {
            put_u64(&mut bytes, 16 + index * 8, value);
        }
        bytes
    }

    /// Декодирует и проверяет destroy request.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, SurfaceAbiError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(SurfaceAbiError::UnsupportedSize);
        }
        let request = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            flags: get_u32(bytes, 4),
            surface: SurfaceId(get_u64(bytes, 8)),
            reserved: [
                get_u64(bytes, 16),
                get_u64(bytes, 24),
                get_u64(bytes, 32),
                get_u64(bytes, 40),
                get_u64(bytes, 48),
                get_u64(bytes, 56),
            ],
        };
        request.validate()?;
        Ok(request)
    }
}

/// Флаги [`SurfaceCommit`].
pub mod commit_flags {
    /// Весь buffer изменён; damage list должен отсутствовать.
    pub const FULL_DAMAGE: u32 = 1 << 0;
    /// Клиенту нужен [`super::PresentationFeedback`] для frame.
    pub const REQUEST_FEEDBACK: u32 = 1 << 1;
    /// Все известные биты первой версии.
    pub const KNOWN: u32 = FULL_DAMAGE | REQUEST_FEEDBACK;
}

/// Атомарная публикация одного полностью сформированного frame.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceCommit {
    /// [`SURFACE_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// Биты [`commit_flags`].
    pub flags: u32,
    /// ID, выданный [`SurfaceCreated`].
    pub surface: SurfaceId,
    /// Logical/physical size и fractional device scale.
    pub metrics: SurfaceMetrics,
    /// [`SurfaceTransform`].
    pub transform: SurfaceTransform,
    /// [`PresentMode`].
    pub present_mode: PresentMode,
    /// Число [`DamageRect`] в третьем transferred handle.
    pub damage_count: u16,
    /// Индекс buffer в созданной очереди.
    pub buffer_slot: u16,
    /// Монотонный ID frame внутри surface.
    pub frame_id: u64,
    /// Желаемое время показа по монотонным часам, ns; ноль = ближайшее.
    pub target_present_time_ns: u64,
}

impl SurfaceCommit {
    /// Создаёт full-damage commit. Клиент может затем задать shared damage
    /// list и снять флаг `FULL_DAMAGE`.
    pub const fn full_damage(
        surface: SurfaceId,
        metrics: SurfaceMetrics,
        frame_id: u64,
        buffer_slot: u16,
    ) -> Self {
        Self {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: commit_flags::FULL_DAMAGE,
            surface,
            metrics,
            transform: SurfaceTransform::NORMAL,
            present_mode: PresentMode::FIFO,
            damage_count: 0,
            buffer_slot,
            frame_id,
            target_present_time_ns: 0,
        }
    }

    /// Проверяет packet до импорта buffer и чтения damage memory.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags & !commit_flags::KNOWN != 0 {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() || self.frame_id == 0 {
            return Err(SurfaceAbiError::InvalidSurface);
        }
        self.metrics.validate()?;
        if !self.transform.is_known() || !self.present_mode.is_known() {
            return Err(SurfaceAbiError::UnsupportedMode);
        }
        if self.damage_count > SURFACE_MAX_DAMAGE_RECTS {
            return Err(SurfaceAbiError::InvalidDamage);
        }
        let full_damage = self.flags & commit_flags::FULL_DAMAGE != 0;
        if (full_damage && self.damage_count != 0) || (!full_damage && self.damage_count == 0) {
            return Err(SurfaceAbiError::InvalidDamage);
        }
        Ok(())
    }

    /// Требуемое число transferred handles для выбранного damage mode.
    pub const fn handle_count(self) -> u16 {
        if self.flags & commit_flags::FULL_DAMAGE != 0 {
            SURFACE_COMMIT_FULL_HANDLE_COUNT
        } else {
            SURFACE_COMMIT_PARTIAL_HANDLE_COUNT
        }
    }

    /// Кодирует ровно 64 байта surface commit metadata. Buffer/acquire/damage
    /// capabilities остаются в `Message::handles` и не дублируются числами.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u32(&mut bytes, 4, self.flags);
        put_u64(&mut bytes, 8, self.surface.0);
        put_metrics(&mut bytes, 16, self.metrics);
        put_u16(&mut bytes, 36, self.transform.0);
        put_u16(&mut bytes, 38, self.present_mode.0);
        put_u16(&mut bytes, 40, self.damage_count);
        put_u16(&mut bytes, 42, self.buffer_slot);
        put_u64(&mut bytes, 48, self.frame_id);
        put_u64(&mut bytes, 56, self.target_present_time_ns);
        bytes
    }

    /// Декодирует и валидирует inline commit metadata.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, SurfaceAbiError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(SurfaceAbiError::UnsupportedSize);
        }
        let commit = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            flags: get_u32(bytes, 4),
            surface: SurfaceId(get_u64(bytes, 8)),
            metrics: get_metrics(bytes, 16),
            transform: SurfaceTransform(get_u16(bytes, 36)),
            present_mode: PresentMode(get_u16(bytes, 38)),
            damage_count: get_u16(bytes, 40),
            buffer_slot: get_u16(bytes, 42),
            frame_id: get_u64(bytes, 48),
            target_present_time_ns: get_u64(bytes, 56),
        };
        commit.validate()?;
        Ok(commit)
    }
}

/// Уведомление, что compositor закончил использовать buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferReleased {
    /// [`SURFACE_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Surface ID.
    pub surface: SurfaceId,
    /// Frame, который использовал buffer последним.
    pub frame_id: u64,
    /// Значение release timeline из transferred handle.
    pub release_value: u64,
    /// Освобождённый slot из [`SurfaceCommit::buffer_slot`].
    pub buffer_slot: u16,
    /// Зарезервировано; заполнено нулями.
    pub reserved_header: [u16; 3],
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved: [u64; 3],
}

impl BufferReleased {
    /// Проверяет release event.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved_header != [0; 3] || self.reserved != [0; 3] {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() || self.frame_id == 0 {
            return Err(SurfaceAbiError::InvalidSurface);
        }
        Ok(())
    }

    /// Кодирует release metadata; timeline capability передаётся отдельно.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u32(&mut bytes, 4, self.flags);
        put_u64(&mut bytes, 8, self.surface.0);
        put_u64(&mut bytes, 16, self.frame_id);
        put_u64(&mut bytes, 24, self.release_value);
        put_u16(&mut bytes, 32, self.buffer_slot);
        put_u16(&mut bytes, 34, self.reserved_header[0]);
        put_u16(&mut bytes, 36, self.reserved_header[1]);
        put_u16(&mut bytes, 38, self.reserved_header[2]);
        for (index, value) in self.reserved.into_iter().enumerate() {
            put_u64(&mut bytes, 40 + index * 8, value);
        }
        bytes
    }

    /// Декодирует release metadata.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, SurfaceAbiError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(SurfaceAbiError::UnsupportedSize);
        }
        let released = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            flags: get_u32(bytes, 4),
            surface: SurfaceId(get_u64(bytes, 8)),
            frame_id: get_u64(bytes, 16),
            release_value: get_u64(bytes, 24),
            buffer_slot: get_u16(bytes, 32),
            reserved_header: [get_u16(bytes, 34), get_u16(bytes, 36), get_u16(bytes, 38)],
            reserved: [get_u64(bytes, 40), get_u64(bytes, 48), get_u64(bytes, 56)],
        };
        released.validate()?;
        Ok(released)
    }
}

/// Результат показа frame.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationStatus(pub u16);

impl PresentationStatus {
    /// Frame действительно появился на output.
    pub const PRESENTED: Self = Self(1);
    /// Frame отброшен до показа.
    pub const DROPPED: Self = Self(2);
    /// Frame заменён более новым mailbox frame.
    pub const REPLACED: Self = Self(3);
    /// Display device потерян до показа.
    pub const DEVICE_LOST: Self = Self(4);

    /// Проверяет известный статус.
    pub const fn is_known(self) -> bool {
        matches!(
            self,
            Self::PRESENTED | Self::DROPPED | Self::REPLACED | Self::DEVICE_LOST
        )
    }
}

/// Флаги [`PresentationFeedback`].
pub mod feedback_flags {
    /// Buffer был показан напрямую без composition.
    pub const DIRECT_SCANOUT: u16 = 1 << 0;
    /// Buffer участвовал в composition.
    pub const COMPOSITED: u16 = 1 << 1;
    /// При показе был разрешён tearing.
    pub const TEARING: u16 = 1 << 2;
    /// Все известные биты первой версии.
    pub const KNOWN: u16 = DIRECT_SCANOUT | COMPOSITED | TEARING;
}

/// Точные сведения о судьбе одного frame.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationFeedback {
    /// [`SURFACE_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// [`PresentationStatus`].
    pub status: PresentationStatus,
    /// Биты [`feedback_flags`].
    pub flags: u16,
    /// Surface ID.
    pub surface: SurfaceId,
    /// ID клиентского frame.
    pub frame_id: u64,
    /// Монотонный display sequence.
    pub sequence: u64,
    /// Фактическое время vblank/present, ns; ноль для отброшенного frame.
    pub actual_time_ns: u64,
    /// Интервал refresh, ns; ноль если frame не показан.
    pub refresh_interval_ns: u64,
    /// Output, на котором был показан frame.
    pub output: OutputId,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved_tail: u64,
}

impl PresentationFeedback {
    /// Проверяет feedback packet перед использованием frame pacing logic.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if !self.status.is_known() {
            return Err(SurfaceAbiError::UnsupportedMode);
        }
        if self.flags & !feedback_flags::KNOWN != 0 || self.reserved_tail != 0 {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() {
            return Err(SurfaceAbiError::InvalidSurface);
        }
        if self.flags & feedback_flags::DIRECT_SCANOUT != 0
            && self.flags & feedback_flags::COMPOSITED != 0
        {
            return Err(SurfaceAbiError::InvalidFeedback);
        }
        if self.status == PresentationStatus::PRESENTED {
            if self.actual_time_ns == 0 || self.refresh_interval_ns == 0 || !self.output.is_valid()
            {
                return Err(SurfaceAbiError::InvalidFeedback);
            }
        } else if self.actual_time_ns != 0
            || self.refresh_interval_ns != 0
            || self.output.is_valid()
            || self.flags != 0
        {
            return Err(SurfaceAbiError::InvalidFeedback);
        }
        Ok(())
    }

    /// Кодирует feedback в один inline payload.
    pub fn encode_inline(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        put_u16(&mut bytes, 0, self.version);
        put_u16(&mut bytes, 2, self.size);
        put_u16(&mut bytes, 4, self.status.0);
        put_u16(&mut bytes, 6, self.flags);
        put_u64(&mut bytes, 8, self.surface.0);
        put_u64(&mut bytes, 16, self.frame_id);
        put_u64(&mut bytes, 24, self.sequence);
        put_u64(&mut bytes, 32, self.actual_time_ns);
        put_u64(&mut bytes, 40, self.refresh_interval_ns);
        put_u64(&mut bytes, 48, self.output.0);
        put_u64(&mut bytes, 56, self.reserved_tail);
        bytes
    }

    /// Декодирует и проверяет feedback.
    pub fn decode_inline(bytes: &[u8]) -> Result<Self, SurfaceAbiError> {
        if bytes.len() != core::mem::size_of::<Self>() {
            return Err(SurfaceAbiError::UnsupportedSize);
        }
        let feedback = Self {
            version: get_u16(bytes, 0),
            size: get_u16(bytes, 2),
            status: PresentationStatus(get_u16(bytes, 4)),
            flags: get_u16(bytes, 6),
            surface: SurfaceId(get_u64(bytes, 8)),
            frame_id: get_u64(bytes, 16),
            sequence: get_u64(bytes, 24),
            actual_time_ns: get_u64(bytes, 32),
            refresh_interval_ns: get_u64(bytes, 40),
            output: OutputId(get_u64(bytes, 48)),
            reserved_tail: get_u64(bytes, 56),
        };
        feedback.validate()?;
        Ok(feedback)
    }
}

/// Ошибка структурной проверки surface protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceAbiError {
    /// Версия не поддерживается.
    UnsupportedVersion,
    /// Размер packet не совпадает с выбранной версией.
    UnsupportedSize,
    /// Capability handle не задан.
    InvalidHandle,
    /// Surface ID не задан или frame ID равен нулю.
    InvalidSurface,
    /// Logical/physical size или scale недопустим.
    InvalidDimensions,
    /// Buffer queue слишком мала или превышает системный предел.
    InvalidQueueDepth,
    /// Transform, present mode или feedback status неизвестен.
    UnsupportedMode,
    /// Acquire/release point частично заполнена.
    InvalidSyncPoint,
    /// Damage list некорректен.
    InvalidDamage,
    /// Presentation feedback внутренне противоречив.
    InvalidFeedback,
    /// Зарезервированное поле или неизвестный флаг не равен нулю.
    ReservedNonZero,
}

fn validate_header(version: u16, size: u16, expected_size: u16) -> Result<(), SurfaceAbiError> {
    if version != SURFACE_ABI_VERSION {
        return Err(SurfaceAbiError::UnsupportedVersion);
    }
    if size != expected_size {
        return Err(SurfaceAbiError::UnsupportedSize);
    }
    Ok(())
}

fn put_metrics(bytes: &mut [u8], offset: usize, metrics: SurfaceMetrics) {
    put_u32(bytes, offset, metrics.logical_width);
    put_u32(bytes, offset + 4, metrics.logical_height);
    put_u32(bytes, offset + 8, metrics.physical_width);
    put_u32(bytes, offset + 12, metrics.physical_height);
    put_u32(bytes, offset + 16, metrics.scale_milli);
}

fn get_metrics(bytes: &[u8], offset: usize) -> SurfaceMetrics {
    SurfaceMetrics {
        logical_width: get_u32(bytes, offset),
        logical_height: get_u32(bytes, offset + 4),
        physical_width: get_u32(bytes, offset + 8),
        physical_height: get_u32(bytes, offset + 12),
        scale_milli: get_u32(bytes, offset + 16),
    }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap_or([0; 2]))
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or([0; 4]))
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap_or([0; 8]))
}

const _: () = assert!(core::mem::size_of::<OutputId>() == 8);
const _: () = assert!(core::mem::size_of::<SurfaceId>() == 8);
const _: () = assert!(core::mem::size_of::<DamageRect>() == 16);
const _: () = assert!(core::mem::size_of::<SurfaceMetrics>() == 20);
const _: () = assert!(core::mem::size_of::<SurfaceCreateRequest>() == 48);
const _: () = assert!(core::mem::size_of::<SurfaceCreated>() == 64);
const _: () = assert!(core::mem::size_of::<SurfaceDestroyRequest>() == 64);
const _: () = assert!(core::mem::size_of::<SurfaceCommit>() == 64);
const _: () = assert!(core::mem::size_of::<BufferReleased>() == 64);
const _: () = assert!(core::mem::size_of::<PresentationFeedback>() == 64);
const _: () = assert!(core::mem::align_of::<SurfaceCommit>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_accepts_fractional_hidpi() {
        let request =
            SurfaceCreateRequest::new(SurfaceMetrics::new(1280, 800, 2048, 1280, 1600), 3);
        assert_eq!(request.validate(), Ok(()));
        assert_eq!(
            SurfaceCreateRequest::decode_inline(&request.encode_inline()[..48]),
            Ok(request)
        );
        let created = SurfaceCreated::new(SurfaceId(42), 3, 1);
        assert_eq!(
            SurfaceCreated::decode_inline(&created.encode_inline()),
            Ok(created)
        );
    }

    #[test]
    fn metrics_reject_post_raster_bitmap_stretching() {
        let request =
            SurfaceCreateRequest::new(SurfaceMetrics::new(1280, 800, 1920, 1200, 1000), 3);
        assert_eq!(request.validate(), Err(SurfaceAbiError::InvalidDimensions));
    }

    #[test]
    fn full_damage_commit_needs_no_damage_memory() {
        let commit = SurfaceCommit::full_damage(
            SurfaceId(2),
            SurfaceMetrics::new(800, 600, 1600, 1200, 2000),
            7,
            0,
        );
        assert_eq!(commit.validate(), Ok(()));
        assert_eq!(commit.handle_count(), SURFACE_COMMIT_FULL_HANDLE_COUNT);
        assert_eq!(
            SurfaceCommit::decode_inline(&commit.encode_inline()),
            Ok(commit)
        );
    }

    #[test]
    fn partial_damage_requires_bounded_shared_array() {
        let mut commit = SurfaceCommit::full_damage(
            SurfaceId(2),
            SurfaceMetrics::new(800, 600, 800, 600, 1000),
            8,
            2,
        );
        commit.flags = commit_flags::REQUEST_FEEDBACK;
        commit.damage_count = 3;
        assert_eq!(commit.validate(), Ok(()));
        assert_eq!(commit.handle_count(), SURFACE_COMMIT_PARTIAL_HANDLE_COUNT);
        commit.damage_count = SURFACE_MAX_DAMAGE_RECTS + 1;
        assert_eq!(commit.validate(), Err(SurfaceAbiError::InvalidDamage));
    }

    #[test]
    fn damage_rect_rejects_overflow_and_out_of_bounds() {
        assert_eq!(
            DamageRect::new(10, 10, 20, 20).validate_within(30, 30),
            Ok(())
        );
        assert_eq!(
            DamageRect::new(u32::MAX, 0, 2, 1).validate_within(u32::MAX, 1),
            Err(SurfaceAbiError::InvalidDamage)
        );
        assert_eq!(
            DamageRect::new(20, 20, 11, 10).validate_within(30, 30),
            Err(SurfaceAbiError::InvalidDamage)
        );
    }

    #[test]
    fn feedback_distinguishes_presented_and_dropped_frames() {
        let presented = PresentationFeedback {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<PresentationFeedback>() as u16,
            status: PresentationStatus::PRESENTED,
            flags: feedback_flags::COMPOSITED,
            surface: SurfaceId(7),
            frame_id: 4,
            sequence: 10,
            actual_time_ns: 100,
            refresh_interval_ns: 16,
            output: OutputId(1),
            reserved_tail: 0,
        };
        assert_eq!(presented.validate(), Ok(()));
        let mut dropped = presented;
        dropped.status = PresentationStatus::DROPPED;
        dropped.flags = 0;
        dropped.actual_time_ns = 0;
        dropped.refresh_interval_ns = 0;
        dropped.output = OutputId::NONE;
        assert_eq!(dropped.validate(), Ok(()));
        dropped.flags = feedback_flags::DIRECT_SCANOUT;
        assert_eq!(dropped.validate(), Err(SurfaceAbiError::InvalidFeedback));
        assert_eq!(
            PresentationFeedback::decode_inline(&presented.encode_inline()),
            Ok(presented)
        );
    }

    #[test]
    fn destroy_and_release_are_pointer_free_inline_records() {
        let destroy = SurfaceDestroyRequest::new(SurfaceId(9));
        assert_eq!(
            SurfaceDestroyRequest::decode_inline(&destroy.encode_inline()),
            Ok(destroy)
        );
        let released = BufferReleased {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<BufferReleased>() as u16,
            flags: 0,
            surface: SurfaceId(9),
            frame_id: 8,
            release_value: 11,
            buffer_slot: 2,
            reserved_header: [0; 3],
            reserved: [0; 3],
        };
        assert_eq!(
            BufferReleased::decode_inline(&released.encode_inline()),
            Ok(released)
        );
    }
}

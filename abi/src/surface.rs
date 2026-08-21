//! Клиентский protocol очереди surface buffers.
//!
//! Окно и surface — разные объекты. Оконный сервер управляет геометрией и
//! policy, а compositor принимает immutable на время показа buffer commits.
//! Пиксели лежат в graphics buffer capability и не копируются через IPC.

use crate::{sync::SyncPoint, Handle};

/// Первая версия surface ABI.
pub const SURFACE_ABI_VERSION: u16 = 1;
/// Максимальное число damage rectangles в одном commit.
pub const SURFACE_MAX_DAMAGE_RECTS: u16 = 256;
/// Минимальная глубина client buffer queue.
pub const SURFACE_MIN_QUEUE_DEPTH: u16 = 2;
/// Максимальная глубина, принимаемая compositor'ом без отдельной квоты.
pub const SURFACE_MAX_QUEUE_DEPTH: u16 = 8;

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
    /// Surface capability.
    pub surface: Handle,
    /// Graphics buffer capability.
    pub buffer: Handle,
    /// Compositor ждёт эту point перед чтением buffer'а.
    pub acquire: SyncPoint,
    /// Read-only shared memory с массивом [`DamageRect`].
    pub damage_memory: Handle,
    /// Число элементов массива damage.
    pub damage_count: u16,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved_header: u16,
    /// Смещение массива damage в shared memory.
    pub damage_offset: u64,
    /// Logical/physical size и fractional device scale.
    pub metrics: SurfaceMetrics,
    /// [`SurfaceTransform`].
    pub transform: SurfaceTransform,
    /// [`PresentMode`].
    pub present_mode: PresentMode,
    /// Монотонный ID frame внутри surface.
    pub frame_id: u64,
    /// Желаемое время показа по монотонным часам, ns; ноль = ближайшее.
    pub target_present_time_ns: u64,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved_tail: [u64; 2],
}

impl SurfaceCommit {
    /// Создаёт full-damage commit. Клиент может затем задать shared damage
    /// list и снять флаг `FULL_DAMAGE`.
    pub const fn full_damage(
        surface: Handle,
        buffer: Handle,
        metrics: SurfaceMetrics,
        frame_id: u64,
    ) -> Self {
        Self {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: commit_flags::FULL_DAMAGE,
            surface,
            buffer,
            acquire: SyncPoint::NONE,
            damage_memory: Handle::INVALID,
            damage_count: 0,
            reserved_header: 0,
            damage_offset: 0,
            metrics,
            transform: SurfaceTransform::NORMAL,
            present_mode: PresentMode::FIFO,
            frame_id,
            target_present_time_ns: 0,
            reserved_tail: [0; 2],
        }
    }

    /// Проверяет packet до импорта buffer и чтения damage memory.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags & !commit_flags::KNOWN != 0
            || self.reserved_header != 0
            || self.reserved_tail != [0; 2]
        {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() || !self.buffer.is_valid() {
            return Err(SurfaceAbiError::InvalidHandle);
        }
        if self.acquire.validate().is_err() {
            return Err(SurfaceAbiError::InvalidSyncPoint);
        }
        self.metrics.validate()?;
        if !self.transform.is_known() || !self.present_mode.is_known() {
            return Err(SurfaceAbiError::UnsupportedMode);
        }
        if self.damage_count > SURFACE_MAX_DAMAGE_RECTS {
            return Err(SurfaceAbiError::InvalidDamage);
        }
        let full_damage = self.flags & commit_flags::FULL_DAMAGE != 0;
        if self.damage_count == 0 {
            if self.damage_memory.is_valid() || self.damage_offset != 0 {
                return Err(SurfaceAbiError::InvalidDamage);
            }
        } else {
            if full_damage
                || !self.damage_memory.is_valid()
                || !self.damage_offset.is_multiple_of(8)
            {
                return Err(SurfaceAbiError::InvalidDamage);
            }
            if self
                .damage_offset
                .checked_add(self.damage_count as u64 * core::mem::size_of::<DamageRect>() as u64)
                .is_none()
            {
                return Err(SurfaceAbiError::InvalidDamage);
            }
        }
        Ok(())
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
    /// Surface capability.
    pub surface: Handle,
    /// Освобождённый graphics buffer capability.
    pub buffer: Handle,
    /// Frame, который использовал buffer последним.
    pub frame_id: u64,
    /// После этой point buffer можно менять; `NONE` означает уже свободен.
    pub release: SyncPoint,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved: u64,
}

impl BufferReleased {
    /// Проверяет release event.
    pub fn validate(self) -> Result<(), SurfaceAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != 0 {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() || !self.buffer.is_valid() {
            return Err(SurfaceAbiError::InvalidHandle);
        }
        if self.release.validate().is_err() {
            return Err(SurfaceAbiError::InvalidSyncPoint);
        }
        Ok(())
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
    /// Surface capability.
    pub surface: Handle,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved_header: u32,
    /// ID клиентского frame.
    pub frame_id: u64,
    /// Монотонный display sequence.
    pub sequence: u64,
    /// Запрошенное время показа, ns.
    pub target_time_ns: u64,
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
        if self.flags & !feedback_flags::KNOWN != 0
            || self.reserved_header != 0
            || self.reserved_tail != 0
        {
            return Err(SurfaceAbiError::ReservedNonZero);
        }
        if !self.surface.is_valid() {
            return Err(SurfaceAbiError::InvalidHandle);
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

const _: () = assert!(core::mem::size_of::<OutputId>() == 8);
const _: () = assert!(core::mem::size_of::<DamageRect>() == 16);
const _: () = assert!(core::mem::size_of::<SurfaceMetrics>() == 20);
const _: () = assert!(core::mem::size_of::<SurfaceCreateRequest>() == 48);
const _: () = assert!(core::mem::size_of::<SurfaceCommit>() == 104);
const _: () = assert!(core::mem::size_of::<BufferReleased>() == 48);
const _: () = assert!(core::mem::size_of::<PresentationFeedback>() == 72);
const _: () = assert!(core::mem::align_of::<SurfaceCommit>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_accepts_fractional_hidpi() {
        let request =
            SurfaceCreateRequest::new(SurfaceMetrics::new(1280, 800, 2048, 1280, 1600), 3);
        assert_eq!(request.validate(), Ok(()));
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
            Handle(2),
            Handle(3),
            SurfaceMetrics::new(800, 600, 1600, 1200, 2000),
            7,
        );
        assert_eq!(commit.validate(), Ok(()));
    }

    #[test]
    fn partial_damage_requires_bounded_shared_array() {
        let mut commit = SurfaceCommit::full_damage(
            Handle(2),
            Handle(3),
            SurfaceMetrics::new(800, 600, 800, 600, 1000),
            8,
        );
        commit.flags = commit_flags::REQUEST_FEEDBACK;
        commit.damage_memory = Handle(4);
        commit.damage_count = 3;
        commit.damage_offset = 16;
        assert_eq!(commit.validate(), Ok(()));
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
            surface: Handle(7),
            reserved_header: 0,
            frame_id: 4,
            sequence: 10,
            target_time_ns: 90,
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
    }
}

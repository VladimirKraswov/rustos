//! Явная timeline-синхронизация CPU, GPU, compositor и display.
//!
//! Timeline монотонно движется вперёд. Операция ждёт значение `>= value`, а
//! signal меньшим значением никогда не откатывает объект назад. Capability
//! rights определяют, кто может ждать и кто может сигналить timeline.

use crate::Handle;

/// Первая версия sync ABI.
pub const SYNC_ABI_VERSION: u16 = 1;
/// Бесконечный timeout для [`SyncTimelineWait`].
pub const SYNC_TIMEOUT_INFINITE: u64 = u64::MAX;
/// Максимальное число points одного атомарного ожидания без отдельной квоты.
pub const SYNC_MAX_WAIT_POINTS: u16 = 64;

/// Одна точка монотонного timeline.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncPoint {
    /// Capability handle timeline object.
    pub timeline: Handle,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved: u32,
    /// Операция завершена, когда timeline достиг этого значения.
    pub value: u64,
}

impl SyncPoint {
    /// Зависимость отсутствует или buffer уже свободен.
    pub const NONE: Self = Self {
        timeline: Handle::INVALID,
        reserved: 0,
        value: 0,
    };

    /// Создаёт явную точку timeline.
    pub const fn new(timeline: Handle, value: u64) -> Self {
        Self {
            timeline,
            reserved: 0,
            value,
        }
    }

    /// Проверяет точку. `NONE` допустим, но частично заполненная пустая точка —
    /// нет: это обнаруживает забытый capability transfer.
    pub fn validate(self) -> Result<(), SyncAbiError> {
        if self.reserved != 0 {
            return Err(SyncAbiError::ReservedNonZero);
        }
        if !self.timeline.is_valid() && self.value != 0 {
            return Err(SyncAbiError::InvalidPoint);
        }
        Ok(())
    }

    /// Точка обозначает отсутствие ожидания.
    pub const fn is_none(self) -> bool {
        !self.timeline.is_valid() && self.value == 0 && self.reserved == 0
    }
}

/// Запрос создания timeline object.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncTimelineCreate {
    /// [`SYNC_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Начальное значение timeline.
    pub initial_value: u64,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved: [u64; 2],
}

impl SyncTimelineCreate {
    /// Создаёт запрос с заданным начальным значением.
    pub const fn new(initial_value: u64) -> Self {
        Self {
            version: SYNC_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            initial_value,
            reserved: [0; 2],
        }
    }

    /// Проверяет request до создания kernel object.
    pub fn validate(self) -> Result<(), SyncAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != [0; 2] {
            return Err(SyncAbiError::ReservedNonZero);
        }
        Ok(())
    }
}

/// Запрос монотонного signal timeline.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncTimelineSignal {
    /// [`SYNC_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Timeline capability с правом signal.
    pub timeline: Handle,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved: u32,
    /// Новое значение, не меньше текущего.
    pub value: u64,
}

impl SyncTimelineSignal {
    /// Создаёт signal request.
    pub const fn new(timeline: Handle, value: u64) -> Self {
        Self {
            version: SYNC_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            timeline,
            reserved: 0,
            value,
        }
    }

    /// Проверяет request. Монотонность значения проверяет сам timeline object.
    pub fn validate(self) -> Result<(), SyncAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != 0 {
            return Err(SyncAbiError::ReservedNonZero);
        }
        if !self.timeline.is_valid() {
            return Err(SyncAbiError::InvalidHandle);
        }
        Ok(())
    }
}

/// Ожидание одной timeline point без busy-wait.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncTimelineWait {
    /// [`SYNC_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Timeline capability с правом wait.
    pub timeline: Handle,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved: u32,
    /// Минимальное требуемое значение timeline.
    pub value: u64,
    /// Timeout по монотонным часам, ns; [`SYNC_TIMEOUT_INFINITE`] без timeout.
    pub timeout_ns: u64,
}

impl SyncTimelineWait {
    /// Создаёт wait request.
    pub const fn new(timeline: Handle, value: u64, timeout_ns: u64) -> Self {
        Self {
            version: SYNC_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            timeline,
            reserved: 0,
            value,
            timeout_ns,
        }
    }

    /// Проверяет request до постановки thread в wait queue.
    pub fn validate(self) -> Result<(), SyncAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != 0 {
            return Err(SyncAbiError::ReservedNonZero);
        }
        if !self.timeline.is_valid() {
            return Err(SyncAbiError::InvalidHandle);
        }
        Ok(())
    }
}

/// Условие завершения ожидания нескольких points.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncWaitMode(pub u16);

impl SyncWaitMode {
    /// Завершить ожидание после достижения всех points.
    pub const ALL: Self = Self(1);
    /// Завершить ожидание после достижения любой point.
    pub const ANY: Self = Self(2);

    /// Проверяет известный режим.
    pub const fn is_known(self) -> bool {
        matches!(self, Self::ALL | Self::ANY)
    }
}

/// Атомарное ожидание массива [`SyncPoint`] без busy-wait.
///
/// Массив находится в read-only shared memory: kernel копирует и проверяет
/// максимум [`SYNC_MAX_WAIT_POINTS`] записей до постановки thread в wait queue.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncWaitMany {
    /// [`SYNC_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// В первой версии равно нулю.
    pub flags: u32,
    /// Shared-memory capability с массивом points.
    pub points_memory: Handle,
    /// Число элементов массива.
    pub point_count: u16,
    /// [`SyncWaitMode`].
    pub mode: SyncWaitMode,
    /// Смещение массива в shared memory.
    pub points_offset: u64,
    /// Timeout по монотонным часам, ns; [`SYNC_TIMEOUT_INFINITE`] без timeout.
    pub timeout_ns: u64,
    /// Зарезервировано; отправитель заполняет нулями.
    pub reserved: [u64; 2],
}

impl SyncWaitMany {
    /// Создаёт bounded wait-set request.
    pub const fn new(
        points_memory: Handle,
        points_offset: u64,
        point_count: u16,
        mode: SyncWaitMode,
        timeout_ns: u64,
    ) -> Self {
        Self {
            version: SYNC_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            points_memory,
            point_count,
            mode,
            points_offset,
            timeout_ns,
            reserved: [0; 2],
        }
    }

    /// Проверяет shared range до чтения массива.
    pub fn validate(self) -> Result<(), SyncAbiError> {
        validate_header(self.version, self.size, core::mem::size_of::<Self>() as u16)?;
        if self.flags != 0 || self.reserved != [0; 2] {
            return Err(SyncAbiError::ReservedNonZero);
        }
        if !self.points_memory.is_valid() {
            return Err(SyncAbiError::InvalidHandle);
        }
        if self.point_count == 0 || self.point_count > SYNC_MAX_WAIT_POINTS {
            return Err(SyncAbiError::InvalidPointCount);
        }
        if !self.mode.is_known() {
            return Err(SyncAbiError::InvalidWaitMode);
        }
        if !self.points_offset.is_multiple_of(8)
            || self
                .points_offset
                .checked_add(self.point_count as u64 * core::mem::size_of::<SyncPoint>() as u64)
                .is_none()
        {
            return Err(SyncAbiError::InvalidSharedRange);
        }
        Ok(())
    }
}

/// Ошибка структурной проверки sync packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncAbiError {
    /// Версия не поддерживается.
    UnsupportedVersion,
    /// Размер структуры не совпадает с выбранной версией.
    UnsupportedSize,
    /// Capability handle не задан.
    InvalidHandle,
    /// Пустая point частично заполнена.
    InvalidPoint,
    /// Массив ожидания пуст или превышает bounded limit.
    InvalidPointCount,
    /// Режим ожидания неизвестен.
    InvalidWaitMode,
    /// Смещение массива не выровнено или его диапазон переполнен.
    InvalidSharedRange,
    /// Зарезервированное поле или неизвестный флаг не равен нулю.
    ReservedNonZero,
}

fn validate_header(version: u16, size: u16, expected_size: u16) -> Result<(), SyncAbiError> {
    if version != SYNC_ABI_VERSION {
        return Err(SyncAbiError::UnsupportedVersion);
    }
    if size != expected_size {
        return Err(SyncAbiError::UnsupportedSize);
    }
    Ok(())
}

const _: () = assert!(core::mem::size_of::<SyncPoint>() == 16);
const _: () = assert!(core::mem::size_of::<SyncTimelineCreate>() == 32);
const _: () = assert!(core::mem::size_of::<SyncTimelineSignal>() == 24);
const _: () = assert!(core::mem::size_of::<SyncTimelineWait>() == 32);
const _: () = assert!(core::mem::size_of::<SyncWaitMany>() == 48);
const _: () = assert!(core::mem::align_of::<SyncPoint>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_the_only_valid_empty_point() {
        assert_eq!(SyncPoint::NONE.validate(), Ok(()));
        assert!(SyncPoint::NONE.is_none());
        assert_eq!(
            SyncPoint::new(Handle::INVALID, 5).validate(),
            Err(SyncAbiError::InvalidPoint)
        );
        let point = SyncPoint::new(Handle(7), 5);
        assert_eq!(point.validate(), Ok(()));
        assert!(!point.is_none());
    }

    #[test]
    fn request_validation_rejects_handles_and_reserved_data() {
        assert_eq!(SyncTimelineCreate::new(3).validate(), Ok(()));
        assert_eq!(
            SyncTimelineSignal::new(Handle::INVALID, 4).validate(),
            Err(SyncAbiError::InvalidHandle)
        );
        let mut wait = SyncTimelineWait::new(Handle(9), 10, SYNC_TIMEOUT_INFINITE);
        wait.flags = 1;
        assert_eq!(wait.validate(), Err(SyncAbiError::ReservedNonZero));
    }

    #[test]
    fn wrong_version_and_size_are_rejected() {
        let mut request = SyncTimelineCreate::new(0);
        request.version += 1;
        assert_eq!(request.validate(), Err(SyncAbiError::UnsupportedVersion));
        request.version = SYNC_ABI_VERSION;
        request.size += 8;
        assert_eq!(request.validate(), Err(SyncAbiError::UnsupportedSize));
    }

    #[test]
    fn wait_many_is_bounded_aligned_and_typed() {
        let request = SyncWaitMany::new(Handle(4), 16, 3, SyncWaitMode::ALL, SYNC_TIMEOUT_INFINITE);
        assert_eq!(request.validate(), Ok(()));

        let mut invalid = request;
        invalid.point_count = SYNC_MAX_WAIT_POINTS + 1;
        assert_eq!(invalid.validate(), Err(SyncAbiError::InvalidPointCount));
        invalid = request;
        invalid.points_offset = 3;
        assert_eq!(invalid.validate(), Err(SyncAbiError::InvalidSharedRange));
        invalid = request;
        invalid.mode = SyncWaitMode(99);
        assert_eq!(invalid.validate(), Err(SyncAbiError::InvalidWaitMode));
    }
}

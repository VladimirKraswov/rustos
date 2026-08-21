//! Переносимое состояние монотонных timeline objects.
//!
//! Модуль не блокирует потоки сам: kernel scheduler хранит wait queues, а эта
//! таблица отвечает только за generation-safe lifetime и монотонность. Такое
//! разделение позволяет одинаково проверить механизм на AMD64 и AArch64.

/// Непрозрачный идентификатор timeline внутри ядра.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineId(pub u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineError {
    /// Фиксированная таблица исчерпана.
    LimitReached,
    /// ID устарел либо объект уже уничтожен.
    InvalidId,
    /// Signal попытался сдвинуть timeline назад.
    NonMonotonic,
}

#[derive(Clone, Copy)]
struct Timeline {
    generation: u8,
    used: bool,
    value: u64,
    references: u16,
}

impl Timeline {
    const EMPTY: Self = Self {
        generation: 1,
        used: false,
        value: 0,
        references: 0,
    };
}

/// Bounded generation-safe таблица timeline objects.
pub struct TimelineTable<const N: usize> {
    timelines: [Timeline; N],
}

impl<const N: usize> TimelineTable<N> {
    /// Создаёт пустую таблицу. ID кодирует 8-битные slot и generation,
    /// поэтому ёмкость намеренно ограничена 256 объектами.
    pub const fn new() -> Self {
        assert!(N <= 256);
        Self {
            timelines: [Timeline::EMPTY; N],
        }
    }

    /// Создаёт объект с одной исходной capability reference.
    pub fn create(&mut self, initial_value: u64) -> Result<TimelineId, TimelineError> {
        let index = self
            .timelines
            .iter()
            .position(|timeline| !timeline.used)
            .ok_or(TimelineError::LimitReached)?;
        let generation = self.timelines[index].generation;
        self.timelines[index] = Timeline {
            generation,
            used: true,
            value: initial_value,
            references: 1,
        };
        Ok(make_id(index, generation))
    }

    /// Удерживает объект для capability либо зарегистрированного waiter'а.
    pub fn retain(&mut self, id: TimelineId) -> Result<(), TimelineError> {
        let timeline = self.get_mut(id)?;
        timeline.references = timeline
            .references
            .checked_add(1)
            .ok_or(TimelineError::LimitReached)?;
        Ok(())
    }

    /// Снимает reference. Последняя уничтожает объект и меняет generation.
    pub fn release(&mut self, id: TimelineId) -> Result<(), TimelineError> {
        let index = id_index(id);
        let timeline = self.get_mut(id)?;
        if timeline.references == 0 {
            return Err(TimelineError::InvalidId);
        }
        timeline.references -= 1;
        if timeline.references == 0 {
            let generation = next_generation(timeline.generation);
            *timeline = Timeline::EMPTY;
            timeline.generation = generation;
        }
        debug_assert!(index < N);
        Ok(())
    }

    /// Возвращает текущее значение.
    pub fn value(&self, id: TimelineId) -> Result<u64, TimelineError> {
        Ok(self.get(id)?.value)
    }

    /// Проверяет достижение point без изменения объекта.
    pub fn reached(&self, id: TimelineId, value: u64) -> Result<bool, TimelineError> {
        Ok(self.value(id)? >= value)
    }

    /// Монотонно продвигает timeline. Повтор того же значения идемпотентен.
    pub fn signal(&mut self, id: TimelineId, value: u64) -> Result<bool, TimelineError> {
        let timeline = self.get_mut(id)?;
        if value < timeline.value {
            return Err(TimelineError::NonMonotonic);
        }
        let changed = value != timeline.value;
        timeline.value = value;
        Ok(changed)
    }

    fn get(&self, id: TimelineId) -> Result<&Timeline, TimelineError> {
        let timeline = self
            .timelines
            .get(id_index(id))
            .ok_or(TimelineError::InvalidId)?;
        if !timeline.used || timeline.generation != id_generation(id) {
            return Err(TimelineError::InvalidId);
        }
        Ok(timeline)
    }

    fn get_mut(&mut self, id: TimelineId) -> Result<&mut Timeline, TimelineError> {
        let timeline = self
            .timelines
            .get_mut(id_index(id))
            .ok_or(TimelineError::InvalidId)?;
        if !timeline.used || timeline.generation != id_generation(id) {
            return Err(TimelineError::InvalidId);
        }
        Ok(timeline)
    }
}

impl<const N: usize> Default for TimelineTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

const fn make_id(index: usize, generation: u8) -> TimelineId {
    TimelineId(((generation as u16) << 8) | index as u16)
}

const fn id_index(id: TimelineId) -> usize {
    (id.0 & 0xff) as usize
}

const fn id_generation(id: TimelineId) -> u8 {
    (id.0 >> 8) as u8
}

const fn next_generation(generation: u8) -> u8 {
    let next = generation.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_is_monotonic_and_idempotent() {
        let mut table = TimelineTable::<2>::new();
        let timeline = table.create(4).unwrap();
        assert_eq!(table.signal(timeline, 4), Ok(false));
        assert_eq!(table.signal(timeline, 9), Ok(true));
        assert_eq!(table.value(timeline), Ok(9));
        assert_eq!(table.signal(timeline, 8), Err(TimelineError::NonMonotonic));
    }

    #[test]
    fn waiter_reference_keeps_object_alive() {
        let mut table = TimelineTable::<1>::new();
        let timeline = table.create(0).unwrap();
        table.retain(timeline).unwrap();
        table.release(timeline).unwrap();
        assert_eq!(table.signal(timeline, 1), Ok(true));
        table.release(timeline).unwrap();
        assert_eq!(table.value(timeline), Err(TimelineError::InvalidId));
    }

    #[test]
    fn stale_id_cannot_address_reused_slot() {
        let mut table = TimelineTable::<1>::new();
        let stale = table.create(0).unwrap();
        table.release(stale).unwrap();
        let fresh = table.create(7).unwrap();
        assert_ne!(fresh, stale);
        assert_eq!(table.value(stale), Err(TimelineError::InvalidId));
        assert_eq!(table.value(fresh), Ok(7));
    }

    #[test]
    fn capacity_failure_publishes_no_partial_object() {
        let mut table = TimelineTable::<1>::new();
        let first = table.create(1).unwrap();
        assert_eq!(table.create(2), Err(TimelineError::LimitReached));
        table.release(first).unwrap();
        assert_eq!(table.create(3).and_then(|id| table.value(id)), Ok(3));
    }
}

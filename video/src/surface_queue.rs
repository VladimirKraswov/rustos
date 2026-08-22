//! Bounded очередь кадров клиентской surface.
//!
//! Очередь хранит только политику владения: конкретные `GraphicsBuffer` и
//! `SyncTimeline` остаются payload'ом вызывающего кода. Благодаря этому одна
//! и та же state machine используется CPU fallback, GPU renderer'ом и
//! compositor'ом, а ошибки повторного использования ещё показываемого buffer'а
//! обнаруживаются до обращения к драйверу.

use crate::protocol::{SURFACE_MAX_QUEUE_DEPTH, SURFACE_MIN_QUEUE_DEPTH};

/// Состояние одного buffer slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceSlotState {
    /// Buffer можно передать renderer'у.
    Free,
    /// Renderer владеет buffer'ом и ещё не опубликовал frame.
    Rendering,
    /// Frame опубликован и ждёт выбора compositor'ом.
    Ready,
    /// Frame передан display pipeline и освободится после release fence.
    Submitted,
}

/// Generation-checked ссылка на slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceSlotToken {
    index: u16,
    generation: u32,
}

impl SurfaceSlotToken {
    /// Индекс полезен для выбора соответствующего preallocated buffer'а.
    pub const fn index(self) -> usize {
        self.index as usize
    }
}

/// Результат mailbox-выбора.
///
/// `selected` становится `Submitted`. Все более старые ready frames сразу
/// освобождаются и возвращаются в `dropped`, чтобы владелец закрыл capabilities.
pub struct MailboxSelection<T: Copy, const N: usize> {
    /// Выбранный самый новый готовый frame.
    pub selected: (SurfaceSlotToken, u64, T),
    /// Payload отброшенных кадров; заполнены первые `dropped_count` элементов.
    pub dropped: [Option<T>; N],
    /// Число отброшенных кадров.
    pub dropped_count: usize,
}

/// Ошибка очереди, не зависящая от конкретного display driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceQueueError {
    /// Глубина не входит в ABI-границы или превышает compile-time capacity.
    InvalidDepth,
    /// Свободных buffers нет: producer обязан дождаться release/backpressure.
    WouldBlock,
    /// Token относится к старому поколению или несуществующему slot.
    StaleToken,
    /// Операция не разрешена в текущем состоянии slot.
    InvalidState,
    /// `frame_id` не монотонен внутри surface.
    NonMonotonicFrame,
    /// Ни одного опубликованного frame пока нет.
    NoReadyFrame,
}

#[derive(Clone, Copy)]
struct SurfaceSlot<T: Copy> {
    generation: u32,
    state: SurfaceSlotState,
    frame_id: u64,
    payload: Option<T>,
}

impl<T: Copy> SurfaceSlot<T> {
    const EMPTY: Self = Self {
        generation: 1,
        state: SurfaceSlotState::Free,
        frame_id: 0,
        payload: None,
    };
}

/// Allocation-free surface queue с FIFO acquire и mailbox presentation.
pub struct SurfaceQueue<T: Copy, const N: usize> {
    slots: [SurfaceSlot<T>; N],
    depth: usize,
    acquire_cursor: usize,
    last_frame_id: u64,
}

impl<T: Copy, const N: usize> SurfaceQueue<T, N> {
    /// Создаёт очередь заданной глубины.
    pub fn new(depth: usize) -> Result<Self, SurfaceQueueError> {
        if depth < usize::from(SURFACE_MIN_QUEUE_DEPTH)
            || depth > usize::from(SURFACE_MAX_QUEUE_DEPTH)
            || depth > N
        {
            return Err(SurfaceQueueError::InvalidDepth);
        }
        Ok(Self {
            slots: [SurfaceSlot::EMPTY; N],
            depth,
            acquire_cursor: 0,
            last_frame_id: 0,
        })
    }

    /// Число buffers, участвующих в очереди.
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// Берёт следующий свободный buffer. Поиск начинается после предыдущего
    /// acquire, чтобы producer равномерно использовал swapchain.
    pub fn acquire(&mut self) -> Result<SurfaceSlotToken, SurfaceQueueError> {
        for offset in 0..self.depth {
            let index = (self.acquire_cursor + offset) % self.depth;
            let slot = &mut self.slots[index];
            if slot.state == SurfaceSlotState::Free {
                slot.state = SurfaceSlotState::Rendering;
                slot.frame_id = 0;
                slot.payload = None;
                self.acquire_cursor = (index + 1) % self.depth;
                return Ok(SurfaceSlotToken {
                    index: index as u16,
                    generation: slot.generation,
                });
            }
        }
        Err(SurfaceQueueError::WouldBlock)
    }

    /// Атомарно публикует полностью сформированный frame.
    pub fn publish(
        &mut self,
        token: SurfaceSlotToken,
        frame_id: u64,
        payload: T,
    ) -> Result<(), SurfaceQueueError> {
        if frame_id == 0 || frame_id <= self.last_frame_id {
            return Err(SurfaceQueueError::NonMonotonicFrame);
        }
        let slot = self.slot_mut(token)?;
        if slot.state != SurfaceSlotState::Rendering {
            return Err(SurfaceQueueError::InvalidState);
        }
        slot.frame_id = frame_id;
        slot.payload = Some(payload);
        slot.state = SurfaceSlotState::Ready;
        self.last_frame_id = frame_id;
        Ok(())
    }

    /// Выбирает самый новый frame, чей acquire fence уже готов. Если ни один
    /// fence ещё не завершён, возвращает самый старый ready frame: downstream
    /// подождёт именно его fence и не создаст busy loop.
    pub fn select_mailbox(
        &mut self,
        mut is_ready: impl FnMut(T) -> bool,
    ) -> Result<MailboxSelection<T, N>, SurfaceQueueError> {
        let newest_ready = self
            .slots
            .iter()
            .take(self.depth)
            .enumerate()
            .filter(|(_, slot)| slot.state == SurfaceSlotState::Ready)
            .filter(|(_, slot)| slot.payload.is_some_and(&mut is_ready))
            .max_by_key(|(_, slot)| slot.frame_id)
            .map(|(index, _)| index);
        let selected_index = newest_ready.or_else(|| {
            self.slots
                .iter()
                .take(self.depth)
                .enumerate()
                .filter(|(_, slot)| slot.state == SurfaceSlotState::Ready)
                .min_by_key(|(_, slot)| slot.frame_id)
                .map(|(index, _)| index)
        });
        let Some(selected_index) = selected_index else {
            return Err(SurfaceQueueError::NoReadyFrame);
        };
        let selected_frame_id = self.slots[selected_index].frame_id;
        let mut dropped = [None; N];
        let mut dropped_count = 0usize;
        for (index, slot) in self.slots.iter_mut().take(self.depth).enumerate() {
            if index != selected_index
                && slot.state == SurfaceSlotState::Ready
                && slot.frame_id < selected_frame_id
            {
                dropped[dropped_count] = slot.payload.take();
                dropped_count += 1;
                release_slot(slot);
            }
        }
        let slot = &mut self.slots[selected_index];
        slot.state = SurfaceSlotState::Submitted;
        let payload = slot.payload.ok_or(SurfaceQueueError::InvalidState)?;
        Ok(MailboxSelection {
            selected: (
                SurfaceSlotToken {
                    index: selected_index as u16,
                    generation: slot.generation,
                },
                selected_frame_id,
                payload,
            ),
            dropped,
            dropped_count,
        })
    }

    /// Release fence display pipeline завершён; buffer снова доступен renderer'у.
    pub fn release(&mut self, token: SurfaceSlotToken) -> Result<(), SurfaceQueueError> {
        let slot = self.slot_mut(token)?;
        if slot.state != SurfaceSlotState::Submitted {
            return Err(SurfaceQueueError::InvalidState);
        }
        release_slot(slot);
        Ok(())
    }

    /// Отменяет frame, который ещё не был передан display pipeline.
    pub fn cancel(&mut self, token: SurfaceSlotToken) -> Result<Option<T>, SurfaceQueueError> {
        let slot = self.slot_mut(token)?;
        if !matches!(
            slot.state,
            SurfaceSlotState::Rendering | SurfaceSlotState::Ready
        ) {
            return Err(SurfaceQueueError::InvalidState);
        }
        let payload = slot.payload.take();
        release_slot(slot);
        Ok(payload)
    }

    /// Текущее состояние slot для диагностики и тестов.
    pub fn state(&self, token: SurfaceSlotToken) -> Result<SurfaceSlotState, SurfaceQueueError> {
        let slot = self.slot(token)?;
        Ok(slot.state)
    }

    fn slot(&self, token: SurfaceSlotToken) -> Result<&SurfaceSlot<T>, SurfaceQueueError> {
        let Some(slot) = self.slots.get(token.index()) else {
            return Err(SurfaceQueueError::StaleToken);
        };
        if token.index() >= self.depth || slot.generation != token.generation {
            return Err(SurfaceQueueError::StaleToken);
        }
        Ok(slot)
    }

    fn slot_mut(
        &mut self,
        token: SurfaceSlotToken,
    ) -> Result<&mut SurfaceSlot<T>, SurfaceQueueError> {
        let Some(slot) = self.slots.get_mut(token.index()) else {
            return Err(SurfaceQueueError::StaleToken);
        };
        if token.index() >= self.depth || slot.generation != token.generation {
            return Err(SurfaceQueueError::StaleToken);
        }
        Ok(slot)
    }
}

fn release_slot<T: Copy>(slot: &mut SurfaceSlot<T>) {
    slot.state = SurfaceSlotState::Free;
    slot.frame_id = 0;
    slot.payload = None;
    slot.generation = slot.generation.wrapping_add(1).max(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_never_reuses_submitted_buffer_before_release() {
        let mut queue = SurfaceQueue::<u32, 3>::new(3).unwrap();
        let first = queue.acquire().unwrap();
        queue.publish(first, 1, 10).unwrap();
        let selected = queue.select_mailbox(|_| true).unwrap();
        assert_eq!(selected.selected.2, 10);
        assert_eq!(queue.state(first), Ok(SurfaceSlotState::Submitted));

        let second = queue.acquire().unwrap();
        let third = queue.acquire().unwrap();
        assert_ne!(first.index(), second.index());
        assert_ne!(first.index(), third.index());
        assert_eq!(queue.acquire(), Err(SurfaceQueueError::WouldBlock));

        queue.release(first).unwrap();
        let reused = queue.acquire().unwrap();
        assert_eq!(reused.index(), first.index());
        assert_eq!(queue.state(first), Err(SurfaceQueueError::StaleToken));
    }

    #[test]
    fn mailbox_presents_newest_ready_and_returns_stale_payloads() {
        let mut queue = SurfaceQueue::<u32, 3>::new(3).unwrap();
        for (frame_id, payload) in [(1, 10), (2, 20), (3, 30)] {
            let token = queue.acquire().unwrap();
            queue.publish(token, frame_id, payload).unwrap();
        }
        let selection = queue.select_mailbox(|payload| payload != 30).unwrap();
        assert_eq!(selection.selected.1, 2);
        assert_eq!(selection.selected.2, 20);
        assert_eq!(selection.dropped_count, 1);
        assert_eq!(selection.dropped[0], Some(10));

        // Более новый, но ещё неготовый frame остаётся в очереди.
        queue.release(selection.selected.0).unwrap();
        let next = queue.select_mailbox(|_| true).unwrap();
        assert_eq!(next.selected.1, 3);
        assert_eq!(next.selected.2, 30);
    }

    #[test]
    fn failed_publish_does_not_lose_rendering_slot() {
        let mut queue = SurfaceQueue::<u32, 3>::new(3).unwrap();
        let first = queue.acquire().unwrap();
        assert_eq!(
            queue.publish(first, 0, 1),
            Err(SurfaceQueueError::NonMonotonicFrame)
        );
        assert_eq!(queue.state(first), Ok(SurfaceSlotState::Rendering));
        queue.publish(first, 1, 1).unwrap();

        let second = queue.acquire().unwrap();
        assert_eq!(
            queue.publish(second, 1, 2),
            Err(SurfaceQueueError::NonMonotonicFrame)
        );
        assert_eq!(queue.cancel(second), Ok(None));
    }

    #[test]
    fn depth_is_bounded_by_abi_and_storage() {
        assert!(matches!(
            SurfaceQueue::<u8, 3>::new(1),
            Err(SurfaceQueueError::InvalidDepth)
        ));
        assert!(matches!(
            SurfaceQueue::<u8, 3>::new(4),
            Err(SurfaceQueueError::InvalidDepth)
        ));
        assert_eq!(SurfaceQueue::<u8, 3>::new(3).unwrap().depth(), 3);
    }
}

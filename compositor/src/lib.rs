//! Renderer-neutral policy ring-3 `compositord`.
//!
//! Модуль намеренно не знает о VirGL, framebuffer, IPC и process-local
//! handles. Он хранит только bounded scene metadata, focus/capture и frame
//! clock. Поэтому GPU provider, software renderer и host tests используют
//! одну и ту же оконную семантику.

#![no_std]

use rustos_abi::surface::SurfaceId;
use rustos_video::Rect;

/// Один стабильный кадр двухслотового scene mailbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneCandidate {
    /// Монотонный ID producer'а.
    pub frame_id: u64,
    /// Номер immutable shared slot.
    pub slot: u16,
    /// Полный кадр создаёт retained cache; transform-only зависит от него.
    pub full: bool,
}

/// Выбирает newest пригодный кадр без FIFO backlog.
///
/// После рестарта renderer'а transform-only кадр нельзя применять к пустому
/// cache. В таком случае сначала выбирается самый новый полный кадр, даже если
/// в соседнем slot уже опубликована более новая transform delta.
pub fn select_newest_scene(
    candidates: &[SceneCandidate],
    last_presented: u64,
    scene_initialized: bool,
) -> Option<SceneCandidate> {
    candidates
        .iter()
        .copied()
        .filter(|candidate| {
            candidate.frame_id > last_presented && (scene_initialized || candidate.full)
        })
        .max_by_key(|candidate| candidate.frame_id)
}

/// Pacing одного output. Несколько invalidation между двумя presentation
/// boundaries схлопываются в один pending frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameClock {
    refresh_interval_ns: u64,
    last_present_time_ns: u64,
    last_sequence: u64,
    pending: bool,
}

impl FrameClock {
    /// Создаёт clock из display mode. Нулевой refresh заменяется безопасным
    /// интервалом 60 Гц и никогда не приводит к busy-loop.
    pub const fn new(refresh_interval_ns: u64) -> Self {
        Self {
            refresh_interval_ns: if refresh_interval_ns == 0 {
                16_666_667
            } else {
                refresh_interval_ns
            },
            last_present_time_ns: 0,
            last_sequence: 0,
            pending: false,
        }
    }

    /// Запрашивает один будущий кадр. Повторные вызовы не создают backlog.
    pub fn request_frame(&mut self) {
        self.pending = true;
    }

    /// Есть ли работа для ближайшей presentation boundary.
    pub const fn pending(&self) -> bool {
        self.pending
    }

    /// Желаемая граница следующего кадра.
    pub fn target_time_ns(&self, now_ns: u64) -> u64 {
        if self.last_present_time_ns == 0 {
            return now_ns;
        }
        self.last_present_time_ns
            .saturating_add(self.refresh_interval_ns)
            .max(now_ns)
    }

    /// Принимает display feedback и завершает ровно один pending frame.
    pub fn presented(&mut self, sequence: u64, actual_time_ns: u64, refresh_interval_ns: u64) {
        if sequence <= self.last_sequence || actual_time_ns < self.last_present_time_ns {
            return;
        }
        self.last_sequence = sequence;
        self.last_present_time_ns = actual_time_ns;
        if refresh_interval_ns != 0 {
            self.refresh_interval_ns = refresh_interval_ns;
        }
        self.pending = false;
    }
}

/// Маршрут input после hit-test. PID нужен только для capability endpoint
/// lookup в service adapter; указателей или kernel handles здесь нет.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputTarget {
    pub owner_pid: u64,
    pub surface: SurfaceId,
}

#[derive(Clone, Copy)]
struct FocusEntry {
    target: InputTarget,
    bounds: Rect,
    z: i32,
    creation_order: u64,
    visible: bool,
}

impl FocusEntry {
    const EMPTY: Self = Self {
        target: InputTarget {
            owner_pid: 0,
            surface: SurfaceId::INVALID,
        },
        bounds: Rect::EMPTY,
        z: 0,
        creation_order: 0,
        visible: false,
    };

    const fn live(self) -> bool {
        self.target.owner_pid != 0 && self.target.surface.is_valid()
    }
}

/// Bounded focus/z-order/capture state одного GUI seat.
pub struct FocusRouter<const N: usize> {
    entries: [FocusEntry; N],
    focused: Option<InputTarget>,
    captured: Option<InputTarget>,
    next_creation_order: u64,
}

impl<const N: usize> FocusRouter<N> {
    pub const fn new() -> Self {
        Self {
            entries: [FocusEntry::EMPTY; N],
            focused: None,
            captured: None,
            next_creation_order: 1,
        }
    }

    pub fn insert(&mut self, target: InputTarget, bounds: Rect, z: i32) -> Result<(), FocusError> {
        if !target.surface.is_valid() || target.owner_pid == 0 || bounds.is_empty() {
            return Err(FocusError::InvalidSurface);
        }
        if self.find(target).is_some() {
            return Err(FocusError::AlreadyExists);
        }
        let slot = self
            .entries
            .iter_mut()
            .find(|entry| !entry.live())
            .ok_or(FocusError::Capacity)?;
        *slot = FocusEntry {
            target,
            bounds,
            z,
            creation_order: self.next_creation_order,
            visible: true,
        };
        self.next_creation_order = self.next_creation_order.saturating_add(1);
        Ok(())
    }

    pub fn remove(&mut self, target: InputTarget) -> Result<(), FocusError> {
        let index = self.find(target).ok_or(FocusError::NotFound)?;
        self.entries[index] = FocusEntry::EMPTY;
        if self.focused == Some(target) {
            self.focused = self.top_at(None);
        }
        if self.captured == Some(target) {
            self.captured = None;
        }
        Ok(())
    }

    /// Pointer capture имеет приоритет над hit-test до button release.
    pub fn pointer_target(&self, x: i32, y: i32) -> Option<InputTarget> {
        self.captured.or_else(|| self.top_at(Some((x, y))))
    }

    pub fn focus_at(&mut self, x: i32, y: i32) -> Option<InputTarget> {
        let target = self.top_at(Some((x, y)));
        self.focused = target;
        target
    }

    pub fn capture(&mut self, target: InputTarget) -> Result<(), FocusError> {
        let index = self.find(target).ok_or(FocusError::NotFound)?;
        if !self.entries[index].visible {
            return Err(FocusError::NotVisible);
        }
        self.captured = Some(target);
        Ok(())
    }

    pub fn release_capture(&mut self, target: InputTarget) {
        if self.captured == Some(target) {
            self.captured = None;
        }
    }

    pub const fn keyboard_target(&self) -> Option<InputTarget> {
        self.focused
    }

    fn find(&self, target: InputTarget) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.live() && entry.target == target)
    }

    fn top_at(&self, point: Option<(i32, i32)>) -> Option<InputTarget> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.live()
                    && entry.visible
                    && point.is_none_or(|(x, y)| entry.bounds.contains(x, y))
            })
            .max_by_key(|entry| (entry.z, entry.creation_order))
            .map(|entry| entry.target)
    }
}

impl<const N: usize> Default for FocusRouter<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusError {
    InvalidSurface,
    AlreadyExists,
    Capacity,
    NotFound,
    NotVisible,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(pid: u64, surface: u64) -> InputTarget {
        InputTarget {
            owner_pid: pid,
            surface: SurfaceId(surface),
        }
    }

    #[test]
    fn mailbox_bootstraps_full_scene_then_uses_newest_delta() {
        let candidates = [
            SceneCandidate {
                frame_id: 10,
                slot: 0,
                full: true,
            },
            SceneCandidate {
                frame_id: 11,
                slot: 1,
                full: false,
            },
        ];
        assert_eq!(
            select_newest_scene(&candidates, 0, false),
            Some(candidates[0])
        );
        assert_eq!(
            select_newest_scene(&candidates, 10, true),
            Some(candidates[1])
        );
        assert_eq!(select_newest_scene(&candidates, 11, true), None);
    }

    #[test]
    fn frame_clock_coalesces_requests_and_uses_feedback() {
        let mut clock = FrameClock::new(16_666_667);
        clock.request_frame();
        clock.request_frame();
        assert!(clock.pending());
        assert_eq!(clock.target_time_ns(100), 100);
        clock.presented(1, 1_000, 20_000_000);
        assert!(!clock.pending());
        assert_eq!(clock.target_time_ns(2_000), 20_001_000);
        clock.presented(1, 500, 1);
        assert_eq!(clock.target_time_ns(2_000), 20_001_000);
    }

    #[test]
    fn focus_uses_z_order_and_pointer_capture() {
        let mut router = FocusRouter::<4>::new();
        let lower = target(10, 1);
        let upper = target(11, 2);
        router.insert(lower, Rect::new(0, 0, 100, 100), 1).unwrap();
        router
            .insert(upper, Rect::new(20, 20, 100, 100), 2)
            .unwrap();
        assert_eq!(router.focus_at(30, 30), Some(upper));
        router.capture(upper).unwrap();
        assert_eq!(router.pointer_target(1, 1), Some(upper));
        router.release_capture(upper);
        assert_eq!(router.pointer_target(1, 1), Some(lower));
        router.remove(upper).unwrap();
        assert_eq!(router.keyboard_target(), Some(lower));
    }
}

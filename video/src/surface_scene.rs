//! Переносимая модель сцены user-space compositor'а.
//!
//! Модуль не знает о VirGL/Vulkan и capabilities конкретного процесса. Он
//! решает только оконную задачу: paint order, clipping, простое occlusion,
//! transform-only move и bounded damage. GPU backend затем преобразует
//! [`VisibleSurface`] в texture layers без чтения pixels обратно на CPU.

use crate::{DamageRegion, Point, Rect};

/// Стабильный ID surface внутри одной compositor session.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceId(pub u64);

impl SurfaceId {
    /// Нулевой ID никогда не принадлежит клиенту.
    pub const INVALID: Self = Self(0);

    /// Проверяет, что ID назначен compositor'ом.
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Начальная политика слоя. Buffer публикуется отдельно через `commit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceLayerConfig {
    /// Surface session ID.
    pub surface: SurfaceId,
    /// Положение и physical размер на output.
    pub destination: Rect,
    /// Z-order; при равенстве сохраняется порядок создания.
    pub z: i32,
    /// Общая прозрачность 0..=255.
    pub opacity: u8,
    /// Каждый pixel surface непрозрачен. При opacity < 255 флаг игнорируется.
    pub opaque: bool,
}

impl SurfaceLayerConfig {
    /// Обычная непрозрачная оконная surface.
    pub const fn opaque(surface: SurfaceId, destination: Rect, z: i32) -> Self {
        Self {
            surface,
            destination,
            z,
            opacity: u8::MAX,
            opaque: true,
        }
    }
}

/// Один видимый слой в порядке снизу вверх.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisibleSurface<T: Copy> {
    /// Surface session ID.
    pub surface: SurfaceId,
    /// Последний монотонный frame клиента.
    pub frame_id: u64,
    /// Caller-owned resource/handle metadata.
    pub payload: T,
    /// Откуда начинать sampling, если окно clipped краем output.
    pub source_offset: Point,
    /// Clipped destination на output.
    pub destination: Rect,
    /// Общая прозрачность.
    pub opacity: u8,
    /// Слой можно учитывать при occlusion/direct-scanout policy.
    pub opaque: bool,
}

/// Ошибка scene state до обращения к GPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceSceneError {
    /// Нулевой ID или пустая geometry.
    InvalidSurface,
    /// ID уже зарегистрирован.
    AlreadyExists,
    /// Bounded таблица заполнена.
    Capacity,
    /// Surface не найдена.
    NotFound,
    /// Frame ID не монотонен для этой surface.
    NonMonotonicFrame,
    /// Caller передал слишком маленький output slice.
    OutputTooSmall,
}

#[derive(Clone, Copy)]
struct SceneSlot<T: Copy> {
    live: bool,
    config: SurfaceLayerConfig,
    creation_order: u64,
    frame_id: u64,
    payload: Option<T>,
}

impl<T: Copy> SceneSlot<T> {
    const EMPTY: Self = Self {
        live: false,
        config: SurfaceLayerConfig {
            surface: SurfaceId::INVALID,
            destination: Rect::EMPTY,
            z: 0,
            opacity: 0,
            opaque: false,
        },
        creation_order: 0,
        frame_id: 0,
        payload: None,
    };
}

/// Allocation-free compositor scene.
pub struct SurfaceScene<T: Copy, const N: usize, const D: usize> {
    output: Rect,
    slots: [SceneSlot<T>; N],
    next_creation_order: u64,
    damage: DamageRegion<D>,
}

impl<T: Copy, const N: usize, const D: usize> SurfaceScene<T, N, D> {
    /// Создаёт пустую сцену для physical output.
    pub fn new(output: Rect) -> Self {
        assert!(N > 0 && D > 0 && !output.is_empty());
        Self {
            output,
            slots: [SceneSlot::EMPTY; N],
            next_creation_order: 1,
            damage: DamageRegion::new(output),
        }
    }

    /// Регистрирует surface без pixels. Первая публикация делается `commit`.
    pub fn create(&mut self, config: SurfaceLayerConfig) -> Result<(), SurfaceSceneError> {
        if !config.surface.is_valid() || config.destination.is_empty() {
            return Err(SurfaceSceneError::InvalidSurface);
        }
        if self.find(config.surface).is_some() {
            return Err(SurfaceSceneError::AlreadyExists);
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| !slot.live) else {
            return Err(SurfaceSceneError::Capacity);
        };
        slot.live = true;
        slot.config = config;
        slot.creation_order = self.next_creation_order;
        slot.frame_id = 0;
        slot.payload = None;
        self.next_creation_order = self.next_creation_order.saturating_add(1);
        self.damage.add(config.destination);
        Ok(())
    }

    /// Публикует новый immutable frame и его local damage.
    ///
    /// Пустой `local_damage` означает full surface damage. Возвращаемый старый
    /// payload caller освобождает только после соответствующего release fence.
    pub fn commit(
        &mut self,
        surface: SurfaceId,
        frame_id: u64,
        payload: T,
        local_damage: Rect,
    ) -> Result<Option<T>, SurfaceSceneError> {
        let index = self.find(surface).ok_or(SurfaceSceneError::NotFound)?;
        let slot = &self.slots[index];
        if frame_id == 0 || frame_id <= slot.frame_id {
            return Err(SurfaceSceneError::NonMonotonicFrame);
        }
        let bounds = slot.config.destination;
        let damage = if local_damage.is_empty() {
            bounds
        } else {
            local_damage
                .intersection(Rect::new(0, 0, bounds.width, bounds.height))
                .translated(Point::new(bounds.x, bounds.y))
        };
        self.damage.add(damage);
        let slot = &mut self.slots[index];
        let previous = slot.payload.replace(payload);
        slot.frame_id = frame_id;
        Ok(previous)
    }

    /// Меняет только transform слоя. Содержимое buffer не перерисовывается;
    /// damage включает старое и новое положение.
    pub fn move_surface(
        &mut self,
        surface: SurfaceId,
        destination: Rect,
    ) -> Result<(), SurfaceSceneError> {
        if destination.is_empty() {
            return Err(SurfaceSceneError::InvalidSurface);
        }
        let index = self.find(surface).ok_or(SurfaceSceneError::NotFound)?;
        let previous = self.slots[index].config.destination;
        self.slots[index].config.destination = destination;
        self.damage.add(previous);
        self.damage.add(destination);
        Ok(())
    }

    /// Обновляет opacity без изменения pixels.
    pub fn set_opacity(
        &mut self,
        surface: SurfaceId,
        opacity: u8,
    ) -> Result<(), SurfaceSceneError> {
        let index = self.find(surface).ok_or(SurfaceSceneError::NotFound)?;
        self.slots[index].config.opacity = opacity;
        self.damage.add(self.slots[index].config.destination);
        Ok(())
    }

    /// Обновляет z-order и повреждает область слоя.
    pub fn set_z(&mut self, surface: SurfaceId, z: i32) -> Result<(), SurfaceSceneError> {
        let index = self.find(surface).ok_or(SurfaceSceneError::NotFound)?;
        self.slots[index].config.z = z;
        self.damage.add(self.slots[index].config.destination);
        Ok(())
    }

    /// Удаляет surface после crash/close и возвращает последний payload.
    pub fn remove(&mut self, surface: SurfaceId) -> Result<Option<T>, SurfaceSceneError> {
        let index = self.find(surface).ok_or(SurfaceSceneError::NotFound)?;
        self.damage.add(self.slots[index].config.destination);
        let payload = self.slots[index].payload.take();
        self.slots[index] = SceneSlot::EMPTY;
        Ok(payload)
    }

    /// Строит видимый список снизу вверх. Полностью закрытые одним верхним
    /// opaque rectangle слои не попадают в GPU batch.
    pub fn visible_layers(
        &self,
        output: &mut [VisibleSurface<T>],
    ) -> Result<usize, SurfaceSceneError> {
        let mut ordered = [usize::MAX; N];
        let mut ordered_len = 0usize;
        for (index, slot) in self.slots.iter().enumerate() {
            if !slot.live || slot.payload.is_none() || slot.config.opacity == 0 {
                continue;
            }
            let key = (slot.config.z, slot.creation_order);
            let insert = (0..ordered_len)
                .find(|position| {
                    let other = &self.slots[ordered[*position]];
                    key < (other.config.z, other.creation_order)
                })
                .unwrap_or(ordered_len);
            ordered.copy_within(insert..ordered_len, insert + 1);
            ordered[insert] = index;
            ordered_len += 1;
        }

        let mut count = 0usize;
        for order_index in 0..ordered_len {
            let slot = &self.slots[ordered[order_index]];
            let destination = slot.config.destination.intersection(self.output);
            if destination.is_empty()
                || ordered[order_index + 1..ordered_len]
                    .iter()
                    .map(|index| &self.slots[*index])
                    .any(|upper| {
                        upper.config.opaque
                            && upper.config.opacity == u8::MAX
                            && contains_rect(
                                upper.config.destination.intersection(self.output),
                                destination,
                            )
                    })
            {
                continue;
            }
            let Some(target) = output.get_mut(count) else {
                return Err(SurfaceSceneError::OutputTooSmall);
            };
            *target = VisibleSurface {
                surface: slot.config.surface,
                frame_id: slot.frame_id,
                payload: slot.payload.expect("visible slot has committed payload"),
                source_offset: Point::new(
                    destination.x.saturating_sub(slot.config.destination.x),
                    destination.y.saturating_sub(slot.config.destination.y),
                ),
                destination,
                opacity: slot.config.opacity,
                opaque: slot.config.opaque && slot.config.opacity == u8::MAX,
            };
            count += 1;
        }
        Ok(count)
    }

    /// Damage следующего atomic compositor commit.
    pub const fn damage(&self) -> &DamageRegion<D> {
        &self.damage
    }

    /// Вызывается только после успешной публикации полного compositor frame.
    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }

    fn find(&self, surface: SurfaceId) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.live && slot.config.surface == surface)
    }
}

fn contains_rect(outer: Rect, inner: Rect) -> bool {
    !outer.is_empty()
        && !inner.is_empty()
        && outer.x <= inner.x
        && outer.y <= inner.y
        && outer.right() >= inner.right()
        && outer.bottom() >= inner.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: Rect = Rect::new(0, 0, 1280, 800);
    const EMPTY_VISIBLE: VisibleSurface<u32> = VisibleSurface {
        surface: SurfaceId::INVALID,
        frame_id: 0,
        payload: 0,
        source_offset: Point::new(0, 0),
        destination: Rect::EMPTY,
        opacity: 0,
        opaque: false,
    };

    #[test]
    fn move_changes_only_transform_and_damages_old_and_new_bounds() {
        let mut scene = SurfaceScene::<u32, 4, 8>::new(OUTPUT);
        scene
            .create(SurfaceLayerConfig::opaque(
                SurfaceId(1),
                Rect::new(40, 50, 320, 200),
                1,
            ))
            .unwrap();
        scene.commit(SurfaceId(1), 1, 77, Rect::EMPTY).unwrap();
        scene.clear_damage();
        scene
            .move_surface(SurfaceId(1), Rect::new(80, 90, 320, 200))
            .unwrap();

        let mut visible = [EMPTY_VISIBLE; 4];
        assert_eq!(scene.visible_layers(&mut visible), Ok(1));
        assert_eq!(visible[0].payload, 77, "buffer не перерисован");
        assert_eq!(visible[0].destination, Rect::new(80, 90, 320, 200));
        assert_eq!(scene.damage().as_slice(), &[Rect::new(40, 50, 360, 240)]);
    }

    #[test]
    fn opaque_upper_surface_removes_fully_hidden_lower_layer() {
        let mut scene = SurfaceScene::<u32, 4, 8>::new(OUTPUT);
        for (id, z, payload) in [(1, 0, 10), (2, 1, 20)] {
            scene
                .create(SurfaceLayerConfig::opaque(SurfaceId(id), OUTPUT, z))
                .unwrap();
            scene
                .commit(SurfaceId(id), 1, payload, Rect::EMPTY)
                .unwrap();
        }
        let mut visible = [EMPTY_VISIBLE; 4];
        assert_eq!(scene.visible_layers(&mut visible), Ok(1));
        assert_eq!(visible[0].surface, SurfaceId(2));
    }

    #[test]
    fn translucent_layers_preserve_bottom_to_top_order() {
        let mut scene = SurfaceScene::<u32, 4, 8>::new(OUTPUT);
        let configs = [
            SurfaceLayerConfig::opaque(SurfaceId(1), Rect::new(0, 0, 640, 400), 8),
            SurfaceLayerConfig {
                surface: SurfaceId(2),
                destination: Rect::new(10, 10, 640, 400),
                z: 9,
                opacity: 180,
                opaque: true,
            },
        ];
        for (index, config) in configs.into_iter().enumerate() {
            scene.create(config).unwrap();
            scene
                .commit(config.surface, 1, index as u32 + 1, Rect::EMPTY)
                .unwrap();
        }
        let mut visible = [EMPTY_VISIBLE; 4];
        assert_eq!(scene.visible_layers(&mut visible), Ok(2));
        assert_eq!(visible[0].surface, SurfaceId(1));
        assert_eq!(visible[1].surface, SurfaceId(2));
        assert!(!visible[1].opaque);
    }

    #[test]
    fn clipped_surface_keeps_sampling_offset_and_crash_releases_payload() {
        let mut scene = SurfaceScene::<u32, 2, 4>::new(OUTPUT);
        scene
            .create(SurfaceLayerConfig::opaque(
                SurfaceId(9),
                Rect::new(-30, -20, 100, 80),
                0,
            ))
            .unwrap();
        scene.commit(SurfaceId(9), 1, 55, Rect::EMPTY).unwrap();
        let mut visible = [EMPTY_VISIBLE; 2];
        assert_eq!(scene.visible_layers(&mut visible), Ok(1));
        assert_eq!(visible[0].source_offset, Point::new(30, 20));
        assert_eq!(visible[0].destination, Rect::new(0, 0, 70, 60));
        assert_eq!(scene.remove(SurfaceId(9)), Ok(Some(55)));
        assert_eq!(scene.visible_layers(&mut visible), Ok(0));
    }
}

//! Общая модель прокрутки, независимая от конкретного компонента и renderer.
//!
//! `ScrollModel` хранит только положение и extents. `ScrollController`
//! нормализует wheel/keyboard/programmatic input. `ScrollbarGeometry` является
//! представлением той же модели и не владеет состоянием контента.

use rustos_video::Rect;

/// Стандартная толщина интерактивной полосы. 14 logical px остаются удобными
/// для мыши при 1× и масштабируются до крупной физической цели на HiDPI.
pub const DEFAULT_SCROLLBAR_THICKNESS: u32 = 14;
/// Место, резервируемое inset-полосой вместе с двухпиксельным отступом.
pub const DEFAULT_SCROLLBAR_INSET: u32 = DEFAULT_SCROLLBAR_THICKNESS + 2;

/// Ось прокрутки.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScrollAxis {
    /// По горизонтали.
    Horizontal = 0,
    /// По вертикали.
    #[default]
    Vertical = 1,
}

/// Единица входного scroll delta.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScrollUnit {
    /// Точное число logical pixels от trackpad/high-resolution wheel.
    #[default]
    Pixel = 0,
    /// Строки; controller применяет системный `line_extent`.
    Line = 1,
    /// Страницы; controller применяет viewport extent.
    Page = 2,
}

/// Двухмерное нормализованное событие прокрутки.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrollDelta {
    /// Горизонтальная составляющая.
    pub x: i32,
    /// Вертикальная составляющая.
    pub y: i32,
    /// Единица обеих составляющих.
    pub unit: ScrollUnit,
}

impl ScrollDelta {
    /// Pixel delta.
    pub const fn pixels(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            unit: ScrollUnit::Pixel,
        }
    }

    /// Line delta.
    pub const fn lines(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            unit: ScrollUnit::Line,
        }
    }

    /// Page delta.
    pub const fn pages(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            unit: ScrollUnit::Page,
        }
    }
}

/// Политика видимости полосы прокрутки.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScrollBarPolicy {
    /// Полоса никогда не рисуется, но программная/жестовая прокрутка работает.
    Hidden = 0,
    /// Полоса появляется только когда content больше viewport.
    #[default]
    Auto = 1,
    /// Полоса занимает своё место даже при нулевом диапазоне.
    Always = 2,
}

/// Способ размещения scrollbar.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScrollBarLayout {
    /// Поверх содержимого; viewport не уменьшается.
    #[default]
    Overlay = 0,
    /// Отдельная полоса внутри компонента.
    Inset = 1,
}

/// Поведение delta на границе вложенной scroll area.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverscrollPolicy {
    /// Неиспользованный delta передаётся ближайшему scrollable-предку.
    #[default]
    Chain = 0,
    /// Delta остаётся внутри области и не передаётся родителю.
    Contain = 1,
}

/// Мгновенная или frame-based прокрутка.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScrollBehavior {
    /// Offset меняется в текущей input transaction.
    #[default]
    Instant = 0,
    /// Меняется target; current приближается к нему через `advance_frame`.
    Smooth = 1,
}

/// Настройки двухосного ScrollView.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollConfig {
    /// Вертикальная полоса.
    pub vertical: ScrollBarPolicy,
    /// Горизонтальная полоса.
    pub horizontal: ScrollBarPolicy,
    /// Overlay или занимающая место полоса.
    pub bar_layout: ScrollBarLayout,
    /// Передача остатка вложенному родителю.
    pub overscroll: OverscrollPolicy,
    /// Wheel/keyboard transition.
    pub behavior: ScrollBehavior,
    /// Системная высота строки для line delta.
    pub line_extent: u16,
}

impl ScrollConfig {
    /// Вертикальная scroll area со стандартным overlay scrollbar.
    pub const VERTICAL: Self = Self {
        vertical: ScrollBarPolicy::Auto,
        horizontal: ScrollBarPolicy::Hidden,
        bar_layout: ScrollBarLayout::Overlay,
        overscroll: OverscrollPolicy::Chain,
        behavior: ScrollBehavior::Instant,
        line_extent: 36,
    };

    /// Обе оси со стандартными overlay scrollbars.
    pub const BOTH: Self = Self {
        vertical: ScrollBarPolicy::Auto,
        horizontal: ScrollBarPolicy::Auto,
        ..Self::VERTICAL
    };

    /// Отключённая прокрутка для обычного компонента.
    pub const NONE: Self = Self {
        vertical: ScrollBarPolicy::Hidden,
        horizontal: ScrollBarPolicy::Hidden,
        bar_layout: ScrollBarLayout::Overlay,
        overscroll: OverscrollPolicy::Chain,
        behavior: ScrollBehavior::Instant,
        line_extent: 36,
    };
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self::VERTICAL
    }
}

/// Переиспользуемое состояние одной оси прокрутки.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrollModel {
    offset: u64,
    target: u64,
    minimum: u64,
    viewport_size: u32,
    content_size: u64,
}

impl ScrollModel {
    /// Пустая модель.
    pub const fn new() -> Self {
        Self {
            offset: 0,
            target: 0,
            minimum: 0,
            viewport_size: 0,
            content_size: 0,
        }
    }

    /// Текущий offset, применяемый layout/raster.
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Целевой offset smooth controller.
    pub const fn target(self) -> u64 {
        self.target
    }

    /// Минимальный offset.
    pub const fn minimum(self) -> u64 {
        self.minimum
    }

    /// Максимальный offset.
    pub const fn maximum(self) -> u64 {
        self.minimum
            .saturating_add(self.content_size.saturating_sub(self.viewport_size as u64))
    }

    /// Размер viewport.
    pub const fn viewport_size(self) -> u32 {
        self.viewport_size
    }

    /// Размер всего содержимого.
    pub const fn content_size(self) -> u64 {
        self.content_size
    }

    /// Размер page step.
    pub const fn page_size(self) -> u32 {
        self.viewport_size
    }

    /// Есть ли реальный диапазон прокрутки.
    pub const fn can_scroll(self) -> bool {
        self.maximum() > self.minimum
    }

    /// Атомарно обновляет extents и ограничивает current/target.
    pub fn set_extents(&mut self, viewport_size: u32, content_size: u64) -> bool {
        let old = *self;
        self.viewport_size = viewport_size;
        self.content_size = content_size;
        let maximum = self.maximum();
        self.offset = self.offset.clamp(self.minimum, maximum);
        self.target = self.target.clamp(self.minimum, maximum);
        *self != old
    }

    /// Устанавливает ненулевой minimum для специализированных координатных
    /// пространств. Обычный ScrollView использует ноль.
    pub fn set_minimum(&mut self, minimum: u64) {
        self.minimum = minimum;
        let maximum = self.maximum();
        self.offset = self.offset.clamp(minimum, maximum);
        self.target = self.target.clamp(minimum, maximum);
    }

    /// Мгновенно переходит к абсолютной позиции.
    pub fn scroll_to(&mut self, offset: u64) -> bool {
        let next = offset.clamp(self.minimum, self.maximum());
        let changed = self.offset != next || self.target != next;
        self.offset = next;
        self.target = next;
        changed
    }

    /// Применяет signed delta и возвращает неиспользованный остаток.
    pub fn scroll_by(&mut self, delta: i64) -> i64 {
        let old = self.offset;
        let requested = if delta < 0 {
            old.saturating_sub(delta.unsigned_abs())
        } else {
            old.saturating_add(delta as u64)
        };
        let next = requested.clamp(self.minimum, self.maximum());
        self.offset = next;
        self.target = next;
        delta.saturating_sub(signed_difference(next, old))
    }

    /// Меняет только smooth target и возвращает delta, который не помещается
    /// в допустимый диапазон.
    pub fn target_by(&mut self, delta: i64) -> i64 {
        let old = self.target;
        let requested = if delta < 0 {
            old.saturating_sub(delta.unsigned_abs())
        } else {
            old.saturating_add(delta as u64)
        };
        let next = requested.clamp(self.minimum, self.maximum());
        self.target = next;
        delta.saturating_sub(signed_difference(next, old))
    }

    /// Делает диапазон `[start, end)` видимым минимальным изменением offset.
    pub fn ensure_visible(&mut self, start: u64, end: u64) -> bool {
        let viewport = u64::from(self.viewport_size);
        let next = if start < self.offset {
            start
        } else if end > self.offset.saturating_add(viewport) {
            end.saturating_sub(viewport)
        } else {
            return false;
        };
        self.scroll_to(next)
    }

    /// Один frame smooth transition. `response_milli=1000` завершает переход,
    /// 250 перемещает примерно четверть оставшегося расстояния.
    pub fn advance_frame(&mut self, response_milli: u16) -> bool {
        if self.offset == self.target {
            return false;
        }
        let response = u64::from(response_milli.clamp(1, 1_000));
        let distance = self.offset.abs_diff(self.target);
        let step = distance.saturating_mul(response).div_ceil(1_000).max(1);
        self.offset = if self.offset < self.target {
            self.offset.saturating_add(step).min(self.target)
        } else {
            self.offset.saturating_sub(step).max(self.target)
        };
        true
    }
}

/// Двухосное состояние одного scrollable компонента.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollState {
    /// Горизонтальная ось.
    pub horizontal: ScrollModel,
    /// Вертикальная ось.
    pub vertical: ScrollModel,
    /// Общая политика.
    pub config: ScrollConfig,
}

impl ScrollState {
    /// Scrollable state с указанной конфигурацией.
    pub const fn new(config: ScrollConfig) -> Self {
        Self {
            horizontal: ScrollModel::new(),
            vertical: ScrollModel::new(),
            config,
        }
    }

    /// Отключённое состояние обычного компонента.
    pub const fn disabled() -> Self {
        Self::new(ScrollConfig::NONE)
    }

    /// Модель выбранной оси.
    pub const fn model(self, axis: ScrollAxis) -> ScrollModel {
        match axis {
            ScrollAxis::Horizontal => self.horizontal,
            ScrollAxis::Vertical => self.vertical,
        }
    }

    /// Mutable-модель выбранной оси.
    pub fn model_mut(&mut self, axis: ScrollAxis) -> &mut ScrollModel {
        match axis {
            ScrollAxis::Horizontal => &mut self.horizontal,
            ScrollAxis::Vertical => &mut self.vertical,
        }
    }
}

impl Default for ScrollState {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Controller преобразует разные единицы ввода в изменение одной модели.
pub struct ScrollController;

impl ScrollController {
    /// Применяет delta к выбранной оси. Возвращаемое значение выражено в
    /// logical pixels и предназначено для nested-scroll chaining.
    pub fn apply(
        model: &mut ScrollModel,
        delta: i32,
        unit: ScrollUnit,
        line_extent: u16,
        behavior: ScrollBehavior,
    ) -> i64 {
        let pixels = match unit {
            ScrollUnit::Pixel => i64::from(delta),
            ScrollUnit::Line => i64::from(delta).saturating_mul(i64::from(line_extent.max(1))),
            ScrollUnit::Page => {
                i64::from(delta).saturating_mul(i64::from(model.page_size().max(1)))
            }
        };
        match behavior {
            ScrollBehavior::Instant => model.scroll_by(pixels),
            ScrollBehavior::Smooth => model.target_by(pixels),
        }
    }
}

/// Геометрия scrollbar track/thumb, полностью выведенная из `ScrollModel`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrollbarGeometry {
    /// Ось.
    pub axis: ScrollAxis,
    /// Полная интерактивная дорожка.
    pub track: Rect,
    /// Перетаскиваемая часть.
    pub thumb: Rect,
    /// Нужно ли рисовать/обрабатывать scrollbar.
    pub visible: bool,
}

impl ScrollbarGeometry {
    /// Строит overlay-геометрию. `minimum_thumb` ограничивает маленькие
    /// документы без изменения соответствия offset ↔ position.
    pub fn overlay(
        viewport: Rect,
        model: ScrollModel,
        axis: ScrollAxis,
        thickness: u32,
        minimum_thumb: u32,
    ) -> Self {
        Self::with_visibility(
            viewport,
            model,
            axis,
            thickness,
            minimum_thumb,
            model.can_scroll(),
        )
    }

    /// Та же геометрия с явной видимостью для policy `Always`. При нулевом
    /// диапазоне thumb занимает всю track и остаётся неопасным для drag.
    pub fn with_visibility(
        viewport: Rect,
        model: ScrollModel,
        axis: ScrollAxis,
        thickness: u32,
        minimum_thumb: u32,
        visible: bool,
    ) -> Self {
        if viewport.is_empty() || !visible {
            return Self {
                axis,
                ..Self::default()
            };
        }
        let thickness = thickness.max(4);
        let track = match axis {
            ScrollAxis::Vertical => Rect::new(
                viewport.right().saturating_sub(thickness as i32 + 2),
                viewport.y.saturating_add(2),
                thickness,
                viewport.height.saturating_sub(4),
            ),
            ScrollAxis::Horizontal => Rect::new(
                viewport.x.saturating_add(2),
                viewport.bottom().saturating_sub(thickness as i32 + 2),
                viewport.width.saturating_sub(4),
                thickness,
            ),
        };
        let track_extent = match axis {
            ScrollAxis::Horizontal => track.width,
            ScrollAxis::Vertical => track.height,
        };
        let content = model.content_size().max(1);
        let thumb_extent = if model.can_scroll() {
            (u64::from(track_extent) * u64::from(model.viewport_size()) / content).clamp(
                u64::from(minimum_thumb.min(track_extent)),
                u64::from(track_extent),
            ) as u32
        } else {
            track_extent
        };
        let travel = track_extent.saturating_sub(thumb_extent);
        let range = model.maximum().saturating_sub(model.minimum()).max(1);
        let thumb_offset =
            (u64::from(travel) * model.offset().saturating_sub(model.minimum()) / range) as i32;
        let thumb = match axis {
            ScrollAxis::Vertical => Rect::new(
                track.x,
                track.y.saturating_add(thumb_offset),
                track.width,
                thumb_extent,
            ),
            ScrollAxis::Horizontal => Rect::new(
                track.x.saturating_add(thumb_offset),
                track.y,
                thumb_extent,
                track.height,
            ),
        };
        Self {
            axis,
            track,
            thumb,
            visible: true,
        }
    }

    /// Преобразует позицию начала thumb внутри track в scroll offset.
    pub fn offset_for_thumb(self, position: i32, model: ScrollModel) -> u64 {
        if !self.visible {
            return model.offset();
        }
        let (track_start, track_extent, thumb_extent) = match self.axis {
            ScrollAxis::Horizontal => (self.track.x, self.track.width, self.thumb.width),
            ScrollAxis::Vertical => (self.track.y, self.track.height, self.thumb.height),
        };
        let travel = track_extent.saturating_sub(thumb_extent);
        if travel == 0 {
            return model.minimum();
        }
        let relative = position.saturating_sub(track_start).clamp(0, travel as i32) as u64;
        model.minimum().saturating_add(
            model
                .maximum()
                .saturating_sub(model.minimum())
                .saturating_mul(relative)
                / u64::from(travel),
        )
    }
}

fn signed_difference(next: u64, previous: u64) -> i64 {
    if next >= previous {
        next.saturating_sub(previous).min(i64::MAX as u64) as i64
    } else {
        -(previous.saturating_sub(next).min(i64::MAX as u64) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_return_remainder_for_nested_scroll() {
        let mut model = ScrollModel::new();
        model.set_extents(100, 300);
        assert_eq!(model.scroll_by(250), 50);
        assert_eq!(model.offset(), 200);
        assert_eq!(model.scroll_by(-250), -50);
        assert_eq!(model.offset(), 0);
    }

    #[test]
    fn ensure_visible_moves_only_when_required() {
        let mut model = ScrollModel::new();
        model.set_extents(100, 1_000);
        assert!(model.ensure_visible(240, 260));
        assert_eq!(model.offset(), 160);
        assert!(!model.ensure_visible(170, 190));
        assert!(model.ensure_visible(20, 40));
        assert_eq!(model.offset(), 20);
    }

    #[test]
    fn smooth_target_advances_on_frames() {
        let mut model = ScrollModel::new();
        model.set_extents(100, 1_000);
        assert_eq!(
            ScrollController::apply(&mut model, 3, ScrollUnit::Line, 40, ScrollBehavior::Smooth),
            0
        );
        assert_eq!(model.offset(), 0);
        assert_eq!(model.target(), 120);
        assert!(model.advance_frame(500));
        assert_eq!(model.offset(), 60);
        assert!(model.advance_frame(1_000));
        assert_eq!(model.offset(), 120);
    }

    #[test]
    fn thumb_tracks_viewport_and_drag_maps_back_to_offset() {
        let mut model = ScrollModel::new();
        model.set_extents(200, 1_000);
        model.scroll_to(400);
        let geometry = ScrollbarGeometry::overlay(
            Rect::new(10, 20, 300, 200),
            model,
            ScrollAxis::Vertical,
            10,
            24,
        );
        assert!(geometry.visible);
        assert_eq!(geometry.thumb.height, 39);
        assert!(geometry.thumb.y > geometry.track.y);
        assert!(
            geometry
                .offset_for_thumb(geometry.thumb.y, model)
                .abs_diff(400)
                <= 6
        );
    }
}

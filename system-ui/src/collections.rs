//! Виртуализация больших коллекций без создания узла на каждый item.

/// Полуоткрытый диапазон индексов `[start, end)`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VisibleRange {
    /// Первый materialized item.
    pub start: u32,
    /// Индекс после последнего materialized item.
    pub end: u32,
}

impl VisibleRange {
    /// Число materialized items.
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }
    /// Пустой диапазон.
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Состояние fixed-extent VirtualList. Оно хранит O(1) metadata независимо от
/// числа элементов и сообщает приложению только небольшой видимый диапазон.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualList {
    item_count: u32,
    item_extent: u32,
    viewport_extent: u32,
    scroll_offset: u64,
    overscan: u16,
}

impl VirtualList {
    /// Создаёт список; `item_extent` минимум один логический пиксель.
    pub const fn new(item_count: u32, item_extent: u32) -> Self {
        Self {
            item_count,
            item_extent: if item_extent == 0 { 1 } else { item_extent },
            viewport_extent: 0,
            scroll_offset: 0,
            overscan: 2,
        }
    }

    /// Настраивает число соседних items для предварительной подготовки.
    pub fn set_overscan(&mut self, items: u16) {
        self.overscan = items.min(64);
    }

    /// Обновляет высоту/ширину viewport.
    pub fn set_viewport(&mut self, extent: u32) {
        self.viewport_extent = extent;
        self.scroll_offset = self.scroll_offset.min(self.maximum_offset());
    }

    /// Прокручивает к абсолютному offset с saturating bounds.
    pub fn scroll_to(&mut self, offset: u64) {
        self.scroll_offset = offset.min(self.maximum_offset());
    }

    /// Прокручивает относительно текущей позиции.
    pub fn scroll_by(&mut self, delta: i64) {
        self.scroll_offset = if delta < 0 {
            self.scroll_offset.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll_offset
                .saturating_add(delta as u64)
                .min(self.maximum_offset())
        };
    }

    /// Текущий materialized диапазон с overscan.
    pub fn visible_range(self) -> VisibleRange {
        if self.item_count == 0 || self.viewport_extent == 0 {
            return VisibleRange::default();
        }
        let first = (self.scroll_offset / u64::from(self.item_extent))
            .min(u64::from(self.item_count)) as u32;
        let visible = self
            .viewport_extent
            .div_ceil(self.item_extent)
            .saturating_add(1);
        VisibleRange {
            start: first.saturating_sub(u32::from(self.overscan)),
            end: first
                .saturating_add(visible)
                .saturating_add(u32::from(self.overscan))
                .min(self.item_count),
        }
    }

    /// Полная логическая длина с 64-bit arithmetic.
    pub const fn content_extent(self) -> u64 {
        self.item_count as u64 * self.item_extent as u64
    }

    /// Максимальный scroll offset.
    pub const fn maximum_offset(self) -> u64 {
        self.content_extent()
            .saturating_sub(self.viewport_extent as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifty_thousand_items_materialize_only_viewport() {
        let mut list = VirtualList::new(50_000, 24);
        list.set_viewport(480);
        list.scroll_to(500_000);
        let range = list.visible_range();
        assert!(range.start > 20_000);
        assert!(range.len() <= 25);
        assert_eq!(list.content_extent(), 1_200_000);
    }
}

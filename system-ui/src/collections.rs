//! Виртуализация и логическое состояние коллекций без создания узла на item.

use crate::{Key, ScrollModel, SelectionMode, SelectionModel, SelectionRange};

/// Число несвязанных диапазонов выбора, хранимых одним runtime ListView.
/// Сплошной `Ctrl+A` по-прежнему занимает ровно один диапазон.
pub const LIST_SELECTION_RANGES: usize = 8;

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

/// Состояние ListView живёт независимо от materialized delegates. Поэтому
/// recycling видимых строк не теряет selection/current item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListViewState {
    configured: bool,
    item_count: u32,
    item_extent: u32,
    overscan: u16,
    selection: SelectionModel<LIST_SELECTION_RANGES>,
}

impl ListViewState {
    /// Неактивное состояние для узла, который не является ListView.
    pub const fn disabled() -> Self {
        Self {
            configured: false,
            item_count: 0,
            item_extent: 1,
            overscan: 2,
            selection: SelectionModel::new(SelectionMode::None),
        }
    }

    /// Настраивает источник списка без materialization его элементов.
    pub fn configure(&mut self, item_count: u32, item_extent: u32, mode: SelectionMode) {
        self.configured = true;
        self.item_count = item_count;
        self.item_extent = item_extent.max(1);
        self.selection.set_mode(mode);
        self.clamp_selection();
    }

    /// Настраивает небольшой overscan для recycling delegates.
    pub fn set_overscan(&mut self, overscan: u16) {
        self.overscan = overscan.min(64);
    }

    /// Признак установленного collection source.
    pub const fn is_configured(self) -> bool {
        self.configured
    }

    /// Число logical items.
    pub const fn item_count(self) -> u32 {
        self.item_count
    }

    /// Высота одной строки fixed-extent списка.
    pub const fn item_extent(self) -> u32 {
        self.item_extent
    }

    /// Полная высота использует 64-bit arithmetic.
    pub const fn content_extent(self) -> u64 {
        self.item_count as u64 * self.item_extent as u64
    }

    /// Read-only selection для application binding/accessibility.
    pub const fn selection(&self) -> &SelectionModel<LIST_SELECTION_RANGES> {
        &self.selection
    }

    /// Выбранные logical ranges.
    pub fn selected_ranges(&self) -> &[SelectionRange] {
        self.selection.ranges()
    }

    /// Диапазон delegates для текущего viewport.
    pub fn visible_range(self, scroll: ScrollModel) -> VisibleRange {
        if !self.configured || self.item_count == 0 || scroll.viewport_size() == 0 {
            return VisibleRange::default();
        }
        let first =
            (scroll.offset() / u64::from(self.item_extent)).min(u64::from(self.item_count)) as u32;
        let visible = scroll
            .viewport_size()
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

    /// Pointer selection по координате внутри viewport.
    pub fn select_at(
        &mut self,
        local_y: u32,
        scroll: ScrollModel,
        shift: bool,
        control: bool,
    ) -> bool {
        if !self.configured || self.item_count == 0 {
            return false;
        }
        let index = scroll
            .offset()
            .saturating_add(u64::from(local_y))
            .checked_div(u64::from(self.item_extent))
            .unwrap_or(0)
            .min(u64::from(self.item_count - 1)) as u32;
        let before = self.selection;
        if shift {
            self.selection.extend_to(index);
        } else if control {
            let _ = self.selection.toggle(index);
        } else {
            self.selection.select(index);
        }
        self.selection != before
    }

    /// Стандартная keyboard navigation. Возвращает `true`, если изменились
    /// selection или scroll offset.
    pub fn navigate(
        &mut self,
        key: Key,
        shift: bool,
        control: bool,
        scroll: &mut ScrollModel,
    ) -> bool {
        if !self.configured || self.item_count == 0 {
            return false;
        }
        if control && key == Key::Character('a') {
            let before = self.selection;
            self.selection.select_all(self.item_count);
            return self.selection != before;
        }
        let current = self
            .selection
            .current()
            .unwrap_or(0)
            .min(self.item_count - 1);
        let page_items = scroll.viewport_size().div_ceil(self.item_extent).max(1);
        let next = match key {
            Key::Up => current.saturating_sub(1),
            Key::Down => current.saturating_add(1).min(self.item_count - 1),
            Key::PageUp => current.saturating_sub(page_items),
            Key::PageDown => current.saturating_add(page_items).min(self.item_count - 1),
            Key::Home => 0,
            Key::End => self.item_count - 1,
            _ => return false,
        };
        let before = self.selection;
        if shift {
            self.selection.extend_to(next);
        } else {
            self.selection.select(next);
        }
        let start = u64::from(next) * u64::from(self.item_extent);
        let scrolled =
            scroll.ensure_visible(start, start.saturating_add(u64::from(self.item_extent)));
        self.selection != before || scrolled
    }

    fn clamp_selection(&mut self) {
        if self.item_count == 0 {
            self.selection.clear();
        } else if self
            .selection
            .current()
            .is_some_and(|current| current >= self.item_count)
        {
            self.selection.select(self.item_count - 1);
        }
    }
}

impl Default for ListViewState {
    fn default() -> Self {
        Self::disabled()
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

    #[test]
    fn keyboard_selection_scrolls_selected_item_into_view() {
        let mut list = ListViewState::disabled();
        list.configure(50_000, 24, SelectionMode::Extended);
        let mut scroll = ScrollModel::new();
        scroll.set_extents(240, list.content_extent());
        assert!(list.navigate(Key::End, false, false, &mut scroll));
        assert_eq!(list.selection().current(), Some(49_999));
        assert_eq!(scroll.offset(), scroll.maximum());
        assert!(list.visible_range(scroll).len() <= 15);
    }

    #[test]
    fn extended_pointer_selection_does_not_materialize_range() {
        let mut list = ListViewState::disabled();
        list.configure(50_000, 20, SelectionMode::Extended);
        let mut scroll = ScrollModel::new();
        scroll.set_extents(200, list.content_extent());
        assert!(list.select_at(10, scroll, false, false));
        scroll.scroll_to(20_000);
        assert!(list.select_at(10, scroll, true, false));
        assert_eq!(list.selected_ranges().len(), 1);
    }
}

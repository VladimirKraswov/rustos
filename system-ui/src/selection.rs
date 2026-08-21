//! Общая selection model для виртуализированных коллекций.
//!
//! Selection хранит logical item ranges, а не `NodeId`: recycling визуальных
//! delegates не теряет выбор и не создаёт узел на каждый элемент.

/// Режим выбора коллекции.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SelectionMode {
    /// Выбор отключён.
    None = 0,
    /// Ровно один current item.
    #[default]
    Single = 1,
    /// Несвязанные элементы через toggle.
    Multiple = 2,
    /// Desktop extended selection с anchor и диапазоном Shift.
    Extended = 3,
}

/// Полуоткрытый выбранный диапазон `[start, end)`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SelectionRange {
    /// Первый индекс.
    pub start: u32,
    /// Индекс после последнего.
    pub end: u32,
}

impl SelectionRange {
    const EMPTY: Self = Self { start: 0, end: 0 };

    /// Нормализованный непустой диапазон.
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// Содержит ли диапазон индекс.
    pub const fn contains(self, index: u32) -> bool {
        index >= self.start && index < self.end
    }
}

/// Ошибка bounded selection model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionError {
    /// Для ещё одного несвязанного диапазона не хватает configured capacity.
    Capacity,
}

/// Selection model с bounded числом несвязанных диапазонов.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionModel<const R: usize> {
    mode: SelectionMode,
    ranges: [SelectionRange; R],
    len: usize,
    current: Option<u32>,
    anchor: Option<u32>,
}

impl<const R: usize> SelectionModel<R> {
    /// Пустая модель выбранного режима.
    pub const fn new(mode: SelectionMode) -> Self {
        Self {
            mode,
            ranges: [SelectionRange::EMPTY; R],
            len: 0,
            current: None,
            anchor: None,
        }
    }

    /// Режим.
    pub const fn mode(&self) -> SelectionMode {
        self.mode
    }

    /// Меняет режим и очищает несовместимое состояние.
    pub fn set_mode(&mut self, mode: SelectionMode) {
        if self.mode != mode {
            self.clear();
            self.mode = mode;
        }
    }

    /// Нормализованные отсортированные диапазоны.
    pub fn ranges(&self) -> &[SelectionRange] {
        &self.ranges[..self.len]
    }

    /// Текущий keyboard item.
    pub const fn current(&self) -> Option<u32> {
        self.current
    }

    /// Anchor extended selection.
    pub const fn anchor(&self) -> Option<u32> {
        self.anchor
    }

    /// Есть ли выбранные элементы.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Проверяет item независимо от materialized delegate.
    pub fn contains(&self, index: u32) -> bool {
        self.ranges().iter().any(|range| range.contains(index))
    }

    /// Снимает выбор и current/anchor.
    pub fn clear(&mut self) {
        self.len = 0;
        self.current = None;
        self.anchor = None;
    }

    /// Заменяет выбор одним item.
    pub fn select(&mut self, index: u32) {
        if self.mode == SelectionMode::None || R == 0 {
            return;
        }
        self.ranges[0] = SelectionRange::new(index, index.saturating_add(1));
        self.len = 1;
        self.current = Some(index);
        self.anchor = Some(index);
    }

    /// Переключает item в Multiple/Extended mode.
    pub fn toggle(&mut self, index: u32) -> Result<(), SelectionError> {
        if matches!(self.mode, SelectionMode::None) {
            return Ok(());
        }
        if self.mode == SelectionMode::Single {
            self.select(index);
            return Ok(());
        }
        if self.contains(index) {
            self.remove_index(index);
        } else {
            self.insert_range(SelectionRange::new(index, index.saturating_add(1)))?;
        }
        self.current = Some(index);
        self.anchor.get_or_insert(index);
        Ok(())
    }

    /// Заменяет выбор диапазоном от anchor до нового current item.
    pub fn extend_to(&mut self, index: u32) {
        if self.mode != SelectionMode::Extended || R == 0 {
            self.select(index);
            return;
        }
        let anchor = self.anchor.unwrap_or(index);
        let start = anchor.min(index);
        let end = anchor.max(index).saturating_add(1);
        self.ranges[0] = SelectionRange::new(start, end);
        self.len = 1;
        self.current = Some(index);
        self.anchor = Some(anchor);
    }

    /// Выбирает все items без materialization каждого элемента.
    pub fn select_all(&mut self, item_count: u32) {
        if !matches!(self.mode, SelectionMode::Multiple | SelectionMode::Extended)
            || item_count == 0
            || R == 0
        {
            return;
        }
        self.ranges[0] = SelectionRange::new(0, item_count);
        self.len = 1;
        self.current = Some(item_count - 1);
        self.anchor = Some(0);
    }

    /// Обновляет индексы после удаления items `[start, start + count)`.
    pub fn items_removed(&mut self, start: u32, count: u32) {
        if count == 0 {
            return;
        }
        let end = start.saturating_add(count);
        let old = self.ranges;
        let old_len = self.len;
        self.len = 0;
        for range in old.iter().copied().take(old_len) {
            if range.start < start {
                let left_end = range.end.min(start);
                if left_end > range.start {
                    let _ = self.insert_range(SelectionRange::new(range.start, left_end));
                }
            }
            if range.end > end {
                let shifted_start = range.start.max(end).saturating_sub(count);
                let shifted_end = range.end.saturating_sub(count);
                if shifted_end > shifted_start {
                    let _ = self.insert_range(SelectionRange::new(shifted_start, shifted_end));
                }
            }
        }
        self.current = shift_index(self.current, start, end, count);
        self.anchor = shift_index(self.anchor, start, end, count);
    }

    fn remove_index(&mut self, index: u32) {
        let mut output = [SelectionRange::EMPTY; R];
        let mut output_len = 0;
        for range in self.ranges().iter().copied() {
            if !range.contains(index) {
                if output_len < R {
                    output[output_len] = range;
                    output_len += 1;
                }
                continue;
            }
            if range.start < index && output_len < R {
                output[output_len] = SelectionRange::new(range.start, index);
                output_len += 1;
            }
            let right_start = index.saturating_add(1);
            if right_start < range.end && output_len < R {
                output[output_len] = SelectionRange::new(right_start, range.end);
                output_len += 1;
            }
        }
        self.ranges = output;
        self.len = output_len;
    }

    fn insert_range(&mut self, mut inserted: SelectionRange) -> Result<(), SelectionError> {
        if inserted.start >= inserted.end {
            return Ok(());
        }
        let old = self.ranges;
        let old_len = self.len;
        let mut output = [SelectionRange::EMPTY; R];
        let mut output_len = 0;
        let mut placed = false;
        for current in old.iter().copied().take(old_len) {
            if current.end < inserted.start {
                push_range(&mut output, &mut output_len, current)?;
            } else if inserted.end < current.start {
                if !placed {
                    push_range(&mut output, &mut output_len, inserted)?;
                    placed = true;
                }
                push_range(&mut output, &mut output_len, current)?;
            } else {
                inserted.start = inserted.start.min(current.start);
                inserted.end = inserted.end.max(current.end);
            }
        }
        if !placed {
            push_range(&mut output, &mut output_len, inserted)?;
        }
        self.ranges = output;
        self.len = output_len;
        Ok(())
    }
}

fn push_range<const R: usize>(
    output: &mut [SelectionRange; R],
    len: &mut usize,
    range: SelectionRange,
) -> Result<(), SelectionError> {
    if *len == R {
        return Err(SelectionError::Capacity);
    }
    output[*len] = range;
    *len += 1;
    Ok(())
}

fn shift_index(value: Option<u32>, start: u32, end: u32, count: u32) -> Option<u32> {
    value.and_then(|index| {
        if index < start {
            Some(index)
        } else if index >= end {
            Some(index.saturating_sub(count))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_selection_uses_logical_ranges() {
        let mut selection = SelectionModel::<8>::new(SelectionMode::Extended);
        selection.select(10);
        selection.extend_to(15);
        assert_eq!(selection.ranges(), &[SelectionRange::new(10, 16)]);
        assert_eq!(selection.current(), Some(15));
        assert_eq!(selection.anchor(), Some(10));
    }

    #[test]
    fn multiple_toggle_merges_and_splits_ranges() {
        let mut selection = SelectionModel::<8>::new(SelectionMode::Multiple);
        selection.toggle(4).unwrap();
        selection.toggle(5).unwrap();
        selection.toggle(6).unwrap();
        assert_eq!(selection.ranges(), &[SelectionRange::new(4, 7)]);
        selection.toggle(5).unwrap();
        assert_eq!(
            selection.ranges(),
            &[SelectionRange::new(4, 5), SelectionRange::new(6, 7)]
        );
    }

    #[test]
    fn item_removal_preserves_selection_after_removed_range() {
        let mut selection = SelectionModel::<8>::new(SelectionMode::Multiple);
        selection.toggle(2).unwrap();
        selection.toggle(8).unwrap();
        selection.items_removed(4, 3);
        assert!(selection.contains(2));
        assert!(selection.contains(5));
        assert!(!selection.contains(8));
    }

    #[test]
    fn select_all_is_constant_size() {
        let mut selection = SelectionModel::<1>::new(SelectionMode::Extended);
        selection.select_all(50_000);
        assert_eq!(selection.ranges(), &[SelectionRange::new(0, 50_000)]);
    }
}

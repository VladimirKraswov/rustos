//! Bounded damage tracker для compositor'а без heap allocation.

use crate::Rect;

pub struct DamageRegion<const CAPACITY: usize> {
    bounds: Rect,
    rects: [Rect; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> DamageRegion<CAPACITY> {
    pub fn new(bounds: Rect) -> Self {
        assert!(CAPACITY > 0, "damage capacity must be positive");
        Self {
            bounds,
            rects: [Rect::EMPTY; CAPACITY],
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.rects[..self.len].fill(Rect::EMPTY);
        self.len = 0;
    }

    pub fn add(&mut self, rect: Rect) {
        let mut merged = rect.intersection(self.bounds);
        if merged.is_empty() {
            return;
        }
        let mut index = 0;
        while index < self.len {
            if should_merge(merged, self.rects[index]) {
                merged = merged.union(self.rects[index]);
                self.remove(index);
                index = 0;
            } else {
                index += 1;
            }
        }
        if self.len < CAPACITY {
            self.rects[self.len] = merged;
            self.len += 1;
            return;
        }
        // Переполнение не теряет damage: bounded metadata схлопывается в
        // один больший rectangle, что дороже, но всегда корректно визуально.
        for existing in &self.rects[..self.len] {
            merged = merged.union(*existing);
        }
        self.rects.fill(Rect::EMPTY);
        self.rects[0] = merged.intersection(self.bounds);
        self.len = 1;
    }

    pub fn iter(&self) -> core::slice::Iter<'_, Rect> {
        self.rects[..self.len].iter()
    }

    pub fn as_slice(&self) -> &[Rect] {
        &self.rects[..self.len]
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn covered_pixels(&self) -> u64 {
        self.rects[..self.len].iter().map(|rect| rect.area()).sum()
    }

    fn remove(&mut self, index: usize) {
        self.rects.copy_within(index + 1..self.len, index);
        self.len -= 1;
        self.rects[self.len] = Rect::EMPTY;
    }
}

/// Объединяем пересечение и дешёвые соседние rectangles, но не превращаем
/// L-образный контур окна в огромный bounding box. Допустим не более 50%
/// лишних пикселей относительно двух исходных областей.
fn should_merge(left: Rect, right: Rect) -> bool {
    if !left.touches(right) {
        return false;
    }
    let separate = left.area().saturating_add(right.area());
    left.union(right).area() <= separate.saturating_add(separate / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_is_clipped_and_touching_rectangles_merge() {
        let mut damage = DamageRegion::<4>::new(Rect::new(0, 0, 100, 100));
        damage.add(Rect::new(-10, 5, 20, 10));
        damage.add(Rect::new(10, 5, 5, 10));
        assert_eq!(damage.len(), 1);
        assert_eq!(damage.iter().next().copied(), Some(Rect::new(0, 5, 15, 10)));
    }

    #[test]
    fn overflow_collapses_without_losing_pixels() {
        let mut damage = DamageRegion::<2>::new(Rect::new(0, 0, 100, 100));
        damage.add(Rect::new(0, 0, 2, 2));
        damage.add(Rect::new(20, 20, 2, 2));
        damage.add(Rect::new(40, 40, 2, 2));
        assert_eq!(damage.len(), 1);
        assert_eq!(damage.iter().next().copied(), Some(Rect::new(0, 0, 42, 42)));
    }

    #[test]
    fn thin_window_outline_does_not_expand_to_full_window() {
        let mut damage = DamageRegion::<4>::new(Rect::new(0, 0, 200, 200));
        damage.add(Rect::new(20, 20, 100, 10));
        damage.add(Rect::new(20, 30, 2, 90));
        assert_eq!(damage.len(), 2);
        assert_eq!(damage.covered_pixels(), 1_180);
    }
}

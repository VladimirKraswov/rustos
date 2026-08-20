//! Целочисленная геометрия без floating point и heap.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const EMPTY: Self = Self::new(0, 0, 0, 0);

    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn right(self) -> i32 {
        self.x
            .saturating_add(self.width.min(i32::MAX as u32) as i32)
    }

    pub fn bottom(self) -> i32 {
        self.y
            .saturating_add(self.height.min(i32::MAX as u32) as i32)
    }

    pub fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    pub fn translated(self, offset: Point) -> Self {
        Self::new(
            self.x.saturating_add(offset.x),
            self.y.saturating_add(offset.y),
            self.width,
            self.height,
        )
    }

    pub fn intersection(self, other: Self) -> Self {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x0 >= x1 || y0 >= y1 {
            return Self::EMPTY;
        }
        Self::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
    }

    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Self::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
    }

    pub fn touches(self, other: Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x <= other.right()
            && self.right() >= other.x
            && self.y <= other.bottom()
            && self.bottom() >= other.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_and_union_are_clipped() {
        let left = Rect::new(-5, 3, 12, 8);
        let right = Rect::new(2, 0, 10, 7);
        assert_eq!(left.intersection(right), Rect::new(2, 3, 5, 4));
        assert_eq!(left.union(right), Rect::new(-5, 0, 17, 11));
    }
}

//! 64-битный allocator физических блоков VaraniaFS.
//!
//! В памяти хранится небольшой отсортированный cache свободных диапазонов;
//! полная истина остаётся в space B+tree. Cache можно заполнять потоковым
//! обходом дерева, он объединяет соседние ranges и выбирает best-fit, не
//! создавая по одному extent на каждый блок.

use crate::format::{Error, FIRST_ALLOCATABLE_BLOCK};

pub const MAX_CACHED_EXTENTS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub start: u64,
    pub blocks: u64,
}

impl Extent {
    pub const EMPTY: Self = Self {
        start: 0,
        blocks: 0,
    };

    pub fn end(self) -> Result<u64, Error> {
        self.start.checked_add(self.blocks).ok_or(Error::Capacity)
    }
}

pub struct BlockAllocator {
    volume_blocks: u64,
    high_water: u64,
    free: [Extent; MAX_CACHED_EXTENTS],
    free_count: usize,
}

impl BlockAllocator {
    pub(crate) const fn empty() -> Self {
        Self {
            volume_blocks: 0,
            high_water: 0,
            free: [Extent::EMPTY; MAX_CACHED_EXTENTS],
            free_count: 0,
        }
    }

    pub fn new(volume_blocks: u64, high_water: u64) -> Result<Self, Error> {
        if high_water < FIRST_ALLOCATABLE_BLOCK || high_water > volume_blocks {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            volume_blocks,
            high_water,
            free: [Extent::EMPTY; MAX_CACHED_EXTENTS],
            free_count: 0,
        })
    }

    pub const fn high_water(&self) -> u64 {
        self.high_water
    }

    pub const fn free_count(&self) -> usize {
        self.free_count
    }

    pub fn free_extent(&self, index: usize) -> Option<Extent> {
        self.free
            .get(index)
            .copied()
            .filter(|extent| extent.blocks != 0)
    }

    pub fn add_free(&mut self, extent: Extent) -> Result<(), Error> {
        let end = extent.end()?;
        if extent.blocks == 0 || extent.start < FIRST_ALLOCATABLE_BLOCK || end > self.volume_blocks
        {
            return Err(Error::InvalidArgument);
        }
        let mut index = 0usize;
        while index < self.free_count && self.free[index].start < extent.start {
            index += 1;
        }
        if index != 0 && self.free[index - 1].end()? > extent.start {
            return Err(Error::InvalidArgument);
        }
        if index < self.free_count && end > self.free[index].start {
            return Err(Error::InvalidArgument);
        }

        let merge_left = index != 0 && self.free[index - 1].end()? == extent.start;
        let merge_right = index < self.free_count && end == self.free[index].start;
        match (merge_left, merge_right) {
            (true, true) => {
                let right_end = self.free[index].end()?;
                self.free[index - 1].blocks = right_end - self.free[index - 1].start;
                self.remove(index);
            }
            (true, false) => self.free[index - 1].blocks += extent.blocks,
            (false, true) => {
                self.free[index].start = extent.start;
                self.free[index].blocks += extent.blocks;
            }
            (false, false) => {
                if self.free_count == MAX_CACHED_EXTENTS {
                    return Err(Error::Capacity);
                }
                for cursor in (index..self.free_count).rev() {
                    self.free[cursor + 1] = self.free[cursor];
                }
                self.free[index] = extent;
                self.free_count += 1;
            }
        }
        self.trim_high_water();
        Ok(())
    }

    pub fn allocate_metadata_pair(&mut self) -> Result<u64, Error> {
        self.allocate(2, 2)
    }

    /// Используется при перестройке самого space tree: его nodes нельзя
    /// одновременно брать из extent, запись о котором сейчас удаляется.
    pub fn allocate_metadata_pair_from_tail(&mut self) -> Result<u64, Error> {
        let start = align_up(self.high_water, 2)?;
        let end = start.checked_add(2).ok_or(Error::Capacity)?;
        if end > self.volume_blocks {
            return Err(Error::Capacity);
        }
        if start != self.high_water {
            self.add_free(Extent {
                start: self.high_water,
                blocks: start - self.high_water,
            })?;
        }
        self.high_water = end;
        Ok(start)
    }

    pub fn allocate_data(&mut self, blocks: u64, alignment: u64) -> Result<Extent, Error> {
        let start = self.allocate(blocks, alignment.max(1))?;
        Ok(Extent { start, blocks })
    }

    pub fn release(&mut self, extent: Extent) -> Result<(), Error> {
        self.add_free(extent)
    }

    fn allocate(&mut self, blocks: u64, alignment: u64) -> Result<u64, Error> {
        if blocks == 0 || !alignment.is_power_of_two() {
            return Err(Error::InvalidArgument);
        }
        let mut best: Option<(usize, u64, u64)> = None;
        for index in 0..self.free_count {
            let extent = self.free[index];
            let aligned = align_up(extent.start, alignment)?;
            let end = aligned.checked_add(blocks).ok_or(Error::Capacity)?;
            if end > extent.end()? {
                continue;
            }
            let waste = extent.blocks - blocks;
            if best.is_none_or(|(_, _, current)| waste < current) {
                best = Some((index, aligned, waste));
            }
        }
        if let Some((index, start, _)) = best {
            self.consume_free(index, start, blocks)?;
            return Ok(start);
        }

        let start = align_up(self.high_water, alignment)?;
        let end = start.checked_add(blocks).ok_or(Error::Capacity)?;
        if end > self.volume_blocks {
            return Err(Error::Capacity);
        }
        // Alignment gap не теряется: он сразу становится свободным extent.
        if start != self.high_water {
            let gap = Extent {
                start: self.high_water,
                blocks: start - self.high_water,
            };
            self.high_water = end;
            self.add_free(gap)?;
        } else {
            self.high_water = end;
        }
        Ok(start)
    }

    fn consume_free(&mut self, index: usize, start: u64, blocks: u64) -> Result<(), Error> {
        let original = self.free[index];
        let original_end = original.end()?;
        let allocation_end = start.checked_add(blocks).ok_or(Error::Capacity)?;
        let prefix = start - original.start;
        let suffix = original_end - allocation_end;
        match (prefix, suffix) {
            (0, 0) => self.remove(index),
            (0, suffix) => {
                self.free[index].start = allocation_end;
                self.free[index].blocks = suffix;
            }
            (prefix, 0) => self.free[index].blocks = prefix,
            (prefix, suffix) => {
                if self.free_count == MAX_CACHED_EXTENTS {
                    return Err(Error::Capacity);
                }
                for cursor in (index + 1..self.free_count).rev() {
                    self.free[cursor + 1] = self.free[cursor];
                }
                self.free[index].blocks = prefix;
                self.free[index + 1] = Extent {
                    start: allocation_end,
                    blocks: suffix,
                };
                self.free_count += 1;
            }
        }
        Ok(())
    }

    fn remove(&mut self, index: usize) {
        for cursor in index..self.free_count - 1 {
            self.free[cursor] = self.free[cursor + 1];
        }
        self.free_count -= 1;
        self.free[self.free_count] = Extent::EMPTY;
    }

    fn trim_high_water(&mut self) {
        while self.free_count != 0 {
            let last = self.free[self.free_count - 1];
            if last.end().ok() != Some(self.high_water) {
                break;
            }
            self.high_water = last.start;
            self.remove(self.free_count - 1);
        }
    }
}

fn align_up(value: u64, alignment: u64) -> Result<u64, Error> {
    value
        .checked_add(alignment - 1)
        .map(|candidate| candidate & !(alignment - 1))
        .ok_or(Error::Capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_both_sides_and_rejects_overlap() {
        let mut allocator = BlockAllocator::new(10_000, 1000).unwrap();
        allocator
            .add_free(Extent {
                start: 200,
                blocks: 10,
            })
            .unwrap();
        allocator
            .add_free(Extent {
                start: 220,
                blocks: 10,
            })
            .unwrap();
        allocator
            .add_free(Extent {
                start: 210,
                blocks: 10,
            })
            .unwrap();
        assert_eq!(allocator.free_count(), 1);
        assert_eq!(
            allocator.free_extent(0),
            Some(Extent {
                start: 200,
                blocks: 30
            })
        );
        assert_eq!(
            allocator.add_free(Extent {
                start: 205,
                blocks: 1
            }),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn best_fit_alignment_and_metadata_pairs_are_exact() {
        let mut allocator = BlockAllocator::new(10_000, 1001).unwrap();
        allocator
            .add_free(Extent {
                start: 200,
                blocks: 20,
            })
            .unwrap();
        allocator
            .add_free(Extent {
                start: 300,
                blocks: 4,
            })
            .unwrap();
        assert_eq!(allocator.allocate_metadata_pair().unwrap(), 300);
        assert_eq!(
            allocator.allocate_data(3, 8).unwrap(),
            Extent {
                start: 200,
                blocks: 3
            }
        );
        let metadata = allocator.allocate_metadata_pair().unwrap();
        assert_eq!(metadata & 1, 0);
    }

    #[test]
    fn freeing_tail_moves_high_water_back_without_leak() {
        let mut allocator = BlockAllocator::new(10_000, 1000).unwrap();
        let extent = allocator.allocate_data(128, 1).unwrap();
        assert_eq!(allocator.high_water(), 1128);
        allocator.release(extent).unwrap();
        assert_eq!(allocator.high_water(), 1000);
        assert_eq!(allocator.free_count(), 0);
    }
}

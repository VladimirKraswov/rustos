//! Потокобезопасный allocator физических 4-KiB кадров на свободных extent'ах.

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

use rustos_abi::{BootInfo, MemRegionKind, PAGE_SIZE};

const MAX_EXTENTS: usize = 256;
/// Низкая память содержит legacy/firmware structures даже когда firmware
/// помечает часть её conventional. Обычные kernel allocations начинаем с 1 MiB.
const MIN_ALLOCATABLE: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Extent {
    base: u64,
    frames: u64,
}

impl Extent {
    const EMPTY: Self = Self { base: 0, frames: 0 };

    const fn end(self) -> u64 {
        self.base + self.frames * PAGE_SIZE
    }
}

/// Непрерывный диапазон физических кадров.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameBlock {
    pub phys: u64,
    pub frames: u64,
}

/// Сводка allocator'а для boot diagnostics и будущего task manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameStats {
    pub total_frames: u64,
    pub free_frames: u64,
    pub extents: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameAllocatorError {
    AlreadyInitialized,
    NotInitialized,
    InvalidRequest,
    OutOfMemory,
    TooFragmented,
    InvalidFree,
}

struct FrameAllocator {
    extents: [Extent; MAX_EXTENTS],
    len: usize,
    total_frames: u64,
}

impl FrameAllocator {
    const fn empty() -> Self {
        Self {
            extents: [Extent::EMPTY; MAX_EXTENTS],
            len: 0,
            total_frames: 0,
        }
    }

    fn initialize(&mut self, info: &BootInfo) -> Result<(), FrameAllocatorError> {
        self.len = 0;
        self.total_frames = 0;
        for region in info.memmap.iter().take(info.memmap_count as usize) {
            if region.kind != MemRegionKind::Usable as u32 {
                continue;
            }
            let region_end = region
                .phys_start
                .checked_add(region.size)
                .ok_or(FrameAllocatorError::InvalidRequest)?;
            let start = align_up(region.phys_start.max(MIN_ALLOCATABLE), PAGE_SIZE)
                .ok_or(FrameAllocatorError::InvalidRequest)?;
            let end = align_down(region_end, PAGE_SIZE);
            if start >= end {
                continue;
            }
            let frames = (end - start) / PAGE_SIZE;
            self.push(Extent {
                base: start,
                frames,
            })?;
            self.total_frames += frames;
        }
        self.normalize();
        if self.len == 0 {
            return Err(FrameAllocatorError::OutOfMemory);
        }
        Ok(())
    }

    fn allocate(
        &mut self,
        frames: u64,
        alignment_frames: u64,
    ) -> Result<FrameBlock, FrameAllocatorError> {
        if frames == 0 || alignment_frames == 0 || !alignment_frames.is_power_of_two() {
            return Err(FrameAllocatorError::InvalidRequest);
        }
        let alignment = alignment_frames
            .checked_mul(PAGE_SIZE)
            .ok_or(FrameAllocatorError::InvalidRequest)?;
        let bytes = frames
            .checked_mul(PAGE_SIZE)
            .ok_or(FrameAllocatorError::InvalidRequest)?;
        for index in 0..self.len {
            let extent = self.extents[index];
            let Some(start) = align_up(extent.base, alignment) else {
                continue;
            };
            let Some(end) = start.checked_add(bytes) else {
                continue;
            };
            if end > extent.end() {
                continue;
            }

            self.remove(index);
            if extent.base < start {
                self.push(Extent {
                    base: extent.base,
                    frames: (start - extent.base) / PAGE_SIZE,
                })?;
            }
            if end < extent.end() {
                self.push(Extent {
                    base: end,
                    frames: (extent.end() - end) / PAGE_SIZE,
                })?;
            }
            self.normalize();
            return Ok(FrameBlock {
                phys: start,
                frames,
            });
        }
        Err(FrameAllocatorError::OutOfMemory)
    }

    fn free(&mut self, block: FrameBlock) -> Result<(), FrameAllocatorError> {
        if block.frames == 0 || !block.phys.is_multiple_of(PAGE_SIZE) {
            return Err(FrameAllocatorError::InvalidFree);
        }
        let end = block
            .phys
            .checked_add(
                block
                    .frames
                    .checked_mul(PAGE_SIZE)
                    .ok_or(FrameAllocatorError::InvalidFree)?,
            )
            .ok_or(FrameAllocatorError::InvalidFree)?;
        if self.extents[..self.len]
            .iter()
            .any(|extent| block.phys < extent.end() && end > extent.base)
        {
            return Err(FrameAllocatorError::InvalidFree);
        }
        self.push(Extent {
            base: block.phys,
            frames: block.frames,
        })?;
        self.normalize();
        Ok(())
    }

    fn push(&mut self, extent: Extent) -> Result<(), FrameAllocatorError> {
        if extent.frames == 0 {
            return Ok(());
        }
        if self.len == MAX_EXTENTS {
            return Err(FrameAllocatorError::TooFragmented);
        }
        self.extents[self.len] = extent;
        self.len += 1;
        Ok(())
    }

    fn remove(&mut self, index: usize) {
        for cursor in index..self.len - 1 {
            self.extents[cursor] = self.extents[cursor + 1];
        }
        self.len -= 1;
        self.extents[self.len] = Extent::EMPTY;
    }

    fn normalize(&mut self) {
        // UEFI обычно даёт уже отсортированную карту, но free() добавляет
        // extent в конец. При MAX_EXTENTS=256 insertion sort прост и bounded.
        for index in 1..self.len {
            let value = self.extents[index];
            let mut cursor = index;
            while cursor > 0 && self.extents[cursor - 1].base > value.base {
                self.extents[cursor] = self.extents[cursor - 1];
                cursor -= 1;
            }
            self.extents[cursor] = value;
        }
        let mut index = 0;
        while index + 1 < self.len {
            if self.extents[index].end() == self.extents[index + 1].base {
                self.extents[index].frames += self.extents[index + 1].frames;
                self.remove(index + 1);
            } else {
                index += 1;
            }
        }
    }

    fn stats(&self) -> FrameStats {
        FrameStats {
            total_frames: self.total_frames,
            free_frames: self.extents[..self.len]
                .iter()
                .map(|extent| extent.frames)
                .sum(),
            extents: self.len,
        }
    }
}

struct LockedAllocator {
    locked: AtomicBool,
    initialized: AtomicBool,
    value: UnsafeCell<FrameAllocator>,
}

// Доступ к value сериализуется spinlock'ом. В interrupt context allocator
// позднее будет вызываться только с local IRQ disabled.
unsafe impl Sync for LockedAllocator {}

impl LockedAllocator {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            value: UnsafeCell::new(FrameAllocator::empty()),
        }
    }

    fn with<R>(
        &self,
        operation: impl FnOnce(&mut FrameAllocator) -> Result<R, FrameAllocatorError>,
    ) -> Result<R, FrameAllocatorError> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: lock удерживается текущим CPU до Release store ниже.
        let result = operation(unsafe { &mut *self.value.get() });
        self.locked.store(false, Ordering::Release);
        result
    }
}

static ALLOCATOR: LockedAllocator = LockedAllocator::new();

/// Один раз импортирует usable extent'ы BootInfo.
pub fn initialize(info: &BootInfo) -> Result<FrameStats, FrameAllocatorError> {
    if ALLOCATOR
        .initialized
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(FrameAllocatorError::AlreadyInitialized);
    }
    let result = ALLOCATOR.with(|allocator| {
        allocator.initialize(info)?;
        Ok(allocator.stats())
    });
    if result.is_err() {
        ALLOCATOR.initialized.store(false, Ordering::Release);
    }
    result
}

/// Выделяет `frames` непрерывных кадров с выравниванием в кадрах.
pub fn allocate(frames: u64, alignment_frames: u64) -> Result<FrameBlock, FrameAllocatorError> {
    if !ALLOCATOR.initialized.load(Ordering::Acquire) {
        return Err(FrameAllocatorError::NotInitialized);
    }
    ALLOCATOR.with(|allocator| allocator.allocate(frames, alignment_frames))
}

/// Возвращает ранее выделенный непрерывный диапазон.
pub fn free(block: FrameBlock) -> Result<(), FrameAllocatorError> {
    if !ALLOCATOR.initialized.load(Ordering::Acquire) {
        return Err(FrameAllocatorError::NotInitialized);
    }
    ALLOCATOR.with(|allocator| allocator.free(block))
}

/// Возвращает моментальный снимок allocator'а. Он используется не только для
/// диагностики: lifecycle test сравнивает число свободных кадров до создания
/// address space и после его уничтожения.
pub fn stats() -> Result<FrameStats, FrameAllocatorError> {
    if !ALLOCATOR.initialized.load(Ordering::Acquire) {
        return Err(FrameAllocatorError::NotInitialized);
    }
    ALLOCATOR.with(|allocator| Ok(allocator.stats()))
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

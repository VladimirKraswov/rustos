//! Virtio 1.x MMIO block transport для QEMU `virt` на AArch64.
//!
//! Драйвер синхронный и polling-only — это bootstrap transport до переноса
//! blockd в user space. Он всё равно соблюдает современный virtio handshake,
//! использует physical DMA addresses, проверяет capacity/status/timeout и не
//! смешивает filesystem semantics с транспортом.

use core::{
    cell::UnsafeCell,
    ptr,
    sync::atomic::{fence, AtomicBool, Ordering},
};

use super::{BlockError, BlockInfo};
use crate::memory::{self, FrameBlock};

const MMIO_FIRST: u64 = 0x0a00_0000;
const MMIO_STRIDE: u64 = 0x200;
const MMIO_SLOTS: u64 = 32;
const MAGIC: u32 = 0x7472_6976;
const VERSION_MODERN: u32 = 2;
const DEVICE_BLOCK: u32 = 2;

const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_INTERRUPT_STATUS: u64 = 0x060;
const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_AVAIL_LOW: u64 = 0x090;
const REG_QUEUE_USED_LOW: u64 = 0x0a0;
const REG_CONFIG: u64 = 0x100;

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const FEATURE_FLUSH: u32 = 1 << 9;
const FEATURE_VERSION_1_WORD1: u32 = 1;

const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const REQUEST_IN: u32 = 0;
const REQUEST_OUT: u32 = 1;
const REQUEST_FLUSH: u32 = 4;
const QUEUE_SIZE: u16 = 128;
const POLL_LIMIT: usize = 50_000_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RequestHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

struct Device {
    base: u64,
    capacity_sectors: u64,
    queue_size: u16,
    queue: FrameBlock,
    dma: FrameBlock,
    last_used: u16,
    flush_supported: bool,
}

struct LockedDevice {
    locked: AtomicBool,
    value: UnsafeCell<Option<Device>>,
}

unsafe impl Sync for LockedDevice {}

impl LockedDevice {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }

    fn with<R>(
        &self,
        operation: impl FnOnce(&mut Device) -> Result<R, BlockError>,
    ) -> Result<R, BlockError> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        let result = unsafe { &mut *self.value.get() }
            .as_mut()
            .ok_or(BlockError::Unsupported)
            .and_then(operation);
        self.locked.store(false, Ordering::Release);
        result
    }

    fn install(&self, device: Device) -> Result<(), BlockError> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        let slot = unsafe { &mut *self.value.get() };
        let result = if slot.is_none() {
            *slot = Some(device);
            Ok(())
        } else {
            Err(BlockError::Device)
        };
        self.locked.store(false, Ordering::Release);
        result
    }
}

static DEVICE: LockedDevice = LockedDevice::new();

pub fn initialize() -> Result<BlockInfo, BlockError> {
    let base = find_device().ok_or(BlockError::Unsupported)?;
    write32(base, REG_STATUS, 0);
    write32(base, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    write32(base, REG_DEVICE_FEATURES_SEL, 1);
    if read32(base, REG_DEVICE_FEATURES) & FEATURE_VERSION_1_WORD1 == 0 {
        return Err(BlockError::Device);
    }
    write32(base, REG_DEVICE_FEATURES_SEL, 0);
    let word0 = read32(base, REG_DEVICE_FEATURES);
    let accepted0 = word0 & FEATURE_FLUSH;
    write32(base, REG_DRIVER_FEATURES_SEL, 0);
    write32(base, REG_DRIVER_FEATURES, accepted0);
    write32(base, REG_DRIVER_FEATURES_SEL, 1);
    write32(base, REG_DRIVER_FEATURES, FEATURE_VERSION_1_WORD1);

    let status = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
    write32(base, REG_STATUS, status);
    if read32(base, REG_STATUS) & STATUS_FEATURES_OK == 0 {
        return Err(BlockError::Device);
    }

    write32(base, REG_QUEUE_SEL, 0);
    let maximum = read32(base, REG_QUEUE_NUM_MAX);
    if maximum < 3 {
        return Err(BlockError::Device);
    }
    let queue_size = u32::from(QUEUE_SIZE).min(maximum) as u16;
    let queue = memory::allocate(3, 1).map_err(|_| BlockError::OutOfMemory)?;
    let dma = match memory::allocate(2, 1) {
        Ok(block) => block,
        Err(_) => {
            let _ = memory::free(queue);
            return Err(BlockError::OutOfMemory);
        }
    };
    unsafe {
        (queue.phys as *mut u8).write_bytes(0, 3 * 4096);
        (dma.phys as *mut u8).write_bytes(0, 2 * 4096);
    }
    write32(base, REG_QUEUE_NUM, u32::from(queue_size));
    write_address(base, REG_QUEUE_DESC_LOW, queue.phys);
    write_address(base, REG_QUEUE_AVAIL_LOW, queue.phys + 4096);
    write_address(base, REG_QUEUE_USED_LOW, queue.phys + 8192);
    write32(base, REG_QUEUE_READY, 1);

    let capacity_sectors = read64(base, REG_CONFIG);
    if capacity_sectors < 8 || !capacity_sectors.is_multiple_of(8) {
        let _ = memory::free(queue);
        let _ = memory::free(dma);
        return Err(BlockError::Device);
    }
    write32(base, REG_STATUS, status | STATUS_DRIVER_OK);
    DEVICE.install(Device {
        base,
        capacity_sectors,
        queue_size,
        queue,
        dma,
        last_used: 0,
        flush_supported: accepted0 & FEATURE_FLUSH != 0,
    })?;
    Ok(BlockInfo {
        blocks: capacity_sectors / 8,
        transport: "virtio-blk modern MMIO",
    })
}

pub fn info() -> Result<BlockInfo, BlockError> {
    DEVICE.with(|device| {
        Ok(BlockInfo {
            blocks: device.capacity_sectors / 8,
            transport: "virtio-blk modern MMIO",
        })
    })
}

pub fn read_block(block: u64, output: &mut [u8; 4096]) -> Result<(), BlockError> {
    DEVICE.with(|device| {
        device.request(REQUEST_IN, block, true)?;
        unsafe {
            ptr::copy_nonoverlapping(device.dma.phys as *const u8, output.as_mut_ptr(), 4096)
        };
        Ok(())
    })
}

pub fn write_block(block: u64, input: &[u8; 4096]) -> Result<(), BlockError> {
    DEVICE.with(|device| {
        if block >= device.capacity_sectors / 8 {
            return Err(BlockError::InvalidRange);
        }
        unsafe { ptr::copy_nonoverlapping(input.as_ptr(), device.dma.phys as *mut u8, 4096) };
        device.request(REQUEST_OUT, block, false)
    })
}

pub fn flush() -> Result<(), BlockError> {
    DEVICE.with(|device| {
        if device.flush_supported {
            device.flush_request()
        } else {
            Ok(())
        }
    })
}

impl Device {
    fn request(&mut self, kind: u32, block: u64, device_writes: bool) -> Result<(), BlockError> {
        if block >= self.capacity_sectors / 8 {
            return Err(BlockError::InvalidRange);
        }
        self.write_header(kind, block * 8);
        self.write_descriptor(0, self.dma.phys + 4096, 16, DESC_NEXT, 1);
        self.write_descriptor(
            1,
            self.dma.phys,
            4096,
            DESC_NEXT | if device_writes { DESC_WRITE } else { 0 },
            2,
        );
        self.write_descriptor(2, self.dma.phys + 4112, 1, DESC_WRITE, 0);
        self.submit(0)
    }

    fn flush_request(&mut self) -> Result<(), BlockError> {
        self.write_header(REQUEST_FLUSH, 0);
        self.write_descriptor(0, self.dma.phys + 4096, 16, DESC_NEXT, 1);
        self.write_descriptor(1, self.dma.phys + 4112, 1, DESC_WRITE, 0);
        self.submit(0)
    }

    fn write_header(&mut self, kind: u32, sector: u64) {
        let header = RequestHeader {
            kind,
            reserved: 0,
            sector,
        };
        unsafe {
            (self.dma.phys as *mut RequestHeader)
                .add(256)
                .write_volatile(header);
            (self.dma.phys as *mut u8).add(4112).write_volatile(0xff);
        }
    }

    fn write_descriptor(&self, index: usize, address: u64, length: u32, flags: u16, next: u16) {
        debug_assert!(index < usize::from(self.queue_size));
        unsafe {
            (self.queue.phys as *mut Descriptor)
                .add(index)
                .write_volatile(Descriptor {
                    address,
                    length,
                    flags,
                    next,
                });
        }
    }

    fn submit(&mut self, head: u16) -> Result<(), BlockError> {
        let available = (self.queue.phys + 4096) as *mut u8;
        let index = unsafe { available.add(2).cast::<u16>().read_volatile() };
        let slot = usize::from(index % self.queue_size);
        unsafe {
            available
                .add(4 + slot * 2)
                .cast::<u16>()
                .write_volatile(head);
        }
        fence(Ordering::Release);
        unsafe {
            available
                .add(2)
                .cast::<u16>()
                .write_volatile(index.wrapping_add(1));
        }
        write32(self.base, REG_QUEUE_NOTIFY, 0);

        let wanted = self.last_used.wrapping_add(1);
        let used_index = (self.queue.phys + 8192 + 2) as *const u16;
        let mut completed = false;
        for _ in 0..POLL_LIMIT {
            fence(Ordering::Acquire);
            if unsafe { used_index.read_volatile() } == wanted {
                completed = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !completed {
            return Err(BlockError::Timeout);
        }
        self.last_used = wanted;
        let interrupt = read32(self.base, REG_INTERRUPT_STATUS);
        if interrupt != 0 {
            write32(self.base, REG_INTERRUPT_ACK, interrupt);
        }
        let status = unsafe { (self.dma.phys as *const u8).add(4112).read_volatile() };
        (status == 0).then_some(()).ok_or(BlockError::Device)
    }
}

fn find_device() -> Option<u64> {
    for slot in 0..MMIO_SLOTS {
        let base = MMIO_FIRST + slot * MMIO_STRIDE;
        if read32(base, REG_MAGIC) == MAGIC
            && read32(base, REG_VERSION) == VERSION_MODERN
            && read32(base, REG_DEVICE_ID) == DEVICE_BLOCK
        {
            return Some(base);
        }
    }
    None
}

fn write_address(base: u64, low_register: u64, address: u64) {
    write32(base, low_register, address as u32);
    write32(base, low_register + 4, (address >> 32) as u32);
}

#[inline]
fn read32(base: u64, offset: u64) -> u32 {
    unsafe { ((base + offset) as *const u32).read_volatile() }
}

#[inline]
fn write32(base: u64, offset: u64, value: u32) {
    unsafe { ((base + offset) as *mut u32).write_volatile(value) };
}

#[inline]
fn read64(base: u64, offset: u64) -> u64 {
    u64::from(read32(base, offset)) | (u64::from(read32(base, offset + 4)) << 32)
}

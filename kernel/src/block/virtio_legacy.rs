//! Синхронный virtio-blk legacy transport для первого persistent volume.
//!
//! QEMU создаёт transitional устройство специально для RustOS system disk.
//! Очередь и bounce page физически непрерывны и identity-mapped. Драйвер не
//! доверяет device: проверяет capacity, queue size, status byte и timeout.

use core::{
    cell::UnsafeCell,
    ptr,
    sync::atomic::{fence, AtomicBool, Ordering},
};

use super::{BlockError, BlockInfo};
use crate::{
    arch,
    memory::{self, FrameBlock},
};

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_BLOCK_LEGACY: u16 = 0x1001;
const QUEUE_SELECT: u16 = 0;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const REQUEST_IN: u32 = 0;
const REQUEST_OUT: u32 = 1;
const REQUEST_FLUSH: u32 = 4;
const FEATURE_FLUSH: u32 = 1 << 9;
const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const MAX_QUEUE_SIZE: u16 = 256;
const POLL_LIMIT: usize = 50_000_000;
/// В QEMU platform contract persistent system volume находится в PCI slot 5.
/// Это не универсальное обнаружение томов: будущий user-space `blockd`
/// перечислит все устройства и выберет VaraniaFS по UUID/superblock.
const SYSTEM_DISK_SLOT: u8 = 5;

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
    io: u16,
    capacity_sectors: u64,
    queue_size: u16,
    queue: FrameBlock,
    queue_bytes: u64,
    dma: FrameBlock,
    last_used: u16,
    flush_supported: bool,
}

struct LockedDevice {
    locked: AtomicBool,
    value: UnsafeCell<Option<Device>>,
}

// `value` доступен только под spinlock. Сейчас I/O выполняет CPU0, но такая
// граница сразу корректна и при подключении нескольких service workers.
unsafe impl Sync for LockedDevice {}

impl LockedDevice {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }

    fn lock(&self) {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    fn with<R>(
        &self,
        operation: impl FnOnce(&mut Device) -> Result<R, BlockError>,
    ) -> Result<R, BlockError> {
        self.lock();
        let result = unsafe { &mut *self.value.get() }
            .as_mut()
            .ok_or(BlockError::Unsupported)
            .and_then(operation);
        self.locked.store(false, Ordering::Release);
        result
    }

    fn install(&self, device: Device) -> Result<(), BlockError> {
        self.lock();
        let slot = unsafe { &mut *self.value.get() };
        let result = if slot.is_some() {
            Err(BlockError::Device)
        } else {
            *slot = Some(device);
            Ok(())
        };
        self.locked.store(false, Ordering::Release);
        result
    }
}

static DEVICE: LockedDevice = LockedDevice::new();

pub fn initialize() -> Result<BlockInfo, BlockError> {
    let (slot, io) = find_device().ok_or(BlockError::Unsupported)?;
    enable_pci_device(slot);
    unsafe { arch::outb(io + 18, 0) };
    unsafe { arch::outb(io + 18, STATUS_ACKNOWLEDGE | STATUS_DRIVER) };
    let host_features = unsafe { arch::inl(io) };
    let guest_features = host_features & FEATURE_FLUSH;
    unsafe { arch::outl(io + 4, guest_features) };
    unsafe { arch::outw(io + 14, QUEUE_SELECT) };
    let queue_size = unsafe { arch::inw(io + 12) };
    if !(3..=MAX_QUEUE_SIZE).contains(&queue_size) {
        return Err(BlockError::Device);
    }
    let descriptor_bytes = u64::from(queue_size) * 16;
    let available_bytes = 6 + u64::from(queue_size) * 2;
    let used_offset = align_up(descriptor_bytes + available_bytes, 4096);
    let queue_bytes = used_offset + 6 + u64::from(queue_size) * 8;
    let queue_frames = queue_bytes.div_ceil(4096);
    let queue = memory::allocate(queue_frames, 1).map_err(|_| BlockError::OutOfMemory)?;
    let dma = match memory::allocate(2, 1) {
        Ok(dma) => dma,
        Err(_) => {
            let _ = memory::free(queue);
            return Err(BlockError::OutOfMemory);
        }
    };
    unsafe {
        (queue.phys as *mut u8).write_bytes(0, (queue.frames * 4096) as usize);
        (dma.phys as *mut u8).write_bytes(0, 8192);
        arch::outl(io + 8, (queue.phys >> 12) as u32);
    }
    let capacity_sectors = unsafe { read_port_u64(io + 20) };
    if capacity_sectors < 8 || capacity_sectors % 8 != 0 {
        let _ = memory::free(queue);
        let _ = memory::free(dma);
        return Err(BlockError::Device);
    }
    unsafe {
        arch::outb(
            io + 18,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK,
        )
    };
    DEVICE.install(Device {
        io,
        capacity_sectors,
        queue_size,
        queue,
        queue_bytes,
        dma,
        last_used: 0,
        flush_supported: guest_features & FEATURE_FLUSH != 0,
    })?;
    Ok(BlockInfo {
        blocks: capacity_sectors / 8,
        transport: "virtio-blk legacy PCI",
    })
}

pub fn info() -> Result<BlockInfo, BlockError> {
    DEVICE.with(|device| {
        Ok(BlockInfo {
            blocks: device.capacity_sectors / 8,
            transport: "virtio-blk legacy PCI",
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
        self.write_descriptor(
            0,
            Descriptor {
                address: self.dma.phys + 4096,
                length: 16,
                flags: DESC_NEXT,
                next: 1,
            },
        );
        self.write_descriptor(
            1,
            Descriptor {
                address: self.dma.phys,
                length: 4096,
                flags: DESC_NEXT | if device_writes { DESC_WRITE } else { 0 },
                next: 2,
            },
        );
        self.write_descriptor(
            2,
            Descriptor {
                address: self.dma.phys + 16 + 4096,
                length: 1,
                flags: DESC_WRITE,
                next: 0,
            },
        );
        self.submit(0)
    }

    fn flush_request(&mut self) -> Result<(), BlockError> {
        self.write_header(REQUEST_FLUSH, 0);
        self.write_descriptor(
            0,
            Descriptor {
                address: self.dma.phys + 4096,
                length: 16,
                flags: DESC_NEXT,
                next: 1,
            },
        );
        self.write_descriptor(
            1,
            Descriptor {
                address: self.dma.phys + 16 + 4096,
                length: 1,
                flags: DESC_WRITE,
                next: 0,
            },
        );
        self.submit(0)
    }

    fn write_header(&mut self, kind: u32, sector: u64) {
        let header = RequestHeader {
            kind,
            reserved: 0,
            sector,
        };
        unsafe {
            (self.dma.phys as *mut u8)
                .add(4096)
                .cast::<RequestHeader>()
                .write_volatile(header);
            (self.dma.phys as *mut u8).add(4112).write_volatile(0xff);
        }
    }

    fn write_descriptor(&self, index: usize, descriptor: Descriptor) {
        debug_assert!(index < self.queue_size as usize);
        unsafe {
            (self.queue.phys as *mut Descriptor)
                .add(index)
                .write_volatile(descriptor)
        };
    }

    fn submit(&mut self, head: u16) -> Result<(), BlockError> {
        let descriptor_bytes = u64::from(self.queue_size) * 16;
        let available = (self.queue.phys + descriptor_bytes) as *mut u8;
        let available_index = unsafe { available.add(2).cast::<u16>().read_volatile() };
        let ring_slot = usize::from(available_index % self.queue_size);
        unsafe {
            available
                .add(4 + ring_slot * 2)
                .cast::<u16>()
                .write_volatile(head)
        };
        fence(Ordering::Release);
        unsafe {
            available
                .add(2)
                .cast::<u16>()
                .write_volatile(available_index.wrapping_add(1))
        };
        unsafe { arch::outw(self.io + 16, QUEUE_SELECT) };

        let used_offset = align_up(descriptor_bytes + 6 + u64::from(self.queue_size) * 2, 4096);
        if used_offset + 6 > self.queue_bytes {
            return Err(BlockError::Device);
        }
        let used_index = (self.queue.phys + used_offset + 2) as *const u16;
        let wanted = self.last_used.wrapping_add(1);
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
        let status = unsafe { (self.dma.phys as *const u8).add(4112).read_volatile() };
        if status == 0 {
            Ok(())
        } else {
            Err(BlockError::Device)
        }
    }
}

fn find_device() -> Option<(u8, u16)> {
    let id = pci_read_u32(SYSTEM_DISK_SLOT, 0x00);
    if id as u16 != VIRTIO_VENDOR || (id >> 16) as u16 != VIRTIO_BLOCK_LEGACY {
        return None;
    }
    let bar = pci_read_u32(SYSTEM_DISK_SLOT, 0x10);
    let io = (bar & !3) as u16;
    (bar & 1 != 0 && io != 0).then_some((SYSTEM_DISK_SLOT, io))
}

fn enable_pci_device(slot: u8) {
    let command = pci_read_u32(slot, 0x04);
    pci_write_u32(slot, 0x04, command | 0x5);
}

fn pci_read_u32(slot: u8, offset: u8) -> u32 {
    let address = 0x8000_0000u32 | (u32::from(slot) << 11) | u32::from(offset & 0xfc);
    unsafe {
        arch::outl(0xcf8, address);
        arch::inl(0xcfc)
    }
}

fn pci_write_u32(slot: u8, offset: u8, value: u32) {
    let address = 0x8000_0000u32 | (u32::from(slot) << 11) | u32::from(offset & 0xfc);
    unsafe {
        arch::outl(0xcf8, address);
        arch::outl(0xcfc, value)
    };
}

unsafe fn read_port_u64(port: u16) -> u64 {
    let low = unsafe { arch::inl(port) };
    let high = unsafe { arch::inl(port + 4) };
    u64::from(low) | (u64::from(high) << 32)
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

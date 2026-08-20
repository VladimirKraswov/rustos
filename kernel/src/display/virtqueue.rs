//! Modern virtio-pci transport и одна split control queue.
//!
//! Очередь синхронная только на bootstrap-этапе: один producer CPU0, два
//! descriptor'а на команду (request + response), bounded polling timeout.
//! Формат очереди уже соответствует Virtio 1.x и позже может получить IRQ,
//! несколько in-flight commands и fences без изменения GPU protocol layer.

use core::{
    mem, ptr,
    sync::atomic::{fence, Ordering},
};

use crate::memory::{self, FrameBlock};

use super::pci::VirtioPciRegions;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;
const VIRTIO_F_VERSION_1_HIGH: u32 = 1;
const VIRTIO_GPU_F_EDID: u32 = 1 << 1;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const CONTROL_QUEUE: u16 = 0;
const MAX_QUEUE_SIZE: u16 = 64;
const POLL_LIMIT: usize = 50_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    Unsupported,
    InvalidConfiguration,
    OutOfMemory,
    RejectedFeatures,
    Timeout,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

pub struct ModernTransport {
    common: *mut u8,
    notify: *mut u8,
    device: *mut u8,
    device_length: u32,
    queue: FrameBlock,
    queue_size: u16,
    available_offset: u64,
    used_offset: u64,
    notify_offset: u64,
    dma: FrameBlock,
    last_used: u16,
    edid: bool,
}

impl ModernTransport {
    pub fn initialize(regions: VirtioPciRegions) -> Result<Self, TransportError> {
        if regions.common_length < 56
            || regions.device_length < 16
            || regions.notify_multiplier == 0
            || regions.isr == 0
        {
            return Err(TransportError::InvalidConfiguration);
        }
        let common = regions.common as *mut u8;
        unsafe { write_u8(common, 20, 0) };
        let mut reset = false;
        for _ in 0..100_000 {
            if unsafe { read_u8(common, 20) } == 0 {
                reset = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !reset {
            return Err(TransportError::InvalidConfiguration);
        }
        unsafe { write_u8(common, 20, STATUS_ACKNOWLEDGE | STATUS_DRIVER) };

        unsafe { write_u32(common, 0, 0) };
        let device_features_low = unsafe { read_u32(common, 4) };
        unsafe { write_u32(common, 0, 1) };
        let device_features_high = unsafe { read_u32(common, 4) };
        if device_features_high & VIRTIO_F_VERSION_1_HIGH == 0 {
            unsafe { write_u8(common, 20, STATUS_FAILED) };
            return Err(TransportError::Unsupported);
        }
        let accepted_low = device_features_low & VIRTIO_GPU_F_EDID;
        unsafe {
            write_u32(common, 8, 0);
            write_u32(common, 12, accepted_low);
            write_u32(common, 8, 1);
            write_u32(common, 12, VIRTIO_F_VERSION_1_HIGH);
            write_u8(
                common,
                20,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
            );
        }
        if unsafe { read_u8(common, 20) } & STATUS_FEATURES_OK == 0 {
            unsafe { write_u8(common, 20, STATUS_FAILED) };
            return Err(TransportError::RejectedFeatures);
        }

        unsafe { write_u16(common, 22, CONTROL_QUEUE) };
        let maximum = unsafe { read_u16(common, 24) };
        let queue_size = maximum.min(MAX_QUEUE_SIZE);
        if queue_size < 2 {
            unsafe { write_u8(common, 20, STATUS_FAILED) };
            return Err(TransportError::InvalidConfiguration);
        }
        let descriptor_bytes = u64::from(queue_size) * 16;
        let available_offset = descriptor_bytes;
        let available_bytes = 6 + u64::from(queue_size) * 2;
        let used_offset = align_up(available_offset + available_bytes, 4);
        let used_bytes = 6 + u64::from(queue_size) * 8;
        let queue_bytes = used_offset + used_bytes;
        let queue = memory::allocate(queue_bytes.div_ceil(4096), 1)
            .map_err(|_| TransportError::OutOfMemory)?;
        let dma = match memory::allocate(2, 1) {
            Ok(block) => block,
            Err(_) => {
                let _ = memory::free(queue);
                unsafe { write_u8(common, 20, STATUS_FAILED) };
                return Err(TransportError::OutOfMemory);
            }
        };
        unsafe {
            ptr::write_bytes(queue.phys as *mut u8, 0, (queue.frames * 4096) as usize);
            ptr::write_bytes(dma.phys as *mut u8, 0, 8192);
            write_u16(common, 24, queue_size);
            write_u64(common, 32, queue.phys);
            write_u64(common, 40, queue.phys + available_offset);
            write_u64(common, 48, queue.phys + used_offset);
        }
        let queue_notify_off = unsafe { read_u16(common, 30) };
        let notify_offset = u64::from(queue_notify_off)
            .checked_mul(u64::from(regions.notify_multiplier))
            .ok_or(TransportError::InvalidConfiguration)?;
        if notify_offset + 2 > u64::from(regions.notify_length) {
            let _ = memory::free(queue);
            let _ = memory::free(dma);
            unsafe { write_u8(common, 20, STATUS_FAILED) };
            return Err(TransportError::InvalidConfiguration);
        }
        unsafe {
            write_u16(common, 28, 1);
            write_u8(
                common,
                20,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
            );
        }
        Ok(Self {
            common,
            notify: regions.notify as *mut u8,
            device: regions.device as *mut u8,
            device_length: regions.device_length,
            queue,
            queue_size,
            available_offset,
            used_offset,
            notify_offset,
            dma,
            last_used: 0,
            edid: accepted_low & VIRTIO_GPU_F_EDID != 0,
        })
    }

    pub const fn edid_supported(&self) -> bool {
        self.edid
    }

    pub fn num_scanouts(&self) -> u32 {
        if self.device_length < 12 {
            return 1;
        }
        unsafe { read_u32(self.device, 8) }.clamp(1, 16)
    }

    pub fn command<Request: Copy, Response: Copy>(
        &mut self,
        request: &Request,
    ) -> Result<Response, TransportError> {
        let request_size = mem::size_of::<Request>();
        let response_size = mem::size_of::<Response>();
        if request_size == 0 || request_size > 4096 || response_size == 0 || response_size > 4096 {
            return Err(TransportError::InvalidConfiguration);
        }
        unsafe {
            ptr::write_bytes(self.dma.phys as *mut u8, 0, 8192);
            (self.dma.phys as *mut Request).write_volatile(*request);
            self.write_descriptor(
                0,
                Descriptor {
                    address: self.dma.phys,
                    length: request_size as u32,
                    flags: DESC_NEXT,
                    next: 1,
                },
            );
            self.write_descriptor(
                1,
                Descriptor {
                    address: self.dma.phys + 4096,
                    length: response_size as u32,
                    flags: DESC_WRITE,
                    next: 0,
                },
            );
        }
        self.submit(0)?;
        Ok(unsafe { ((self.dma.phys + 4096) as *const Response).read_volatile() })
    }

    unsafe fn write_descriptor(&self, index: usize, descriptor: Descriptor) {
        debug_assert!(index < self.queue_size as usize);
        unsafe {
            (self.queue.phys as *mut Descriptor)
                .add(index)
                .write_volatile(descriptor);
        }
    }

    fn submit(&mut self, head: u16) -> Result<(), TransportError> {
        let available = (self.queue.phys + self.available_offset) as *mut u8;
        let available_index = unsafe { available.add(2).cast::<u16>().read_volatile() };
        let slot = usize::from(available_index % self.queue_size);
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
                .write_volatile(available_index.wrapping_add(1));
            self.notify
                .add(self.notify_offset as usize)
                .cast::<u16>()
                .write_volatile(CONTROL_QUEUE);
        }
        let used_index = (self.queue.phys + self.used_offset + 2) as *const u16;
        let wanted = self.last_used.wrapping_add(1);
        for _ in 0..POLL_LIMIT {
            fence(Ordering::Acquire);
            if unsafe { used_index.read_volatile() } == wanted {
                self.last_used = wanted;
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(TransportError::Timeout)
    }
}

impl Drop for ModernTransport {
    fn drop(&mut self) {
        unsafe { write_u8(self.common, 20, 0) };
        let _ = memory::free(self.queue);
        let _ = memory::free(self.dma);
    }
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

unsafe fn read_u8(base: *mut u8, offset: usize) -> u8 {
    unsafe { base.add(offset).read_volatile() }
}

unsafe fn read_u16(base: *mut u8, offset: usize) -> u16 {
    unsafe { base.add(offset).cast::<u16>().read_volatile() }
}

unsafe fn read_u32(base: *mut u8, offset: usize) -> u32 {
    unsafe { base.add(offset).cast::<u32>().read_volatile() }
}

unsafe fn write_u8(base: *mut u8, offset: usize, value: u8) {
    unsafe { base.add(offset).write_volatile(value) };
}

unsafe fn write_u16(base: *mut u8, offset: usize, value: u16) {
    unsafe { base.add(offset).cast::<u16>().write_volatile(value) };
}

unsafe fn write_u32(base: *mut u8, offset: usize, value: u32) {
    unsafe { base.add(offset).cast::<u32>().write_volatile(value) };
}

unsafe fn write_u64(base: *mut u8, offset: usize, value: u64) {
    unsafe { base.add(offset).cast::<u64>().write_volatile(value) };
}

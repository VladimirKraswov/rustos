//! Modern virtio-mmio transport для GPU на AArch64 QEMU `virt`.
//!
//! Здесь находится только шина и split virtqueue. Команды display protocol
//! остаются в `virtio_gpu.rs` и тем самым одинаковы на AMD64/PCI и ARM/MMIO.
//! Драйвер пока синхронный: одна control queue, один запрос в полёте и
//! bounded polling. Это простой, проверяемый bootstrap до переноса `displayd`
//! в отдельный процесс и подключения IRQ/fence scheduler.

use core::{
    mem, ptr,
    sync::atomic::{fence, Ordering},
};

use crate::memory::{self, FrameBlock};

use super::TransportError;

const MMIO_FIRST: u64 = 0x0a00_0000;
const MMIO_STRIDE: u64 = 0x200;
const MMIO_SLOTS: u64 = 32;
const MAGIC: u32 = 0x7472_6976;
const VERSION_MODERN: u32 = 2;
const DEVICE_GPU: u32 = 16;

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
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_AVAIL_LOW: u64 = 0x090;
const REG_QUEUE_USED_LOW: u64 = 0x0a0;
const REG_CONFIG: u64 = 0x100;

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_FAILED: u32 = 128;
const VIRTIO_F_VERSION_1_HIGH: u32 = 1;
const VIRTIO_GPU_F_EDID: u32 = 1 << 1;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const CONTROL_QUEUE: u16 = 0;
const MAX_QUEUE_SIZE: u16 = 64;
const POLL_LIMIT: usize = 50_000_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

/// Очередь и DMA-память принадлежат одному GPU transport до его `Drop`.
pub struct ModernMmioTransport {
    base: u64,
    queue: FrameBlock,
    queue_size: u16,
    available_offset: u64,
    used_offset: u64,
    dma: FrameBlock,
    last_used: u16,
    edid: bool,
}

impl ModernMmioTransport {
    /// Находит device id 16 среди стандартных слотов QEMU `virt` и проводит
    /// обязательный Virtio 1.x feature/status handshake.
    pub fn initialize() -> Result<Self, TransportError> {
        let base = find_device().ok_or(TransportError::Unsupported)?;
        write32(base, REG_STATUS, 0);
        if read32(base, REG_STATUS) != 0 {
            return Err(TransportError::InvalidConfiguration);
        }
        write32(base, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

        write32(base, REG_DEVICE_FEATURES_SEL, 1);
        let high = read32(base, REG_DEVICE_FEATURES);
        if high & VIRTIO_F_VERSION_1_HIGH == 0 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return Err(TransportError::Unsupported);
        }
        write32(base, REG_DEVICE_FEATURES_SEL, 0);
        let low = read32(base, REG_DEVICE_FEATURES);
        let accepted_low = low & VIRTIO_GPU_F_EDID;
        write32(base, REG_DRIVER_FEATURES_SEL, 0);
        write32(base, REG_DRIVER_FEATURES, accepted_low);
        write32(base, REG_DRIVER_FEATURES_SEL, 1);
        write32(base, REG_DRIVER_FEATURES, VIRTIO_F_VERSION_1_HIGH);

        let negotiated = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
        write32(base, REG_STATUS, negotiated);
        if read32(base, REG_STATUS) & STATUS_FEATURES_OK == 0 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return Err(TransportError::RejectedFeatures);
        }

        write32(base, REG_QUEUE_SEL, u32::from(CONTROL_QUEUE));
        if read32(base, REG_QUEUE_READY) != 0 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return Err(TransportError::InvalidConfiguration);
        }
        let maximum = read32(base, REG_QUEUE_NUM_MAX).min(u32::from(MAX_QUEUE_SIZE));
        if maximum < 2 {
            write32(base, REG_STATUS, STATUS_FAILED);
            return Err(TransportError::InvalidConfiguration);
        }
        let queue_size = maximum as u16;
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
                write32(base, REG_STATUS, STATUS_FAILED);
                return Err(TransportError::OutOfMemory);
            }
        };
        unsafe {
            ptr::write_bytes(queue.phys as *mut u8, 0, (queue.frames * 4096) as usize);
            ptr::write_bytes(dma.phys as *mut u8, 0, 8192);
        }
        write32(base, REG_QUEUE_NUM, u32::from(queue_size));
        write_address(base, REG_QUEUE_DESC_LOW, queue.phys);
        write_address(base, REG_QUEUE_AVAIL_LOW, queue.phys + available_offset);
        write_address(base, REG_QUEUE_USED_LOW, queue.phys + used_offset);
        write32(base, REG_QUEUE_READY, 1);
        write32(base, REG_STATUS, negotiated | STATUS_DRIVER_OK);

        Ok(Self {
            base,
            queue,
            queue_size,
            available_offset,
            used_offset,
            dma,
            last_used: 0,
            edid: accepted_low & VIRTIO_GPU_F_EDID != 0,
        })
    }

    pub const fn edid_supported(&self) -> bool {
        self.edid
    }

    pub fn num_scanouts(&self) -> u32 {
        read32(self.base, REG_CONFIG + 8).clamp(1, 16)
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
        }
        write32(self.base, REG_QUEUE_NOTIFY, u32::from(CONTROL_QUEUE));

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

impl Drop for ModernMmioTransport {
    fn drop(&mut self) {
        write32(self.base, REG_STATUS, 0);
        let _ = memory::free(self.queue);
        let _ = memory::free(self.dma);
    }
}

fn find_device() -> Option<u64> {
    (0..MMIO_SLOTS)
        .map(|slot| MMIO_FIRST + slot * MMIO_STRIDE)
        .find(|base| {
            read32(*base, REG_MAGIC) == MAGIC
                && read32(*base, REG_VERSION) == VERSION_MODERN
                && read32(*base, REG_DEVICE_ID) == DEVICE_GPU
        })
}

fn read32(base: u64, offset: u64) -> u32 {
    unsafe { ((base + offset) as *const u32).read_volatile() }
}

fn write32(base: u64, offset: u64, value: u32) {
    unsafe { ((base + offset) as *mut u32).write_volatile(value) }
}

fn write_address(base: u64, low_register: u64, address: u64) {
    write32(base, low_register, address as u32);
    write32(base, low_register + 4, (address >> 32) as u32);
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

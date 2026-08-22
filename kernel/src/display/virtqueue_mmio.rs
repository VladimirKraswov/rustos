//! Modern virtio-mmio transport для GPU на AArch64 QEMU `virt`.
//!
//! Здесь находится только шина и split virtqueue. Команды display protocol
//! остаются в `virtio_gpu.rs` и тем самым одинаковы на AMD64/PCI и ARM/MMIO.
//! Одна control queue содержит несколько независимых command slots.
//! Bootstrap использует synchronous wrapper, а ring-3 renderer получает
//! немедленный возврат и completion через периодический bounded poll.

use core::{mem, ptr};

use crate::{
    arch,
    memory::{self, FrameBlock},
};

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
const STATUS_DEVICE_NEEDS_RESET: u32 = 64;
const STATUS_FAILED: u32 = 128;
const VIRTIO_F_VERSION_1_HIGH: u32 = 1;
const VIRTIO_GPU_F_VIRGL: u32 = 1 << 0;
const VIRTIO_GPU_F_EDID: u32 = 1 << 1;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const CONTROL_QUEUE: u16 = 0;
const CURSOR_QUEUE: u16 = 1;
const MAX_QUEUE_SIZE: u16 = 64;
const POLL_LIMIT: usize = 50_000_000;
// Совпадает с PCI transport: три render кадра не должны занимать последний
// slot, нужный display/control command во время их выполнения.
const COMMAND_SLOTS: usize = 8;
/// Cursor move/update не имеют response descriptor. Четыре request slot дают
/// mouse producer'у опубликовать новую позицию, пока device завершает старую.
const CURSOR_SLOTS: usize = 4;
const QEMU_VIRT_SPI_BASE: u32 = 32 + 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AsyncCompletion {
    pub fence_id: u64,
    pub response_kind: u32,
}

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
struct UsedElement {
    id: u32,
    length: u32,
}

#[derive(Clone, Copy)]
struct CommandSlot {
    generation: u16,
    state: u8,
}

impl CommandSlot {
    const FREE: Self = Self {
        generation: 1,
        state: 0,
    };
}

struct CursorQueue {
    queue: FrameBlock,
    queue_size: u16,
    available_offset: u64,
    used_offset: u64,
    dma: FrameBlock,
    slot_count: usize,
    busy: [bool; CURSOR_SLOTS],
    device_used: u16,
}

/// Очередь и DMA-память принадлежат одному GPU transport до его `Drop`.
pub struct ModernMmioTransport {
    base: u64,
    interrupt: u32,
    queue: FrameBlock,
    queue_size: u16,
    available_offset: u64,
    used_offset: u64,
    dma: FrameBlock,
    slot_count: usize,
    slots: [CommandSlot; COMMAND_SLOTS],
    device_used: u16,
    edid: bool,
    virgl: bool,
    cursor: Option<CursorQueue>,
}

impl ModernMmioTransport {
    /// Находит device id 16 среди стандартных слотов QEMU `virt` и проводит
    /// обязательный Virtio 1.x feature/status handshake.
    pub fn initialize() -> Result<Self, TransportError> {
        let (base, slot) = find_device().ok_or(TransportError::Unsupported)?;
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
        let accepted_low = low & (VIRTIO_GPU_F_EDID | VIRTIO_GPU_F_VIRGL);
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
        let slot_count = (usize::from(queue_size) / 2).min(COMMAND_SLOTS);
        let dma = match memory::allocate((slot_count * 2) as u64, 1) {
            Ok(block) => block,
            Err(_) => {
                let _ = memory::free(queue);
                write32(base, REG_STATUS, STATUS_FAILED);
                return Err(TransportError::OutOfMemory);
            }
        };
        unsafe {
            ptr::write_bytes(queue.phys as *mut u8, 0, (queue.frames * 4096) as usize);
            ptr::write_bytes(dma.phys as *mut u8, 0, slot_count * 8192);
        }
        write32(base, REG_QUEUE_NUM, u32::from(queue_size));
        write_address(base, REG_QUEUE_DESC_LOW, queue.phys);
        write_address(base, REG_QUEUE_AVAIL_LOW, queue.phys + available_offset);
        write_address(base, REG_QUEUE_USED_LOW, queue.phys + used_offset);
        write32(base, REG_QUEUE_READY, 1);
        // Cursor queue является независимым fast path. Отсутствие queue 1 или
        // локальная нехватка RAM не ломают display: compositor сохранит
        // software cursor и controlq продолжит работу.
        let cursor = initialize_cursor_queue(base).ok();
        write32(base, REG_STATUS, negotiated | STATUS_DRIVER_OK);

        Ok(Self {
            base,
            interrupt: QEMU_VIRT_SPI_BASE + slot,
            queue,
            queue_size,
            available_offset,
            used_offset,
            dma,
            slot_count,
            slots: [CommandSlot::FREE; COMMAND_SLOTS],
            device_used: 0,
            edid: accepted_low & VIRTIO_GPU_F_EDID != 0,
            virgl: accepted_low & VIRTIO_GPU_F_VIRGL != 0,
            cursor,
        })
    }

    pub const fn edid_supported(&self) -> bool {
        self.edid
    }

    pub const fn virgl_supported(&self) -> bool {
        self.virgl
    }

    pub const fn cursor_supported(&self) -> bool {
        self.cursor.is_some()
    }

    /// GIC INTID QEMU `virt`: SPI index 16+slot превращается в INTID 48+slot.
    pub const fn interrupt_id(&self) -> u32 {
        self.interrupt
    }

    /// Снимает edge-latch virtio-mmio. Used ring читает completion path;
    /// DMA barrier остаётся сосредоточен в `poll_used`.
    pub fn acknowledge_interrupt(&mut self) -> Result<bool, TransportError> {
        self.ensure_device_ready()?;
        let status = read32(self.base, REG_INTERRUPT_STATUS);
        if status & 1 != 0 {
            if let Some(cursor) = self.cursor.as_mut() {
                cursor.poll_used()?;
            }
        }
        if status != 0 {
            write32(self.base, REG_INTERRUPT_ACK, status);
        }
        Ok(status & 1 != 0)
    }

    /// Публикует UPDATE_CURSOR/MOVE_CURSOR без ожидания device. Если четыре
    /// старые позиции ещё in-flight, возвращает Busy: caller отбрасывает
    /// устаревшую позицию и следующая mouse event принесёт актуальную.
    pub fn submit_cursor<Request: Copy>(
        &mut self,
        request: &Request,
    ) -> Result<(), TransportError> {
        self.ensure_device_ready()?;
        let request_size = mem::size_of::<Request>();
        if request_size == 0 || request_size > 4096 {
            return Err(TransportError::InvalidConfiguration);
        }
        let base = self.base;
        let cursor = self.cursor.as_mut().ok_or(TransportError::Unsupported)?;
        cursor.poll_used()?;
        let index = cursor
            .busy
            .iter()
            .take(cursor.slot_count)
            .position(|busy| !*busy)
            .ok_or(TransportError::Busy)?;
        let request_address = cursor.dma.phys + index as u64 * 4096;
        unsafe {
            ptr::write_bytes(request_address as *mut u8, 0, 4096);
            ptr::copy_nonoverlapping(
                (request as *const Request).cast::<u8>(),
                request_address as *mut u8,
                request_size,
            );
            (cursor.queue.phys as *mut Descriptor)
                .add(index)
                .write_volatile(Descriptor {
                    address: request_address,
                    length: request_size as u32,
                    flags: 0,
                    next: 0,
                });
        }
        let available = (cursor.queue.phys + cursor.available_offset) as *mut u8;
        let available_index = unsafe { available.add(2).cast::<u16>().read_volatile() };
        let ring_slot = usize::from(available_index % cursor.queue_size);
        unsafe {
            available
                .add(4 + ring_slot * 2)
                .cast::<u16>()
                .write_volatile(index as u16);
        }
        arch::dma_write_barrier();
        unsafe {
            available
                .add(2)
                .cast::<u16>()
                .write_volatile(available_index.wrapping_add(1));
        }
        cursor.busy[index] = true;
        arch::dma_write_barrier();
        write32(base, REG_QUEUE_NOTIFY, u32::from(CURSOR_QUEUE));
        Ok(())
    }

    pub fn num_capsets(&self) -> u32 {
        read32(self.base, REG_CONFIG + 12)
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
        let request_bytes = unsafe {
            core::slice::from_raw_parts((request as *const Request).cast::<u8>(), request_size)
        };
        let token = self.submit_bytes(request_bytes, &[], response_size)?;
        for _ in 0..POLL_LIMIT {
            self.poll_used()?;
            let index = token_index(token);
            if self.slots[index].state == 2
                && self.slots[index].generation == token_generation(token)
            {
                let response =
                    unsafe { (self.response_address(index) as *const Response).read_volatile() };
                self.release_slot(index);
                return Ok(response);
            }
            core::hint::spin_loop();
        }
        // Не освобождаем descriptor после timeout: device может завершить DMA
        // позднее. Весь transport помечается FAILED и больше не принимает
        // команды до контролируемого reset/reprobe.
        self.fail_device();
        Err(TransportError::Timeout)
    }

    pub fn submit_bytes(
        &mut self,
        prefix: &[u8],
        payload: &[u8],
        response_size: usize,
    ) -> Result<u32, TransportError> {
        let request_size = prefix
            .len()
            .checked_add(payload.len())
            .ok_or(TransportError::InvalidConfiguration)?;
        if request_size == 0 || request_size > 4096 || !(24..=4096).contains(&response_size) {
            return Err(TransportError::InvalidConfiguration);
        }
        self.poll_used()?;
        let index = self.slots[..self.slot_count]
            .iter()
            .position(|slot| slot.state == 0)
            .ok_or(TransportError::Busy)?;
        let request_address = self.request_address(index);
        let response_address = self.response_address(index);
        unsafe {
            ptr::write_bytes(request_address as *mut u8, 0, 4096);
            ptr::write_bytes(response_address as *mut u8, 0, 4096);
            ptr::copy_nonoverlapping(prefix.as_ptr(), request_address as *mut u8, prefix.len());
            ptr::copy_nonoverlapping(
                payload.as_ptr(),
                (request_address as *mut u8).add(prefix.len()),
                payload.len(),
            );
            let head = index * 2;
            self.write_descriptor(
                head,
                Descriptor {
                    address: request_address,
                    length: request_size as u32,
                    flags: DESC_NEXT,
                    next: (head + 1) as u16,
                },
            );
            self.write_descriptor(
                head + 1,
                Descriptor {
                    address: response_address,
                    length: response_size as u32,
                    flags: DESC_WRITE,
                    next: 0,
                },
            );
        }
        let generation = next_generation(self.slots[index].generation);
        self.slots[index] = CommandSlot {
            generation,
            state: 1,
        };
        self.publish((index * 2) as u16);
        Ok(make_token(index, generation))
    }

    pub fn poll_completion(&mut self) -> Result<Option<AsyncCompletion>, TransportError> {
        self.poll_used()?;
        let Some(index) = self.slots[..self.slot_count]
            .iter()
            .position(|slot| slot.state == 2)
        else {
            return Ok(None);
        };
        let response = self.response_address(index);
        let response_kind = unsafe { (response as *const u32).read_volatile() };
        let fence_id = unsafe { ((response + 8) as *const u64).read_volatile() };
        self.release_slot(index);
        Ok(Some(AsyncCompletion {
            fence_id,
            response_kind,
        }))
    }

    unsafe fn write_descriptor(&self, index: usize, descriptor: Descriptor) {
        debug_assert!(index < self.queue_size as usize);
        unsafe {
            (self.queue.phys as *mut Descriptor)
                .add(index)
                .write_volatile(descriptor);
        }
    }

    fn publish(&mut self, head: u16) {
        let available = (self.queue.phys + self.available_offset) as *mut u8;
        let available_index = unsafe { available.add(2).cast::<u16>().read_volatile() };
        let slot = usize::from(available_index % self.queue_size);
        unsafe {
            available
                .add(4 + slot * 2)
                .cast::<u16>()
                .write_volatile(head);
        }
        arch::dma_write_barrier();
        unsafe {
            available
                .add(2)
                .cast::<u16>()
                .write_volatile(available_index.wrapping_add(1));
        }
        arch::dma_write_barrier();
        write32(self.base, REG_QUEUE_NOTIFY, u32::from(CONTROL_QUEUE));
    }

    fn poll_used(&mut self) -> Result<(), TransportError> {
        self.ensure_device_ready()?;
        let used = (self.queue.phys + self.used_offset) as *const u8;
        let available = unsafe { used.add(2).cast::<u16>().read_volatile() };
        // `used.idx` является publication point устройства: содержимое ring и
        // response buffer читается только после acquire barrier.
        arch::dma_read_barrier();
        while self.device_used != available {
            let ring_index = usize::from(self.device_used % self.queue_size);
            let element = unsafe {
                used.add(4 + ring_index * core::mem::size_of::<UsedElement>())
                    .cast::<UsedElement>()
                    .read_volatile()
            };
            let head = usize::try_from(element.id).map_err(|_| TransportError::DeviceError)?;
            if !head.is_multiple_of(2) || head / 2 >= self.slot_count {
                return Err(TransportError::DeviceError);
            }
            let slot = &mut self.slots[head / 2];
            if slot.state != 1 {
                return Err(TransportError::DeviceError);
            }
            slot.state = 2;
            self.device_used = self.device_used.wrapping_add(1);
        }
        Ok(())
    }

    fn ensure_device_ready(&self) -> Result<(), TransportError> {
        let status = read32(self.base, REG_STATUS);
        if status & STATUS_DRIVER_OK == 0
            || status & (STATUS_DEVICE_NEEDS_RESET | STATUS_FAILED) != 0
        {
            return Err(TransportError::DeviceError);
        }
        Ok(())
    }

    fn fail_device(&self) {
        let status = read32(self.base, REG_STATUS);
        write32(self.base, REG_STATUS, status | STATUS_FAILED);
    }

    fn request_address(&self, index: usize) -> u64 {
        self.dma.phys + index as u64 * 8192
    }

    fn response_address(&self, index: usize) -> u64 {
        self.request_address(index) + 4096
    }

    fn release_slot(&mut self, index: usize) {
        self.slots[index].state = 0;
    }
}

impl Drop for ModernMmioTransport {
    fn drop(&mut self) {
        write32(self.base, REG_STATUS, 0);
        if let Some(cursor) = self.cursor.take() {
            let _ = memory::free(cursor.queue);
            let _ = memory::free(cursor.dma);
        }
        let _ = memory::free(self.queue);
        let _ = memory::free(self.dma);
    }
}

impl CursorQueue {
    fn poll_used(&mut self) -> Result<(), TransportError> {
        let used = (self.queue.phys + self.used_offset) as *const u8;
        let available = unsafe { used.add(2).cast::<u16>().read_volatile() };
        arch::dma_read_barrier();
        while self.device_used != available {
            let ring_index = usize::from(self.device_used % self.queue_size);
            let element = unsafe {
                used.add(4 + ring_index * core::mem::size_of::<UsedElement>())
                    .cast::<UsedElement>()
                    .read_volatile()
            };
            let slot = usize::try_from(element.id).map_err(|_| TransportError::DeviceError)?;
            if slot >= self.slot_count || !self.busy[slot] {
                return Err(TransportError::DeviceError);
            }
            self.busy[slot] = false;
            self.device_used = self.device_used.wrapping_add(1);
        }
        Ok(())
    }
}

fn initialize_cursor_queue(base: u64) -> Result<CursorQueue, TransportError> {
    write32(base, REG_QUEUE_SEL, u32::from(CURSOR_QUEUE));
    if read32(base, REG_QUEUE_READY) != 0 {
        return Err(TransportError::InvalidConfiguration);
    }
    let maximum = read32(base, REG_QUEUE_NUM_MAX).min(CURSOR_SLOTS as u32);
    if maximum < 2 {
        return Err(TransportError::Unsupported);
    }
    let queue_size = maximum as u16;
    let descriptor_bytes = u64::from(queue_size) * 16;
    let available_offset = descriptor_bytes;
    let available_bytes = 6 + u64::from(queue_size) * 2;
    let used_offset = align_up(available_offset + available_bytes, 4);
    let used_bytes = 6 + u64::from(queue_size) * 8;
    let queue_bytes = used_offset + used_bytes;
    let queue =
        memory::allocate(queue_bytes.div_ceil(4096), 1).map_err(|_| TransportError::OutOfMemory)?;
    let slot_count = usize::from(queue_size).min(CURSOR_SLOTS);
    let dma = match memory::allocate(slot_count as u64, 1) {
        Ok(block) => block,
        Err(_) => {
            let _ = memory::free(queue);
            return Err(TransportError::OutOfMemory);
        }
    };
    unsafe {
        ptr::write_bytes(queue.phys as *mut u8, 0, (queue.frames * 4096) as usize);
        ptr::write_bytes(dma.phys as *mut u8, 0, slot_count * 4096);
    }
    write32(base, REG_QUEUE_NUM, u32::from(queue_size));
    write_address(base, REG_QUEUE_DESC_LOW, queue.phys);
    write_address(base, REG_QUEUE_AVAIL_LOW, queue.phys + available_offset);
    write_address(base, REG_QUEUE_USED_LOW, queue.phys + used_offset);
    write32(base, REG_QUEUE_READY, 1);
    Ok(CursorQueue {
        queue,
        queue_size,
        available_offset,
        used_offset,
        dma,
        slot_count,
        busy: [false; CURSOR_SLOTS],
        device_used: 0,
    })
}

fn find_device() -> Option<(u64, u32)> {
    (0..MMIO_SLOTS)
        .map(|slot| (MMIO_FIRST + slot * MMIO_STRIDE, slot as u32))
        .find(|(base, _)| {
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

const fn make_token(index: usize, generation: u16) -> u32 {
    ((generation as u32) << 8) | index as u32
}

const fn token_index(token: u32) -> usize {
    (token & 0xff) as usize
}

const fn token_generation(token: u32) -> u16 {
    (token >> 8) as u16
}

const fn next_generation(generation: u16) -> u16 {
    let next = generation.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

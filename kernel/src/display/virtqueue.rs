//! Modern virtio-pci transport и асинхронная split control queue.
//!
//! Bootstrap-команды всё ещё имеют удобную синхронную обёртку, но сама
//! очередь поддерживает несколько независимых descriptor chains. Ring-3
//! render submission только публикует chain; timer tick забирает used entry
//! и fence без многомиллионного busy-spin.

use core::{mem, ptr};

use crate::{
    arch,
    memory::{self, FrameBlock},
};

use super::{pci::VirtioPciRegions, TransportError};

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_DEVICE_NEEDS_RESET: u8 = 64;
const STATUS_FAILED: u8 = 128;
const VIRTIO_F_VERSION_1_HIGH: u32 = 1;
const VIRTIO_GPU_F_VIRGL: u32 = 1 << 0;
const VIRTIO_GPU_F_EDID: u32 = 1 << 1;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const CONTROL_QUEUE: u16 = 0;
const MAX_QUEUE_SIZE: u16 = 64;
const POLL_LIMIT: usize = 50_000_000;
// Девять команд трёх 2D presents и три независимых render submission должны
// одновременно помещаться в controlq. Остаток нужен для bounded bootstrap/
// teardown command; каждый slot владеет отдельными request/response DMA pages.
const COMMAND_SLOTS: usize = 16;

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
    slot_count: usize,
    slots: [CommandSlot; COMMAND_SLOTS],
    device_used: u16,
    edid: bool,
    virgl: bool,
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
        let accepted_low = device_features_low & (VIRTIO_GPU_F_EDID | VIRTIO_GPU_F_VIRGL);
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
        let slot_count = (usize::from(queue_size) / 2).min(COMMAND_SLOTS);
        let dma = match memory::allocate((slot_count * 2) as u64, 1) {
            Ok(block) => block,
            Err(_) => {
                let _ = memory::free(queue);
                unsafe { write_u8(common, 20, STATUS_FAILED) };
                return Err(TransportError::OutOfMemory);
            }
        };
        unsafe {
            ptr::write_bytes(queue.phys as *mut u8, 0, (queue.frames * 4096) as usize);
            ptr::write_bytes(dma.phys as *mut u8, 0, slot_count * 8192);
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
            slot_count,
            slots: [CommandSlot::FREE; COMMAND_SLOTS],
            device_used: 0,
            edid: accepted_low & VIRTIO_GPU_F_EDID != 0,
            virgl: accepted_low & VIRTIO_GPU_F_VIRGL != 0,
        })
    }

    pub const fn edid_supported(&self) -> bool {
        self.edid
    }

    pub const fn virgl_supported(&self) -> bool {
        self.virgl
    }

    /// PCI cursorq будет включена вместе с MSI-X interrupt-domain. До этого
    /// x86 compositor использует тот же корректный software fallback.
    pub const fn cursor_supported(&self) -> bool {
        false
    }

    pub fn num_capsets(&self) -> u32 {
        if self.device_length < 16 {
            return 0;
        }
        unsafe { read_u32(self.device, 12) }
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
        // Descriptor нельзя просто вернуть в FREE: устройство ещё может
        // завершить DMA и опубликовать stale used entry. По Virtio 1.x
        // timeout переводит весь transport в FAILED: верхний уровень получает
        // DeviceLost вместо повторного использования потенциально
        // повреждённой очереди.
        self.fail_device();
        Err(TransportError::Timeout)
    }

    /// Публикует command без ожидания device. Prefix и payload копируются в
    /// отдельную DMA page, поэтому возврат в ring 3 не создаёт TOCTOU.
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

    /// Забирает одно завершение. Вызов неблокирующий и подходит для timer/IRQ
    /// bottom half; отсутствие used entry — нормальный `Ok(None)`.
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
            arch::dma_write_barrier();
            self.notify
                .add(self.notify_offset as usize)
                .cast::<u16>()
                .write_volatile(CONTROL_QUEUE);
        }
    }

    fn poll_used(&mut self) -> Result<(), TransportError> {
        self.ensure_device_ready()?;
        let used = (self.queue.phys + self.used_offset) as *const u8;
        let available = unsafe { used.add(2).cast::<u16>().read_volatile() };
        // Сначала наблюдаем новый used.idx, затем запрещаем процессору читать
        // элементы кольца и response page раньше публикации этого индекса
        // устройством. Это обычная acquire-сторона протокола Virtqueue.
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
        let status = unsafe { read_u8(self.common, 20) };
        if status & STATUS_DRIVER_OK == 0
            || status & (STATUS_DEVICE_NEEDS_RESET | STATUS_FAILED) != 0
        {
            return Err(TransportError::DeviceError);
        }
        Ok(())
    }

    fn fail_device(&self) {
        let status = unsafe { read_u8(self.common, 20) };
        unsafe { write_u8(self.common, 20, status | STATUS_FAILED) };
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

impl Drop for ModernTransport {
    fn drop(&mut self) {
        unsafe { write_u8(self.common, 20, 0) };
        let _ = memory::free(self.queue);
        let _ = memory::free(self.dma);
    }
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

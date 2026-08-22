//! Modern virtio-pci block transport для AArch64 QEMU `virt`/UTM.
//!
//! UTM оформляет внутренние raw images как PCI-функции, чтобы sandboxed QEMU
//! получал к ним доступ через штатный drive API. Этот bootstrap-драйвер не
//! знает о файловой системе: он перечисляет bus 0, выбирает самый вместительный
//! virtio-blk volume и предоставляет те же bounded 4-KiB операции, что MMIO.
//! Маленький UEFI ESP поэтому не может случайно стать persistent system disk.

use core::{
    cell::UnsafeCell,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{BlockError, BlockInfo};
use crate::{
    arch,
    memory::{self, FrameBlock},
    serial,
};

const QEMU_VIRT_ECAM_BASES: [u64; 2] = [0x3f00_0000, 0x0040_1000_0000];
const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_BLOCK_TRANSITIONAL: u16 = 0x1001;
const VIRTIO_BLOCK_MODERN: u16 = 0x1042;
const PCI_STATUS_CAPABILITIES: u16 = 1 << 4;
const PCI_CAP_VENDOR: u8 = 0x09;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;
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

#[derive(Clone, Copy)]
struct Function {
    ecam: u64,
    slot: u8,
    function: u8,
}

#[derive(Clone, Copy)]
struct Regions {
    function: Function,
    common: u64,
    common_length: u32,
    notify: u64,
    notify_length: u32,
    notify_multiplier: u32,
    isr: u64,
    device: u64,
    device_length: u32,
}

struct Device {
    notify: u64,
    notify_offset: u64,
    isr: u64,
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

// `value` читается и меняется только под spinlock. Сейчас bootstrap I/O
// синхронный, но правило остаётся корректным после запуска дополнительных CPU.
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
    let regions = discover_system_volume().ok_or(BlockError::Unsupported)?;
    if regions.common_length < 56
        || regions.notify_length < 2
        || regions.device_length < 8
        || regions.notify_multiplier == 0
    {
        return Err(BlockError::Device);
    }

    // Memory space + bus mastering. Верхняя половина status содержит W1C
    // флаги, поэтому обратно записываются только command bits.
    let command = regions.function.read_u16(0x04);
    regions.function.write_u16(0x04, command | 0x0006);

    write_u8(regions.common + 20, 0);
    let mut reset = false;
    for _ in 0..100_000 {
        if read_u8(regions.common + 20) == 0 {
            reset = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !reset {
        return Err(BlockError::Device);
    }
    write_u8(regions.common + 20, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    write_u32(regions.common, 0);
    let features_low = read_u32(regions.common + 4);
    write_u32(regions.common, 1);
    if read_u32(regions.common + 4) & FEATURE_VERSION_1_WORD1 == 0 {
        write_u8(regions.common + 20, STATUS_FAILED);
        return Err(BlockError::Unsupported);
    }
    let accepted_low = features_low & FEATURE_FLUSH;
    write_u32(regions.common + 8, 0);
    write_u32(regions.common + 12, accepted_low);
    write_u32(regions.common + 8, 1);
    write_u32(regions.common + 12, FEATURE_VERSION_1_WORD1);

    let negotiated = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
    write_u8(regions.common + 20, negotiated);
    if read_u8(regions.common + 20) & STATUS_FEATURES_OK == 0 {
        write_u8(regions.common + 20, STATUS_FAILED);
        return Err(BlockError::Device);
    }

    write_u16(regions.common + 22, 0);
    let maximum = read_u16(regions.common + 24);
    if maximum < 3 || read_u16(regions.common + 28) != 0 {
        write_u8(regions.common + 20, STATUS_FAILED);
        return Err(BlockError::Device);
    }
    let queue_size = maximum.min(QUEUE_SIZE);
    let queue = memory::allocate(3, 1).map_err(|_| BlockError::OutOfMemory)?;
    let dma = match memory::allocate(2, 1) {
        Ok(block) => block,
        Err(_) => {
            let _ = memory::free(queue);
            return Err(BlockError::OutOfMemory);
        }
    };
    unsafe {
        ptr::write_bytes(queue.phys as *mut u8, 0, 3 * 4096);
        ptr::write_bytes(dma.phys as *mut u8, 0, 2 * 4096);
    }
    write_u16(regions.common + 24, queue_size);
    write_u64(regions.common + 32, queue.phys);
    write_u64(regions.common + 40, queue.phys + 4096);
    write_u64(regions.common + 48, queue.phys + 8192);
    let queue_notify_offset = read_u16(regions.common + 30);
    let notify_offset = u64::from(queue_notify_offset)
        .checked_mul(u64::from(regions.notify_multiplier))
        .ok_or(BlockError::Device)?;
    if notify_offset + 2 > u64::from(regions.notify_length) {
        let _ = memory::free(queue);
        let _ = memory::free(dma);
        write_u8(regions.common + 20, STATUS_FAILED);
        return Err(BlockError::Device);
    }
    write_u16(regions.common + 28, 1);

    let capacity_sectors = read_u64_stable(regions.device);
    if capacity_sectors < 8 || !capacity_sectors.is_multiple_of(8) {
        let _ = memory::free(queue);
        let _ = memory::free(dma);
        write_u8(regions.common + 20, STATUS_FAILED);
        return Err(BlockError::Device);
    }
    write_u8(regions.common + 20, negotiated | STATUS_DRIVER_OK);

    DEVICE.install(Device {
        notify: regions.notify,
        notify_offset,
        isr: regions.isr,
        capacity_sectors,
        queue_size,
        queue,
        dma,
        last_used: 0,
        flush_supported: accepted_low & FEATURE_FLUSH != 0,
    })?;
    Ok(BlockInfo {
        blocks: capacity_sectors / 8,
        transport: "virtio-blk modern PCI",
    })
}

pub fn info() -> Result<BlockInfo, BlockError> {
    DEVICE.with(|device| {
        Ok(BlockInfo {
            blocks: device.capacity_sectors / 8,
            transport: "virtio-blk modern PCI",
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
        arch::dma_write_barrier();
        unsafe {
            available
                .add(2)
                .cast::<u16>()
                .write_volatile(index.wrapping_add(1));
        }
        arch::dma_write_barrier();
        write_u16(self.notify + self.notify_offset, 0);

        let wanted = self.last_used.wrapping_add(1);
        let used_index = (self.queue.phys + 8192 + 2) as *const u16;
        let mut completed = false;
        for _ in 0..POLL_LIMIT {
            if unsafe { used_index.read_volatile() } == wanted {
                completed = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !completed {
            return Err(BlockError::Timeout);
        }
        arch::dma_read_barrier();
        self.last_used = wanted;
        // Чтение ISR status снимает edge-latch даже при polling transport.
        let _ = read_u8(self.isr);
        let status = unsafe { (self.dma.phys as *const u8).add(4112).read_volatile() };
        (status == 0).then_some(()).ok_or(BlockError::Device)
    }
}

fn discover_system_volume() -> Option<Regions> {
    let mut selected: Option<(u64, Regions)> = None;
    'windows: for ecam in QEMU_VIRT_ECAM_BASES {
        let mut window_present = false;
        for slot in 0..32 {
            let first = Function {
                ecam,
                slot,
                function: 0,
            };
            if first.read_u16(0) == u16::MAX {
                continue;
            }
            window_present = true;
            let functions = if first.read_u8(0x0e) & 0x80 != 0 {
                8
            } else {
                1
            };
            for function in 0..functions {
                let pci_function = Function {
                    ecam,
                    slot,
                    function,
                };
                let id = pci_function.read_u32(0);
                let device = (id >> 16) as u16;
                if id as u16 != VIRTIO_VENDOR
                    || (device != VIRTIO_BLOCK_TRANSITIONAL && device != VIRTIO_BLOCK_MODERN)
                {
                    continue;
                }
                let Some(regions) = parse_capabilities(pci_function) else {
                    continue;
                };
                // UEFI вправе погасить PCI command register при завершении.
                // Device-specific BAR нельзя читать до Memory Space Enable.
                let command = pci_function.read_u16(0x04);
                pci_function.write_u16(0x04, command | 0x0006);
                let capacity = read_u64_stable(regions.device);
                if capacity >= 8
                    && capacity != u64::MAX
                    && selected
                        .as_ref()
                        .is_none_or(|(current, _)| capacity > *current)
                {
                    selected = Some((capacity, regions));
                }
            }
        }
        // Compact и high ECAM — взаимоисключающие host windows. Не читаем
        // физический hole второго варианта: на реальном ARM он может дать
        // synchronous external abort вместо PCI-compliant all-ones.
        if window_present {
            break 'windows;
        }
    }
    selected.map(|(capacity, regions)| {
        serial::put_str("[block-pci] selected ecam=");
        serial::put_hex(regions.function.ecam);
        serial::put_str(" slot=");
        serial::put_hex(u64::from(regions.function.slot));
        serial::put_str(" sectors=");
        serial::put_hex(capacity);
        serial::put_str("\n");
        regions
    })
}

fn parse_capabilities(function: Function) -> Option<Regions> {
    if function.read_u16(0x06) & PCI_STATUS_CAPABILITIES == 0 {
        return None;
    }
    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = 0;
    let mut isr = None;
    let mut device = None;
    let mut cursor = function.read_u8(0x34) & !3;
    for _ in 0..48 {
        if !(0x40..=0xf0).contains(&cursor) || cursor & 3 != 0 {
            break;
        }
        let id = function.read_u8(cursor);
        let next = function.read_u8(cursor + 1) & !3;
        let length = function.read_u8(cursor + 2);
        if u16::from(cursor) + u16::from(length) > 256 {
            break;
        }
        if id == PCI_CAP_VENDOR && length >= 16 {
            let config_type = function.read_u8(cursor + 3);
            let bar_index = function.read_u8(cursor + 4);
            let offset = function.read_u32(cursor + 8);
            let region_length = function.read_u32(cursor + 12);
            if let Some(address) = function
                .bar(bar_index)
                .and_then(|bar| bar.checked_add(u64::from(offset)))
            {
                match config_type {
                    VIRTIO_PCI_CAP_COMMON_CFG if region_length >= 56 => {
                        common = Some((address, region_length));
                    }
                    VIRTIO_PCI_CAP_NOTIFY_CFG if length >= 20 && region_length >= 2 => {
                        notify = Some((address, region_length));
                        notify_multiplier = function.read_u32(cursor + 16);
                    }
                    VIRTIO_PCI_CAP_ISR_CFG if region_length >= 1 => isr = Some(address),
                    VIRTIO_PCI_CAP_DEVICE_CFG if region_length >= 8 => {
                        device = Some((address, region_length));
                    }
                    _ => {}
                }
            }
        }
        if next == 0 || next == cursor {
            break;
        }
        cursor = next;
    }
    let (common, common_length) = common?;
    let (notify, notify_length) = notify?;
    let (device, device_length) = device?;
    Some(Regions {
        function,
        common,
        common_length,
        notify,
        notify_length,
        notify_multiplier,
        isr: isr?,
        device,
        device_length,
    })
}

impl Function {
    fn address(self, offset: u16) -> u64 {
        self.ecam
            + (u64::from(self.slot) << 15)
            + (u64::from(self.function) << 12)
            + u64::from(offset)
    }

    fn read_u32(self, offset: u8) -> u32 {
        read_u32(self.address(u16::from(offset & !3)))
    }

    fn read_u16(self, offset: u8) -> u16 {
        let shift = u32::from(offset & 2) * 8;
        (self.read_u32(offset) >> shift) as u16
    }

    fn read_u8(self, offset: u8) -> u8 {
        let shift = u32::from(offset & 3) * 8;
        (self.read_u32(offset) >> shift) as u8
    }

    fn write_u16(self, offset: u8, value: u16) {
        // 16-bit ECAM store меняет только PCI command register и не пишет
        // обратно соседний W1C status register.
        write_u16(self.address(u16::from(offset)), value);
    }

    fn bar(self, index: u8) -> Option<u64> {
        if index >= 6 {
            return None;
        }
        let offset = 0x10 + index * 4;
        let low = self.read_u32(offset);
        if low == 0 || low == u32::MAX || low & 1 != 0 {
            return None;
        }
        let kind = (low >> 1) & 3;
        let mut address = u64::from(low & !0x0f);
        if kind == 2 {
            if index == 5 {
                return None;
            }
            address |= u64::from(self.read_u32(offset + 4)) << 32;
        } else if kind != 0 {
            return None;
        }
        (address != 0).then_some(address)
    }
}

fn read_u64_stable(address: u64) -> u64 {
    // Capacity может пересекать два 32-bit MMIO access. Для обычного raw disk
    // она неизменна, но повтор high исключает torn read и ничего не стоит.
    loop {
        let high_before = read_u32(address + 4);
        let low = read_u32(address);
        let high_after = read_u32(address + 4);
        if high_before == high_after {
            return u64::from(low) | (u64::from(high_after) << 32);
        }
    }
}

#[inline]
fn read_u8(address: u64) -> u8 {
    unsafe { (address as *const u8).read_volatile() }
}

#[inline]
fn read_u16(address: u64) -> u16 {
    unsafe { (address as *const u16).read_volatile() }
}

#[inline]
fn read_u32(address: u64) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}

#[inline]
fn write_u8(address: u64, value: u8) {
    unsafe { (address as *mut u8).write_volatile(value) };
}

#[inline]
fn write_u16(address: u64, value: u16) {
    unsafe { (address as *mut u16).write_volatile(value) };
}

#[inline]
fn write_u32(address: u64, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) };
}

#[inline]
fn write_u64(address: u64, value: u64) {
    unsafe { (address as *mut u64).write_volatile(value) };
}

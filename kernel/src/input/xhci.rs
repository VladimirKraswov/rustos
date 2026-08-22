//! Современный xHCI USB host controller и HID keyboard/pointer.
//!
//! Драйвер использует DMA command/event/transfer rings и не исполняет policy
//! оконной системы. Сейчас он работает как bounded bootstrap transport в
//! kernel; descriptor/HID parser уже вынесен в переносимый `rustos-usb`,
//! поэтому при появлении IOMMU/IRQ capabilities transport переедет в `usbd`.

use core::ptr;

use rustos_abi::input::{MouseCapabilities, MouseSettings};
use rustos_usb::{
    endpoint_zero_packet_size, find_hid_interface, AbsolutePointerReport, HidInterface, HidKind,
    KeyboardReport, MouseReport,
};

use crate::{
    arch,
    input::{Event, Key, MouseEvent, PointerMotion},
    memory::{self, FrameBlock},
    serial,
};

const PAGE_SIZE: u64 = 4096;
const RING_TRBS: usize = 256;
const MAX_DEVICES: usize = 8;
const MAX_PORTS: usize = 32;
const MAX_SCRATCHPADS: usize = 16;
const EVENT_QUEUE_CAPACITY: usize = 32;
const DEFERRED_EVENT_CAPACITY: usize = 32;
const POLL_LIMIT: usize = 10_000_000;

const CAP_CAPLENGTH: u64 = 0x00;
const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCSPARAMS2: u64 = 0x08;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;

const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;
const OP_PAGESIZE: u64 = 0x08;
const OP_CRCR: u64 = 0x18;
const OP_DCBAAP: u64 = 0x30;
const OP_CONFIG: u64 = 0x38;
const OP_PORTS: u64 = 0x400;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const STATUS_HALTED: u32 = 1 << 0;
const STATUS_NOT_READY: u32 = 1 << 11;

const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
const PORT_SPEED_SHIFT: u32 = 10;
const PORT_CHANGE_BITS: u32 = 0x7f << 17;

const RUNTIME_INTERRUPTER_ZERO: u64 = 0x20;
const IR_IMAN: u64 = 0x00;
const IR_ERSTSZ: u64 = 0x08;
const IR_ERSTBA: u64 = 0x10;
const IR_ERDP: u64 = 0x18;
const ERDP_EVENT_HANDLER_BUSY: u64 = 1 << 3;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
const TRB_IMMEDIATE_DATA: u32 = 1 << 6;
const TRB_DIRECTION_IN: u32 = 1 << 16;
const TRB_TYPE_SHIFT: u32 = 10;

const TRB_NORMAL: u32 = 1;
const TRB_SETUP_STAGE: u32 = 2;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;

const COMPLETION_SUCCESS: u8 = 1;
const COMPLETION_SHORT_PACKET: u8 = 13;

const ENDPOINT_TYPE_CONTROL: u32 = 4;
const ENDPOINT_TYPE_INTERRUPT_IN: u32 = 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsbError {
    Unsupported,
    OutOfMemory,
    Controller,
    Timeout,
    Transfer,
    Descriptor,
    Capacity,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default)]
struct Trb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl Trb {
    const fn command(kind: u32, parameter: u64, control: u32) -> Self {
        Self {
            parameter,
            status: 0,
            control: control | (kind << TRB_TYPE_SHIFT),
        }
    }

    const fn kind(self) -> u32 {
        (self.control >> TRB_TYPE_SHIFT) & 0x3f
    }

    const fn completion_code(self) -> u8 {
        (self.status >> 24) as u8
    }

    const fn slot_id(self) -> u8 {
        (self.control >> 24) as u8
    }

    const fn endpoint_id(self) -> u8 {
        ((self.control >> 16) & 0x1f) as u8
    }
}

#[derive(Clone, Copy)]
struct ProducerRing {
    frame: u64,
    enqueue: usize,
    cycle: bool,
}

impl ProducerRing {
    fn initialize(frame: u64) -> Self {
        unsafe { ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
        let ring = Self {
            frame,
            enqueue: 0,
            cycle: true,
        };
        ring.write_link();
        ring
    }

    fn push(&mut self, mut trb: Trb) -> Result<u64, UsbError> {
        if self.enqueue >= RING_TRBS - 1 {
            return Err(UsbError::Capacity);
        }
        trb.control = (trb.control & !TRB_CYCLE) | u32::from(self.cycle);
        let address = self.frame + self.enqueue as u64 * 16;
        unsafe { (address as *mut Trb).write_volatile(trb) };
        self.enqueue += 1;
        if self.enqueue == RING_TRBS - 1 {
            self.write_link();
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        Ok(address)
    }

    fn write_link(&self) {
        let link = Trb {
            parameter: self.frame,
            status: 0,
            control: (TRB_LINK << TRB_TYPE_SHIFT) | TRB_TOGGLE_CYCLE | u32::from(self.cycle),
        };
        unsafe { ((self.frame + (RING_TRBS as u64 - 1) * 16) as *mut Trb).write_volatile(link) };
    }
}

#[derive(Clone, Copy)]
struct DeferredEvents {
    values: [Trb; DEFERRED_EVENT_CAPACITY],
    head: usize,
    length: usize,
}

impl DeferredEvents {
    const fn new() -> Self {
        Self {
            values: [Trb {
                parameter: 0,
                status: 0,
                control: 0,
            }; DEFERRED_EVENT_CAPACITY],
            head: 0,
            length: 0,
        }
    }

    fn push(&mut self, event: Trb) {
        if self.length == DEFERRED_EVENT_CAPACITY {
            // Input reports являются состоянием, а не бесконечным журналом.
            // При переполнении вытесняем самый старый report, сохраняя новые
            // button/key snapshots и гарантируя bounded kernel memory.
            self.head = (self.head + 1) % DEFERRED_EVENT_CAPACITY;
            self.length -= 1;
        }
        let tail = (self.head + self.length) % DEFERRED_EVENT_CAPACITY;
        self.values[tail] = event;
        self.length += 1;
    }

    fn pop(&mut self) -> Option<Trb> {
        if self.length == 0 {
            return None;
        }
        let value = self.values[self.head];
        self.head = (self.head + 1) % DEFERRED_EVENT_CAPACITY;
        self.length -= 1;
        Some(value)
    }
}

struct Controller {
    operational: u64,
    runtime: u64,
    doorbells: u64,
    max_slots: u8,
    max_ports: u8,
    context_size: usize,
    core: FrameBlock,
    command: ProducerRing,
    event_dequeue: usize,
    event_cycle: bool,
    deferred: DeferredEvents,
}

impl Controller {
    fn initialize() -> Result<Self, UsbError> {
        let mmio = discover_xhci().ok_or(UsbError::Unsupported)?;
        let cap_length = u64::from(read8(mmio + CAP_CAPLENGTH));
        if !(0x20..=0x80).contains(&cap_length) {
            return Err(UsbError::Controller);
        }
        let hcs1 = read32(mmio + CAP_HCSPARAMS1);
        let max_slots = (hcs1 as u8).min(MAX_DEVICES as u8);
        let max_ports = ((hcs1 >> 24) as u8).min(MAX_PORTS as u8);
        if max_slots == 0 || max_ports == 0 {
            return Err(UsbError::Unsupported);
        }
        let hcc1 = read32(mmio + CAP_HCCPARAMS1);
        let context_size = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
        take_firmware_ownership(mmio, hcc1)?;

        let operational = mmio + cap_length;
        let runtime = mmio + u64::from(read32(mmio + CAP_RTSOFF) & !0x1f);
        let doorbells = mmio + u64::from(read32(mmio + CAP_DBOFF) & !3);
        write32(
            operational + OP_USBCMD,
            read32(operational + OP_USBCMD) & !CMD_RUN,
        );
        wait_until(|| read32(operational + OP_USBSTS) & STATUS_HALTED != 0)?;
        write32(operational + OP_USBCMD, CMD_RESET);
        wait_until(|| read32(operational + OP_USBCMD) & CMD_RESET == 0)?;
        wait_until(|| read32(operational + OP_USBSTS) & STATUS_NOT_READY == 0)?;
        if read32(operational + OP_PAGESIZE) & 1 == 0 {
            return Err(UsbError::Unsupported);
        }

        let hcs2 = read32(mmio + CAP_HCSPARAMS2);
        let scratchpads = (((hcs2 >> 21) & 0x1f) | (((hcs2 >> 27) & 0x1f) << 5)) as usize;
        if scratchpads > MAX_SCRATCHPADS {
            return Err(UsbError::Capacity);
        }
        // DCBAA, command ring, event ring, ERST, scratchpad pointer array,
        // затем сами scratchpad pages — один непрерывный owned DMA extent.
        let core =
            memory::allocate(5 + scratchpads as u64, 1).map_err(|_| UsbError::OutOfMemory)?;
        unsafe { ptr::write_bytes(core.phys as *mut u8, 0, (core.frames * PAGE_SIZE) as usize) };
        let dcbaa = core.phys;
        let command_frame = core.phys + PAGE_SIZE;
        let event_frame = core.phys + PAGE_SIZE * 2;
        let erst = core.phys + PAGE_SIZE * 3;
        let scratchpad_array = core.phys + PAGE_SIZE * 4;
        if scratchpads != 0 {
            unsafe { (dcbaa as *mut u64).write_volatile(scratchpad_array) };
            for index in 0..scratchpads {
                unsafe {
                    (scratchpad_array as *mut u64)
                        .add(index)
                        .write_volatile(core.phys + PAGE_SIZE * (5 + index as u64))
                };
            }
        }
        let command = ProducerRing::initialize(command_frame);
        unsafe {
            (erst as *mut u64).write_volatile(event_frame);
            ((erst + 8) as *mut u32).write_volatile(RING_TRBS as u32);
        }

        let interrupter = runtime + RUNTIME_INTERRUPTER_ZERO;
        write32(interrupter + IR_IMAN, 0);
        write32(interrupter + IR_ERSTSZ, 1);
        write64(interrupter + IR_ERSTBA, erst);
        write64(interrupter + IR_ERDP, event_frame);
        write64(operational + OP_DCBAAP, dcbaa);
        write64(operational + OP_CRCR, command_frame | 1);
        write32(operational + OP_CONFIG, u32::from(max_slots));
        arch::dma_write_barrier();
        write32(operational + OP_USBCMD, CMD_RUN);
        if let Err(error) = wait_until(|| read32(operational + OP_USBSTS) & STATUS_HALTED == 0) {
            // `Self` ещё не создан, поэтому Drop здесь не сработает.
            // Остановка и явный возврат DMA extent исключают утечку при
            // неисправном контроллере, который не выходит из Halted.
            write32(operational + OP_USBCMD, 0);
            let _ = memory::free(core);
            return Err(error);
        }

        serial::put_str("[usb] xhci controller ready slots=");
        serial::put_u32(u32::from(max_slots));
        serial::put_str(" ports=");
        serial::put_u32(u32::from(max_ports));
        serial::put_str(" context=");
        serial::put_u32(context_size as u32);
        serial::put_str("B polling=bounded\n");
        Ok(Self {
            operational,
            runtime,
            doorbells,
            max_slots,
            max_ports,
            context_size,
            core,
            command,
            event_dequeue: 0,
            event_cycle: true,
            deferred: DeferredEvents::new(),
        })
    }

    fn dcbaa(&self) -> u64 {
        self.core.phys
    }

    fn port_status(&self, port: u8) -> u32 {
        read32(self.operational + OP_PORTS + u64::from(port - 1) * 0x10)
    }

    fn clear_port_changes(&self, port: u8, status: u32) {
        write32(
            self.operational + OP_PORTS + u64::from(port - 1) * 0x10,
            (status & PORT_POWER) | (status & PORT_CHANGE_BITS),
        );
    }

    fn reset_port(&self, port: u8) -> Result<u8, UsbError> {
        let address = self.operational + OP_PORTS + u64::from(port - 1) * 0x10;
        let status = read32(address);
        if status & PORT_CONNECTED == 0 {
            return Err(UsbError::Transfer);
        }
        write32(
            address,
            PORT_POWER | PORT_RESET | (status & PORT_CHANGE_BITS),
        );
        wait_until(|| {
            let current = read32(address);
            current & PORT_RESET == 0 && current & PORT_ENABLED != 0
        })?;
        let current = read32(address);
        self.clear_port_changes(port, current);
        let speed = ((current >> PORT_SPEED_SHIFT) & 0x0f) as u8;
        if !(1..=5).contains(&speed) {
            return Err(UsbError::Unsupported);
        }
        Ok(speed)
    }

    fn command(&mut self, command: Trb) -> Result<Trb, UsbError> {
        let pointer = self.command.push(command)?;
        arch::dma_write_barrier();
        write32(self.doorbells, 0);
        for _ in 0..POLL_LIMIT {
            if let Some(event) = self.hardware_event() {
                if event.kind() == TRB_COMMAND_COMPLETION && event.parameter & !0x0f == pointer {
                    return if event.completion_code() == COMPLETION_SUCCESS {
                        Ok(event)
                    } else {
                        Err(UsbError::Controller)
                    };
                }
                self.deferred.push(event);
            }
            core::hint::spin_loop();
        }
        Err(UsbError::Timeout)
    }

    fn enable_slot(&mut self) -> Result<u8, UsbError> {
        let event = self.command(Trb::command(TRB_ENABLE_SLOT, 0, 0))?;
        let slot = event.slot_id();
        if slot == 0 || slot > self.max_slots {
            Err(UsbError::Controller)
        } else {
            Ok(slot)
        }
    }

    fn disable_slot(&mut self, slot: u8) {
        let _ = self.command(Trb::command(TRB_DISABLE_SLOT, 0, u32::from(slot) << 24));
    }

    fn address_device(&mut self, slot: u8, input_context: u64) -> Result<(), UsbError> {
        self.command(Trb::command(
            TRB_ADDRESS_DEVICE,
            input_context,
            u32::from(slot) << 24,
        ))?;
        Ok(())
    }

    fn evaluate_context(&mut self, slot: u8, input_context: u64) -> Result<(), UsbError> {
        self.command(Trb::command(
            TRB_EVALUATE_CONTEXT,
            input_context,
            u32::from(slot) << 24,
        ))?;
        Ok(())
    }

    fn configure_endpoint(&mut self, slot: u8, input_context: u64) -> Result<(), UsbError> {
        self.command(Trb::command(
            TRB_CONFIGURE_ENDPOINT,
            input_context,
            u32::from(slot) << 24,
        ))?;
        Ok(())
    }

    fn ring_doorbell(&self, slot: u8, endpoint: u8) {
        arch::dma_write_barrier();
        write32(self.doorbells + u64::from(slot) * 4, u32::from(endpoint));
    }

    fn wait_transfer(&mut self, slot: u8, endpoint: u8, pointer: u64) -> Result<Trb, UsbError> {
        for _ in 0..POLL_LIMIT {
            if let Some(event) = self.hardware_event() {
                if event.kind() == TRB_TRANSFER_EVENT
                    && event.slot_id() == slot
                    && event.endpoint_id() == endpoint
                    && event.parameter & !0x0f == pointer
                {
                    return if matches!(
                        event.completion_code(),
                        COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET
                    ) {
                        Ok(event)
                    } else {
                        Err(UsbError::Transfer)
                    };
                }
                self.deferred.push(event);
            }
            core::hint::spin_loop();
        }
        Err(UsbError::Timeout)
    }

    fn pop_event(&mut self) -> Option<Trb> {
        self.deferred.pop().or_else(|| self.hardware_event())
    }

    fn hardware_event(&mut self) -> Option<Trb> {
        let event_frame = self.core.phys + PAGE_SIZE * 2;
        let address = event_frame + self.event_dequeue as u64 * 16;
        let event = unsafe { (address as *const Trb).read_volatile() };
        if event.control & TRB_CYCLE != u32::from(self.event_cycle) {
            return None;
        }
        arch::dma_read_barrier();
        self.event_dequeue += 1;
        if self.event_dequeue == RING_TRBS {
            self.event_dequeue = 0;
            self.event_cycle = !self.event_cycle;
        }
        let next = event_frame + self.event_dequeue as u64 * 16;
        write64(
            self.runtime + RUNTIME_INTERRUPTER_ZERO + IR_ERDP,
            next | ERDP_EVENT_HANDLER_BUSY,
        );
        Some(event)
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        write32(self.operational + OP_USBCMD, 0);
        let _ = memory::free(self.core);
    }
}

struct UsbDevice {
    port: u8,
    speed: u8,
    slot: u8,
    endpoint_id: u8,
    interface: HidInterface,
    frames: FrameBlock,
    control_ring: ProducerRing,
    interrupt_ring: ProducerRing,
    interrupt_in_flight: bool,
    report_size: u16,
    previous_keyboard: KeyboardReport,
    caps_lock: bool,
}

impl UsbDevice {
    fn device_context(&self) -> u64 {
        self.frames.phys
    }

    fn input_context(&self) -> u64 {
        self.frames.phys + PAGE_SIZE
    }

    fn report_buffer(&self) -> u64 {
        self.frames.phys + PAGE_SIZE * 4
    }
}

struct InputQueue {
    values: [Option<Event>; EVENT_QUEUE_CAPACITY],
    head: usize,
    length: usize,
}

impl InputQueue {
    const fn new() -> Self {
        Self {
            values: [None; EVENT_QUEUE_CAPACITY],
            head: 0,
            length: 0,
        }
    }

    fn push(&mut self, event: Event) {
        if self.length == EVENT_QUEUE_CAPACITY {
            self.head = (self.head + 1) % EVENT_QUEUE_CAPACITY;
            self.length -= 1;
        }
        let tail = (self.head + self.length) % EVENT_QUEUE_CAPACITY;
        self.values[tail] = Some(event);
        self.length += 1;
    }

    fn pop(&mut self) -> Option<Event> {
        if self.length == 0 {
            return None;
        }
        let event = self.values[self.head].take();
        self.head = (self.head + 1) % EVENT_QUEUE_CAPACITY;
        self.length -= 1;
        event
    }
}

/// Объединённый USB HID input backend. Один controller обслуживает несколько
/// root-port устройств и повторно сканирует их после Port Status Change Event.
pub struct UsbInput {
    controller: Controller,
    devices: [Option<UsbDevice>; MAX_DEVICES],
    events: InputQueue,
    settings: MouseSettings,
    remainder_x: i32,
    remainder_y: i32,
    rescan: bool,
}

impl UsbInput {
    pub fn new() -> Option<Self> {
        let controller = Controller::initialize().ok()?;
        let mut input = Self {
            controller,
            devices: core::array::from_fn(|_| None),
            events: InputQueue::new(),
            settings: MouseSettings::DEFAULT,
            remainder_x: 0,
            remainder_y: 0,
            rescan: true,
        };
        input.rescan_ports();
        input.arm_all();
        Some(input)
    }

    pub fn has_keyboard(&self) -> bool {
        self.devices
            .iter()
            .flatten()
            .any(|device| device.interface.kind == HidKind::Keyboard)
    }

    pub fn has_mouse(&self) -> bool {
        self.devices.iter().flatten().any(|device| {
            matches!(
                device.interface.kind,
                HidKind::RelativePointer | HidKind::AbsolutePointer
            )
        })
    }

    pub fn poll(&mut self) -> Option<Event> {
        if let Some(event) = self.events.pop() {
            return Some(event);
        }
        for _ in 0..64 {
            let Some(event) = self.controller.pop_event() else {
                break;
            };
            match event.kind() {
                TRB_TRANSFER_EVENT => self.handle_transfer(event),
                TRB_PORT_STATUS_CHANGE => self.rescan = true,
                _ => {}
            }
            if let Some(event) = self.events.pop() {
                return Some(event);
            }
        }
        if self.rescan {
            self.rescan_ports();
            self.arm_all();
        }
        self.events.pop()
    }

    pub const fn mouse_settings(&self) -> MouseSettings {
        self.settings
    }

    pub const fn mouse_capabilities(&self) -> MouseCapabilities {
        MouseCapabilities {
            configurable_sample_rate: 0,
            configurable_resolution: 0,
            wheel: 1,
            extra_buttons: 1,
            minimum_rate_hz: 0,
            maximum_rate_hz: 0,
            resolution_levels: 0,
            reserved: [0; 7],
        }
    }

    pub fn set_mouse_settings(&mut self, requested: MouseSettings) -> bool {
        self.settings = requested.sanitized();
        self.remainder_x = 0;
        self.remainder_y = 0;
        true
    }

    fn rescan_ports(&mut self) {
        self.rescan = false;
        for port in 1..=self.controller.max_ports {
            let connected = self.controller.port_status(port) & PORT_CONNECTED != 0;
            let existing = self
                .devices
                .iter()
                .position(|device| device.as_ref().is_some_and(|device| device.port == port));
            match (connected, existing) {
                (false, Some(index)) => self.remove_device(index),
                (true, None) => {
                    if let Ok(device) = self.enumerate(port) {
                        if let Some(slot) = self.devices.iter_mut().find(|slot| slot.is_none()) {
                            serial::put_str("[usb] hid attached port=");
                            serial::put_u32(u32::from(port));
                            serial::put_str(" kind=");
                            serial::put_str(match device.interface.kind {
                                HidKind::Keyboard => "keyboard",
                                HidKind::RelativePointer => "relative-pointer",
                                HidKind::AbsolutePointer => "absolute-tablet",
                            });
                            serial::put_str(" speed=");
                            serial::put_u32(u32::from(device.speed));
                            serial::put_str(" interrupt-in=yes\n");
                            *slot = Some(device);
                        } else {
                            self.controller.disable_slot(device.slot);
                            let _ = memory::free(device.frames);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn enumerate(&mut self, port: u8) -> Result<UsbDevice, UsbError> {
        let speed = self.controller.reset_port(port)?;
        let slot = self.controller.enable_slot()?;
        let frames = match memory::allocate(5, 1) {
            Ok(frames) => frames,
            Err(_) => {
                self.controller.disable_slot(slot);
                return Err(UsbError::OutOfMemory);
            }
        };
        unsafe {
            ptr::write_bytes(
                frames.phys as *mut u8,
                0,
                (frames.frames * PAGE_SIZE) as usize,
            )
        };
        let control_ring = ProducerRing::initialize(frames.phys + PAGE_SIZE * 2);
        let interrupt_ring = ProducerRing::initialize(frames.phys + PAGE_SIZE * 3);
        let placeholder = HidInterface {
            kind: HidKind::Keyboard,
            configuration_value: 0,
            interface_number: 0,
            endpoint_address: 0,
            interval: 1,
            max_packet_size: 8,
        };
        let mut device = UsbDevice {
            port,
            speed,
            slot,
            endpoint_id: 0,
            interface: placeholder,
            frames,
            control_ring,
            interrupt_ring,
            interrupt_in_flight: false,
            report_size: 0,
            previous_keyboard: KeyboardReport::default(),
            caps_lock: false,
        };
        let result = self.enumerate_inner(&mut device);
        if let Err(error) = result {
            self.controller.disable_slot(slot);
            unsafe {
                (self.controller.dcbaa() as *mut u64)
                    .add(usize::from(slot))
                    .write_volatile(0)
            };
            let _ = memory::free(frames);
            return Err(error);
        }
        Ok(device)
    }

    fn enumerate_inner(&mut self, device: &mut UsbDevice) -> Result<(), UsbError> {
        let initial_packet = match device.speed {
            3 => 64,
            4 | 5 => 512,
            _ => 8,
        };
        unsafe {
            (self.controller.dcbaa() as *mut u64)
                .add(usize::from(device.slot))
                .write_volatile(device.device_context())
        };
        self.build_address_context(device, initial_packet);
        self.controller
            .address_device(device.slot, device.input_context())?;

        self.control_in(device, 0x80, 6, 0x0100, 0, 18)?;
        let descriptor =
            unsafe { core::slice::from_raw_parts(device.report_buffer() as *const u8, 18) };
        let packet_size =
            endpoint_zero_packet_size(descriptor).map_err(|_| UsbError::Descriptor)?;
        if packet_size != initial_packet {
            self.update_ep0_packet_size(device, packet_size);
            self.controller
                .evaluate_context(device.slot, device.input_context())?;
        }

        self.control_in(device, 0x80, 6, 0x0200, 0, 9)?;
        let header = unsafe { core::slice::from_raw_parts(device.report_buffer() as *const u8, 9) };
        let total = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if !(9..=PAGE_SIZE as usize).contains(&total) {
            return Err(UsbError::Descriptor);
        }
        self.control_in(device, 0x80, 6, 0x0200, 0, total as u16)?;
        let configuration =
            unsafe { core::slice::from_raw_parts(device.report_buffer() as *const u8, total) };
        device.interface = find_hid_interface(configuration).map_err(|_| UsbError::Descriptor)?;
        device.endpoint_id = endpoint_context_index(device.interface.endpoint_address);
        device.report_size = match device.interface.kind {
            HidKind::Keyboard => 8,
            HidKind::RelativePointer => device.interface.max_packet_size.clamp(3, 8),
            HidKind::AbsolutePointer => device.interface.max_packet_size.clamp(6, 8),
        };
        self.build_endpoint_context(device);
        self.controller
            .configure_endpoint(device.slot, device.input_context())?;
        self.control_no_data(
            device,
            0x00,
            9,
            u16::from(device.interface.configuration_value),
            0,
        )?;
        // SET_PROTOCOL относится только к Boot subclass. Report-protocol
        // tablet закономерно может ответить STALL; такой ответ нельзя считать
        // отказом всего устройства.
        if matches!(
            device.interface.kind,
            HidKind::Keyboard | HidKind::RelativePointer
        ) {
            self.control_no_data(
                device,
                0x21,
                11,
                0,
                u16::from(device.interface.interface_number),
            )?;
        }
        if device.interface.kind == HidKind::Keyboard {
            let _ = self.control_no_data(
                device,
                0x21,
                10,
                0,
                u16::from(device.interface.interface_number),
            );
        }
        Ok(())
    }

    fn build_address_context(&self, device: &UsbDevice, packet_size: u16) {
        let input = device.input_context();
        unsafe { ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE as usize) };
        context_write(input, 0, self.controller.context_size, 1, 0b11);
        context_write(
            input,
            1,
            self.controller.context_size,
            0,
            (u32::from(device.speed) << 20) | (1 << 27),
        );
        context_write(
            input,
            1,
            self.controller.context_size,
            1,
            u32::from(device.port) << 16,
        );
        write_endpoint_context(
            input,
            2,
            self.controller.context_size,
            EndpointContextConfig {
                endpoint_type: ENDPOINT_TYPE_CONTROL,
                max_packet: packet_size,
                interval: 0,
                dequeue: device.control_ring.frame,
                average_trb_length: 8,
            },
        );
    }

    fn update_ep0_packet_size(&self, device: &UsbDevice, packet_size: u16) {
        let input = device.input_context();
        unsafe { ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE as usize) };
        context_write(input, 0, self.controller.context_size, 1, 1 << 1);
        // Evaluate Context читает только отмеченный EP0 context.
        let device_ep0 = device.device_context() + self.controller.context_size as u64;
        unsafe {
            ptr::copy_nonoverlapping(
                device_ep0 as *const u8,
                (input + (self.controller.context_size * 2) as u64) as *mut u8,
                self.controller.context_size,
            )
        };
        let current = context_read(input, 2, self.controller.context_size, 1);
        context_write(
            input,
            2,
            self.controller.context_size,
            1,
            (current & 0x0000_ffff) | (u32::from(packet_size) << 16),
        );
    }

    fn build_endpoint_context(&self, device: &UsbDevice) {
        let input = device.input_context();
        unsafe { ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE as usize) };
        let context_size = self.controller.context_size;
        // Slot context из output device context содержит назначенный USB
        // address/state; копирование сохраняет поля, которыми владеет xHC.
        unsafe {
            ptr::copy_nonoverlapping(
                device.device_context() as *const u8,
                (input + context_size as u64) as *mut u8,
                context_size,
            )
        };
        let slot0 = context_read(input, 1, context_size, 0);
        context_write(
            input,
            1,
            context_size,
            0,
            (slot0 & !(0x1f << 27)) | (u32::from(device.endpoint_id) << 27),
        );
        context_write(input, 0, context_size, 1, 1 | (1u32 << device.endpoint_id));
        let interval = xhci_interval(device.speed, device.interface.interval);
        write_endpoint_context(
            input,
            usize::from(device.endpoint_id) + 1,
            context_size,
            EndpointContextConfig {
                endpoint_type: ENDPOINT_TYPE_INTERRUPT_IN,
                max_packet: device.interface.max_packet_size,
                interval,
                dequeue: device.interrupt_ring.frame,
                average_trb_length: device.report_size,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn control_in(
        &mut self,
        device: &mut UsbDevice,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Result<usize, UsbError> {
        unsafe { ptr::write_bytes(device.report_buffer() as *mut u8, 0, usize::from(length)) };
        let setup = setup_packet(request_type, request, value, index, length);
        device.control_ring.push(Trb {
            parameter: setup,
            status: 8,
            control: (TRB_SETUP_STAGE << TRB_TYPE_SHIFT) | TRB_IMMEDIATE_DATA | (3 << 16),
        })?;
        device.control_ring.push(Trb {
            parameter: device.report_buffer(),
            status: u32::from(length),
            control: (TRB_DATA_STAGE << TRB_TYPE_SHIFT) | TRB_DIRECTION_IN,
        })?;
        let status = device.control_ring.push(Trb {
            parameter: 0,
            status: 0,
            control: (TRB_STATUS_STAGE << TRB_TYPE_SHIFT) | TRB_INTERRUPT_ON_COMPLETION,
        })?;
        self.controller.ring_doorbell(device.slot, 1);
        let event = self.controller.wait_transfer(device.slot, 1, status)?;
        let residual = (event.status & 0x00ff_ffff).min(u32::from(length));
        Ok(usize::from(length) - residual as usize)
    }

    fn control_no_data(
        &mut self,
        device: &mut UsbDevice,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
    ) -> Result<(), UsbError> {
        let setup = setup_packet(request_type, request, value, index, 0);
        device.control_ring.push(Trb {
            parameter: setup,
            status: 8,
            control: (TRB_SETUP_STAGE << TRB_TYPE_SHIFT) | TRB_IMMEDIATE_DATA,
        })?;
        let status = device.control_ring.push(Trb {
            parameter: 0,
            status: 0,
            control: (TRB_STATUS_STAGE << TRB_TYPE_SHIFT)
                | TRB_DIRECTION_IN
                | TRB_INTERRUPT_ON_COMPLETION,
        })?;
        self.controller.ring_doorbell(device.slot, 1);
        self.controller.wait_transfer(device.slot, 1, status)?;
        Ok(())
    }

    fn arm_all(&mut self) {
        for device in self.devices.iter_mut().flatten() {
            if !device_interrupt_armed(device) {
                arm_interrupt(&self.controller, device);
            }
        }
    }

    fn handle_transfer(&mut self, event: Trb) {
        let Some(index) = self.devices.iter().position(|device| {
            device.as_ref().is_some_and(|device| {
                device.slot == event.slot_id() && device.endpoint_id == event.endpoint_id()
            })
        }) else {
            return;
        };
        let completion = event.completion_code();
        if !matches!(completion, COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET) {
            // Halted/broken endpoint нельзя считать всё ещё armed: удаляем
            // slot и на следующем rescan перечисляем подключённое устройство
            // заново. Это восстанавливает ввод без перезагрузки всей ОС.
            self.remove_device(index);
            self.rescan = true;
            return;
        }
        let mut device = self.devices[index].take().expect("device index checked");
        device.interrupt_in_flight = false;
        let residual = (event.status & 0x00ff_ffff).min(u32::from(device.report_size));
        let length = usize::from(device.report_size) - residual as usize;
        arch::dma_read_barrier();
        let bytes =
            unsafe { core::slice::from_raw_parts(device.report_buffer() as *const u8, length) };
        match device.interface.kind {
            HidKind::Keyboard => self.decode_keyboard(&mut device, bytes),
            HidKind::RelativePointer => self.decode_mouse(bytes),
            HidKind::AbsolutePointer => self.decode_absolute_pointer(bytes),
        }
        arm_interrupt(&self.controller, &mut device);
        self.devices[index] = Some(device);
    }

    fn decode_keyboard(&mut self, device: &mut UsbDevice, bytes: &[u8]) {
        let Some(report) = KeyboardReport::decode(bytes) else {
            return;
        };
        let previous = device.previous_keyboard;
        report.newly_pressed(previous, |usage| {
            if usage == 57 {
                device.caps_lock = !device.caps_lock;
            } else if let Some(key) = hid_usage_key(usage, report.shift(), device.caps_lock) {
                self.events.push(Event::Key(key));
            }
        });
        device.previous_keyboard = report;
    }

    fn decode_mouse(&mut self, bytes: &[u8]) {
        let Some(report) = MouseReport::decode(bytes) else {
            return;
        };
        let (dx, dy) = self.scale_motion(report.dx, report.dy);
        self.events.push(Event::Mouse(MouseEvent {
            motion: PointerMotion::Relative { dx, dy },
            wheel_x: 0,
            // USB HID: положительное wheel означает вверх; UI — вниз.
            wheel_y: -report.wheel,
            left: report.buttons & 1 != 0,
            right: report.buttons & 2 != 0,
            middle: report.buttons & 4 != 0,
            packets: 1,
        }));
    }

    fn decode_absolute_pointer(&mut self, bytes: &[u8]) {
        let Some(report) = AbsolutePointerReport::decode(bytes) else {
            return;
        };
        self.events.push(Event::Mouse(MouseEvent {
            motion: PointerMotion::Absolute {
                x: report.x,
                y: report.y,
                maximum_x: AbsolutePointerReport::MAXIMUM_COORDINATE,
                maximum_y: AbsolutePointerReport::MAXIMUM_COORDINATE,
            },
            wheel_x: 0,
            // QEMU HID следует USB convention: плюс означает wheel-up.
            wheel_y: -report.wheel,
            left: report.buttons & 1 != 0,
            right: report.buttons & 2 != 0,
            middle: report.buttons & 4 != 0,
            packets: 1,
        }));
    }

    fn scale_motion(&mut self, dx: i16, dy: i16) -> (i16, i16) {
        let speed = i32::from(dx)
            .abs()
            .saturating_add(i32::from(dy).abs())
            .min(32);
        let gain = i32::from(self.settings.sensitivity_percent)
            + i32::from(self.settings.acceleration_percent) * speed / 32;
        let x = i32::from(dx)
            .saturating_mul(gain)
            .saturating_add(self.remainder_x);
        let y = i32::from(dy)
            .saturating_mul(gain)
            .saturating_add(self.remainder_y);
        self.remainder_x = x % 100;
        self.remainder_y = y % 100;
        (
            (x / 100).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            (y / 100).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )
    }

    fn remove_device(&mut self, index: usize) {
        if let Some(device) = self.devices[index].take() {
            serial::put_str("[usb] hid detached port=");
            serial::put_u32(u32::from(device.port));
            serial::put_str("\n");
            self.controller.disable_slot(device.slot);
            unsafe {
                (self.controller.dcbaa() as *mut u64)
                    .add(usize::from(device.slot))
                    .write_volatile(0)
            };
            let _ = memory::free(device.frames);
        }
    }
}

fn arm_interrupt(controller: &Controller, device: &mut UsbDevice) {
    unsafe {
        ptr::write_bytes(
            device.report_buffer() as *mut u8,
            0,
            usize::from(device.report_size),
        )
    };
    if device
        .interrupt_ring
        .push(Trb {
            parameter: device.report_buffer(),
            status: u32::from(device.report_size),
            control: (TRB_NORMAL << TRB_TYPE_SHIFT) | TRB_INTERRUPT_ON_COMPLETION,
        })
        .is_ok()
    {
        device.interrupt_in_flight = true;
        controller.ring_doorbell(device.slot, device.endpoint_id);
    }
}

fn device_interrupt_armed(device: &UsbDevice) -> bool {
    device.interrupt_in_flight
}

struct EndpointContextConfig {
    endpoint_type: u32,
    max_packet: u16,
    interval: u8,
    dequeue: u64,
    average_trb_length: u16,
}

fn write_endpoint_context(
    base: u64,
    index: usize,
    context_size: usize,
    config: EndpointContextConfig,
) {
    context_write(
        base,
        index,
        context_size,
        0,
        u32::from(config.interval) << 16,
    );
    context_write(
        base,
        index,
        context_size,
        1,
        (3 << 1) | (config.endpoint_type << 3) | (u32::from(config.max_packet) << 16),
    );
    context_write(base, index, context_size, 2, config.dequeue as u32 | 1);
    context_write(base, index, context_size, 3, (config.dequeue >> 32) as u32);
    context_write(
        base,
        index,
        context_size,
        4,
        u32::from(config.average_trb_length) | (u32::from(config.max_packet) << 16),
    );
}

fn context_write(base: u64, index: usize, context_size: usize, dword: usize, value: u32) {
    unsafe {
        ((base + (index * context_size + dword * 4) as u64) as *mut u32).write_volatile(value)
    };
}

fn context_read(base: u64, index: usize, context_size: usize, dword: usize) -> u32 {
    unsafe { ((base + (index * context_size + dword * 4) as u64) as *const u32).read_volatile() }
}

fn endpoint_context_index(address: u8) -> u8 {
    let number = address & 0x0f;
    number * 2 + u8::from(address & 0x80 != 0)
}

fn xhci_interval(speed: u8, usb_interval: u8) -> u8 {
    if speed >= 3 {
        usb_interval.saturating_sub(1).min(15)
    } else {
        // Для full-/low-speed xHCI ждёт log2 периода в кадрах. Некорректный
        // дескриптор с bInterval > 128 не должен приводить к переполнению u8.
        usb_interval
            .clamp(1, 128)
            .next_power_of_two()
            .trailing_zeros() as u8
            + 3
    }
}

fn setup_packet(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> u64 {
    u64::from(request_type)
        | (u64::from(request) << 8)
        | (u64::from(value) << 16)
        | (u64::from(index) << 32)
        | (u64::from(length) << 48)
}

fn hid_usage_key(usage: u8, shift: bool, caps: bool) -> Option<Key> {
    if (4..=29).contains(&usage) {
        let letter = b'a' + usage - 4;
        return Some(Key::Character(if shift ^ caps {
            letter.to_ascii_uppercase()
        } else {
            letter
        }));
    }
    let key = match usage {
        40 => Key::Enter,
        41 => Key::Escape,
        42 => Key::Backspace,
        43 => Key::Tab,
        44 => Key::Character(b' '),
        74 => Key::Home,
        75 => Key::PageUp,
        77 => Key::End,
        78 => Key::PageDown,
        79 => Key::Right,
        80 => Key::Left,
        81 => Key::Down,
        82 => Key::Up,
        usage => Key::Character(match usage {
            30 => {
                if shift {
                    b'!'
                } else {
                    b'1'
                }
            }
            31 => {
                if shift {
                    b'@'
                } else {
                    b'2'
                }
            }
            32 => {
                if shift {
                    b'#'
                } else {
                    b'3'
                }
            }
            33 => {
                if shift {
                    b'$'
                } else {
                    b'4'
                }
            }
            34 => {
                if shift {
                    b'%'
                } else {
                    b'5'
                }
            }
            35 => {
                if shift {
                    b'^'
                } else {
                    b'6'
                }
            }
            36 => {
                if shift {
                    b'&'
                } else {
                    b'7'
                }
            }
            37 => {
                if shift {
                    b'*'
                } else {
                    b'8'
                }
            }
            38 => {
                if shift {
                    b'('
                } else {
                    b'9'
                }
            }
            39 => {
                if shift {
                    b')'
                } else {
                    b'0'
                }
            }
            45 => {
                if shift {
                    b'_'
                } else {
                    b'-'
                }
            }
            46 => {
                if shift {
                    b'+'
                } else {
                    b'='
                }
            }
            47 => {
                if shift {
                    b'{'
                } else {
                    b'['
                }
            }
            48 => {
                if shift {
                    b'}'
                } else {
                    b']'
                }
            }
            49 => {
                if shift {
                    b'|'
                } else {
                    b'\\'
                }
            }
            51 => {
                if shift {
                    b':'
                } else {
                    b';'
                }
            }
            52 => {
                if shift {
                    b'"'
                } else {
                    b'\''
                }
            }
            53 => {
                if shift {
                    b'~'
                } else {
                    b'`'
                }
            }
            54 => {
                if shift {
                    b'<'
                } else {
                    b','
                }
            }
            55 => {
                if shift {
                    b'>'
                } else {
                    b'.'
                }
            }
            56 => {
                if shift {
                    b'?'
                } else {
                    b'/'
                }
            }
            _ => return None,
        }),
    };
    Some(key)
}

fn wait_until(mut condition: impl FnMut() -> bool) -> Result<(), UsbError> {
    for _ in 0..POLL_LIMIT {
        if condition() {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(UsbError::Timeout)
}

fn take_firmware_ownership(mmio: u64, hcc1: u32) -> Result<(), UsbError> {
    let mut offset = u64::from(hcc1 >> 16) * 4;
    for _ in 0..64 {
        if offset == 0 || offset >= 0x4000 {
            return Ok(());
        }
        let capability = read32(mmio + offset);
        let id = capability as u8;
        let next = ((capability >> 8) & 0xff) as u64 * 4;
        if id == 1 {
            write32(mmio + offset, capability | (1 << 24));
            for _ in 0..1_000_000 {
                if read32(mmio + offset) & (1 << 16) == 0 {
                    // Отключаем legacy SMI после подтверждённой передачи.
                    write32(mmio + offset + 4, 0);
                    return Ok(());
                }
                core::hint::spin_loop();
            }
            return Err(UsbError::Timeout);
        }
        if next == 0 {
            return Ok(());
        }
        offset = offset.checked_add(next).ok_or(UsbError::Controller)?;
    }
    Err(UsbError::Controller)
}

#[derive(Clone, Copy)]
struct PciFunction {
    #[cfg(target_arch = "aarch64")]
    ecam: u64,
    bus: u8,
    slot: u8,
    function: u8,
}

impl PciFunction {
    fn read_u32(self, offset: u16) -> u32 {
        pci_read(self, offset & !3)
    }

    fn write_u32(self, offset: u16, value: u32) {
        pci_write(self, offset & !3, value)
    }

    fn bar0(self) -> Option<u64> {
        let low = self.read_u32(0x10);
        if low == 0 || low == u32::MAX || low & 1 != 0 {
            return None;
        }
        let kind = (low >> 1) & 3;
        let mut address = u64::from(low & !0x0f);
        if kind == 2 {
            address |= u64::from(self.read_u32(0x14)) << 32;
        } else if kind != 0 {
            return None;
        }
        (address != 0).then_some(address)
    }
}

fn discover_xhci() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    const ECAM_BASES: &[u64] = &[0];
    #[cfg(target_arch = "aarch64")]
    const ECAM_BASES: &[u64] = &[0x3f00_0000, 0x0040_1000_0000];

    for &ecam in ECAM_BASES {
        #[cfg(target_arch = "x86_64")]
        let _ = ecam;
        // У QEMU `virt` PCI host bridge публикуется ровно в одном из этих
        // окон. После первого валидного PCI function нельзя пробовать второе
        // окно вслепую: чтение отсутствующего ECAM на AArch64 вызывает
        // synchronous external abort, а не возвращает привычное 0xffff_ffff.
        let mut window_present = false;
        for slot in 0..32 {
            for function in 0..8 {
                let pci = PciFunction {
                    #[cfg(target_arch = "aarch64")]
                    ecam,
                    bus: 0,
                    slot,
                    function,
                };
                let id = pci.read_u32(0);
                if id == u32::MAX || id as u16 == 0xffff {
                    if function == 0 {
                        break;
                    }
                    continue;
                }
                window_present = true;
                let class = pci.read_u32(0x08);
                if class >> 8 != 0x000c_0330 {
                    continue;
                }
                let command = pci.read_u32(0x04);
                // MMIO + bus mastering; status bits нельзя записывать обратно.
                pci.write_u32(0x04, (command & 0xffff) | 0x0000_0006);
                return pci.bar0();
            }
        }
        if window_present {
            break;
        }
    }
    None
}

#[cfg(target_arch = "x86_64")]
fn pci_read(function: PciFunction, offset: u16) -> u32 {
    let address = 0x8000_0000
        | (u32::from(function.bus) << 16)
        | (u32::from(function.slot) << 11)
        | (u32::from(function.function) << 8)
        | u32::from(offset);
    unsafe {
        arch::outl(0xcf8, address);
        arch::inl(0xcfc)
    }
}

#[cfg(target_arch = "x86_64")]
fn pci_write(function: PciFunction, offset: u16, value: u32) {
    let address = 0x8000_0000
        | (u32::from(function.bus) << 16)
        | (u32::from(function.slot) << 11)
        | (u32::from(function.function) << 8)
        | u32::from(offset);
    unsafe {
        arch::outl(0xcf8, address);
        arch::outl(0xcfc, value);
    }
}

#[cfg(target_arch = "aarch64")]
fn pci_read(function: PciFunction, offset: u16) -> u32 {
    // QEMU `virt` может публиковать compact low ECAM или 256-MiB high ECAM.
    // Конкретное окно уже выбрано bounded discovery выше.
    let address = function.ecam
        + (u64::from(function.bus) << 20)
        + (u64::from(function.slot) << 15)
        + (u64::from(function.function) << 12)
        + u64::from(offset);
    unsafe { (address as *const u32).read_volatile() }
}

#[cfg(target_arch = "aarch64")]
fn pci_write(function: PciFunction, offset: u16, value: u32) {
    let address = function.ecam
        + (u64::from(function.bus) << 20)
        + (u64::from(function.slot) << 15)
        + (u64::from(function.function) << 12)
        + u64::from(offset);
    unsafe { (address as *mut u32).write_volatile(value) };
}

fn read8(address: u64) -> u8 {
    unsafe { (address as *const u8).read_volatile() }
}

fn read32(address: u64) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}

fn write32(address: u64, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) };
}

fn write64(address: u64, value: u64) {
    unsafe { (address as *mut u64).write_volatile(value) };
}

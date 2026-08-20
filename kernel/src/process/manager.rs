//! Реальный CPU0 process runner: preemption, block/wake, capability transfer
//! и bounded endpoint queues.

use core::{mem::size_of, ptr, slice, str};

use rustos_abi::{
    bootinfo::BootInitramfs,
    ipc::{Message, IPC_MAX_HANDLES},
    syscall::{self, status},
    ExitReason, Handle, PriorityClass, ProcessId, Rights, ThreadId,
};
use rustos_microkernel::{
    derive_capability_rights, prepare_message, CapabilityTransferError, EndpointQueue,
    IpcQueueError, ProcessTable, Scheduler,
};

use crate::{
    arch::{self, apic, traps::TrapFrame},
    fs,
    memory::{self, AddressSpace},
    serial,
};

use super::{
    elf, CapabilityEntry, CapabilityKind, ProcessError, EMPTY_CAPABILITY, MAX_CAPABILITIES,
    VFS_ROOT_SLOT,
};

const MAX_PROCESSES: usize = 6;
const MAX_ENDPOINTS: usize = 2;
const ENDPOINT_QUEUE_CAPACITY: usize = 8;
const ENDPOINT_SLOT: usize = 2;
const NO_EXIT: ExitReason = ExitReason {
    status: 0,
    exception: 0,
    flags: 0,
    fault_address: 0,
};

#[derive(Clone, Copy)]
struct UserContext {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    rip: u64,
    rflags: u64,
    rsp: u64,
}

impl UserContext {
    fn initial(entry: u64, stack: u64, arguments: [u64; 3]) -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rdi: arguments[0],
            rsi: arguments[1],
            rbp: 0,
            rdx: arguments[2],
            rcx: 0,
            rbx: 0,
            rax: 0,
            rip: entry,
            rflags: 0x202,
            rsp: stack,
        }
    }

    fn save(&mut self, frame: &TrapFrame) {
        self.r15 = frame.r15;
        self.r14 = frame.r14;
        self.r13 = frame.r13;
        self.r12 = frame.r12;
        self.r11 = frame.r11;
        self.r10 = frame.r10;
        self.r9 = frame.r9;
        self.r8 = frame.r8;
        self.rdi = frame.rdi;
        self.rsi = frame.rsi;
        self.rbp = frame.rbp;
        self.rdx = frame.rdx;
        self.rcx = frame.rcx;
        self.rbx = frame.rbx;
        self.rax = frame.rax;
        self.rip = frame.rip;
        self.rflags = frame.rflags;
        self.rsp = frame.rsp;
    }

    fn restore(&self, frame: &mut TrapFrame) {
        frame.r15 = self.r15;
        frame.r14 = self.r14;
        frame.r13 = self.r13;
        frame.r12 = self.r12;
        frame.r11 = self.r11;
        frame.r10 = self.r10;
        frame.r9 = self.r9;
        frame.r8 = self.r8;
        frame.rdi = self.rdi;
        frame.rsi = self.rsi;
        frame.rbp = self.rbp;
        frame.rdx = self.rdx;
        frame.rcx = self.rcx;
        frame.rbx = self.rbx;
        frame.rax = self.rax;
        frame.rip = self.rip;
        frame.cs = u64::from(arch::segmentation::USER_CODE_SELECTOR);
        frame.rflags = self.rflags | 0x202;
        frame.rsp = self.rsp;
        frame.ss = u64::from(arch::segmentation::USER_DATA_SELECTOR);
    }
}

#[derive(Clone, Copy)]
struct PendingReceive {
    endpoint: u8,
    user_buffer: u64,
}

struct ManagedProcess {
    pid: ProcessId,
    tid: ThreadId,
    address_space: AddressSpace,
    capabilities: [CapabilityEntry; MAX_CAPABILITIES],
    initramfs: BootInitramfs,
    context: UserContext,
    pending_receive: Option<PendingReceive>,
    exited: bool,
    exit_reason: ExitReason,
    expected_status: i32,
    expected_exception: u16,
}

impl ManagedProcess {
    fn resolve(
        &self,
        handle: Handle,
        kind: CapabilityKind,
        rights: Rights,
    ) -> Result<CapabilityEntry, i64> {
        let Some(entry) = self.capabilities.get(handle.0 as usize).copied() else {
            return Err(status::BAD_HANDLE);
        };
        if entry.kind == CapabilityKind::Empty {
            return Err(status::BAD_HANDLE);
        }
        if entry.kind != kind || !entry.rights.contains(rights) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(entry)
    }

    fn free_capability_slot(&self) -> Option<usize> {
        (1..MAX_CAPABILITIES).find(|index| self.capabilities[*index].kind == CapabilityKind::Empty)
    }

    fn resolve_endpoint(&self, handle: Handle, rights: Rights) -> Result<u8, i64> {
        let Some(entry) = self.capabilities.get(handle.0 as usize).copied() else {
            return Err(status::BAD_HANDLE);
        };
        let CapabilityKind::Endpoint(endpoint) = entry.kind else {
            return Err(if entry.kind == CapabilityKind::Empty {
                status::BAD_HANDLE
            } else {
                status::ACCESS_DENIED
            });
        };
        if !entry.rights.contains(rights) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(endpoint)
    }
}

#[derive(Clone, Copy)]
struct Endpoint {
    receiver: ProcessId,
    queue: EndpointQueue<ENDPOINT_QUEUE_CAPACITY>,
}

impl Endpoint {
    const EMPTY: Self = Self {
        receiver: ProcessId::KERNEL,
        queue: EndpointQueue::new(),
    };
}

struct ProcessManager {
    processes: [Option<ManagedProcess>; MAX_PROCESSES],
    process_table: ProcessTable<MAX_PROCESSES>,
    scheduler: Scheduler<MAX_PROCESSES, 1>,
    endpoints: [Endpoint; MAX_ENDPOINTS],
    current: ThreadId,
    kernel_root: u64,
    initramfs: BootInitramfs,
    tsc_hz: u64,
    timer_ticks: u64,
    context_switches: u64,
    blocked_receives: u64,
    transferred_capabilities: u64,
}

impl ProcessManager {
    const fn empty() -> Self {
        Self {
            processes: [const { None }; MAX_PROCESSES],
            process_table: ProcessTable::new(),
            scheduler: Scheduler::new(),
            endpoints: [Endpoint::EMPTY; MAX_ENDPOINTS],
            current: ThreadId::INVALID,
            kernel_root: 0,
            initramfs: BootInitramfs {
                phys_addr: 0,
                size: 0,
            },
            tsc_hz: 0,
            timer_ticks: 0,
            context_switches: 0,
            blocked_receives: 0,
            transferred_capabilities: 0,
        }
    }

    fn initialize(&mut self, initramfs: BootInitramfs, tsc_hz: u64) {
        self.process_table = ProcessTable::new();
        self.scheduler = Scheduler::new();
        self.initramfs = initramfs;
        self.tsc_hz = tsc_hz;
        self.begin_phase();
    }

    /// Reap сохраняет generation counters; новая фаза очищает только
    /// runtime-объекты и метрики, а не таблицы идентификаторов.
    fn begin_phase(&mut self) {
        self.endpoints = [Endpoint::EMPTY; MAX_ENDPOINTS];
        self.current = ThreadId::INVALID;
        self.kernel_root = arch::read_cr3();
        self.timer_ticks = 0;
        self.context_switches = 0;
        self.blocked_receives = 0;
        self.transferred_capabilities = 0;
    }

    fn spawn(
        &mut self,
        path: &str,
        priority: PriorityClass,
        arguments: [u64; 3],
        expected_status: i32,
        expected_exception: u16,
        capabilities: [CapabilityEntry; MAX_CAPABILITIES],
    ) -> Result<ProcessId, ProcessError> {
        let image =
            fs::initramfs_file(self.initramfs, path).map_err(|_| ProcessError::MissingElf)?;
        let mut address_space =
            AddressSpace::new(self.kernel_root).map_err(|_| ProcessError::AddressSpace)?;
        let loaded = elf::load(&mut address_space, image).map_err(|_| ProcessError::InvalidElf)?;
        let Some(slot) = self.processes.iter().position(Option::is_none) else {
            return Err(ProcessError::AddressSpace);
        };
        let pid = self
            .process_table
            .create(ProcessId::KERNEL)
            .map_err(|_| ProcessError::AddressSpace)?;
        let tid = match self.scheduler.spawn(pid, priority, 1) {
            Ok(tid) => tid,
            Err(_) => {
                // Откатываем PID, если thread table неожиданно заполнена.
                let _ = self.process_table.exit(pid, NO_EXIT);
                let _ = self.process_table.reap(pid);
                return Err(ProcessError::AddressSpace);
            }
        };
        self.processes[slot] = Some(ManagedProcess {
            pid,
            tid,
            address_space,
            capabilities,
            initramfs: self.initramfs,
            context: UserContext::initial(loaded.entry, loaded.stack_pointer, arguments),
            pending_receive: None,
            exited: false,
            exit_reason: NO_EXIT,
            expected_status,
            expected_exception,
        });
        serial::put_str("[process-manager] create pid=0x");
        serial::put_hex(pid.0);
        serial::put_str(" tid=0x");
        serial::put_hex(tid.0);
        serial::put_str(" image=/boot/");
        serial::put_str(path);
        serial::put_str("\n");
        Ok(pid)
    }

    fn run(&mut self) -> Result<(), ProcessError> {
        let first = self
            .scheduler
            .schedule(0)
            .map_err(|_| ProcessError::UnexpectedExit)?
            .ok_or(ProcessError::UnexpectedExit)?;
        self.current = first;
        let first_index = self
            .index_by_tid(first)
            .ok_or(ProcessError::UnexpectedExit)?;
        let first_process = self.processes[first_index]
            .as_ref()
            .ok_or(ProcessError::UnexpectedExit)?;
        let context = first_process.context;
        let root = first_process.address_space.root();

        unsafe { ACTIVE_MANAGER = self };
        apic::start_timer(self.tsc_hz);
        let _result = unsafe {
            arch::traps::enter_user(
                context.rip,
                context.rsp,
                [context.rdi, context.rsi, context.rdx],
                root,
                true,
            )
        };
        apic::stop_timer();
        unsafe { ACTIVE_MANAGER = ptr::null_mut() };
        self.current = ThreadId::INVALID;

        for process in self.processes.iter().flatten() {
            if !process.exited
                || process.exit_reason.exception != process.expected_exception
                || process.exit_reason.status != process.expected_status
            {
                serial::put_str("[process-manager] unexpected pid=0x");
                serial::put_hex(process.pid.0);
                serial::put_str(" status=");
                serial::put_u32(process.exit_reason.status as u32);
                serial::put_str(" exception=");
                serial::put_u32(process.exit_reason.exception as u32);
                serial::put_str("\n");
                return Err(ProcessError::UnexpectedExit);
            }
        }
        Ok(())
    }

    fn handle_trap(&mut self, frame: &mut TrapFrame) -> u64 {
        if frame.vector == u64::from(apic::SPURIOUS_VECTOR) {
            return 0;
        }
        if !frame.is_from_user() {
            serial::put_str("[trap] FATAL preemptive kernel exception\n");
            crate::boot::exit_kernel(0x7e);
        }
        let Some(current_index) = self.index_by_tid(self.current) else {
            crate::boot::exit_kernel(0x7d);
        };
        if let Some(process) = self.processes[current_index].as_mut() {
            process.context.save(frame);
        }

        if frame.vector == u64::from(apic::TIMER_VECTOR) {
            apic::end_of_interrupt();
            self.timer_ticks = self.timer_ticks.saturating_add(1);
            apic::rearm_timer(self.tsc_hz);
            return self.schedule_next(frame);
        }

        if frame.vector == u64::from(syscall::INTERRUPT_VECTOR) {
            return self.handle_syscall(current_index, frame);
        }

        let fault_address = if frame.vector == 14 {
            arch::read_cr2()
        } else {
            frame.rip
        };
        let reason = ExitReason {
            status: status::FAULT as i32,
            exception: frame.vector as u16,
            flags: frame.error_code as u16,
            fault_address,
        };
        self.finish_current(current_index, reason);
        self.schedule_next(frame)
    }

    fn handle_syscall(&mut self, current_index: usize, frame: &mut TrapFrame) -> u64 {
        match frame.rax {
            syscall::number::THREAD_YIELD => {
                frame.rax = status::OK as u64;
                if let Some(process) = self.processes[current_index].as_mut() {
                    process.context.rax = status::OK as u64;
                }
                self.schedule_next(frame)
            }
            syscall::number::PROCESS_EXIT => {
                let reason = ExitReason {
                    status: frame.rdi as i64 as i32,
                    exception: 0,
                    flags: 0,
                    fault_address: 0,
                };
                self.finish_current(current_index, reason);
                self.schedule_next(frame)
            }
            syscall::number::VFS_STAT => {
                let result = self.vfs_stat(
                    current_index,
                    Handle(frame.rdi as u32),
                    frame.rsi,
                    frame.rdx,
                );
                frame.rax = result as u64;
                0
            }
            syscall::number::IPC_SEND => {
                let result = self.ipc_send(current_index, Handle(frame.rdi as u32), frame.rsi);
                frame.rax = result as u64;
                0
            }
            syscall::number::IPC_RECEIVE => {
                match self.ipc_receive(current_index, Handle(frame.rdi as u32), frame.rsi) {
                    ReceiveResult::Return(result) => {
                        frame.rax = result as u64;
                        0
                    }
                    ReceiveResult::Blocked => self.schedule_next(frame),
                }
            }
            _ => {
                frame.rax = status::NOT_SUPPORTED as u64;
                0
            }
        }
    }

    fn finish_current(&mut self, index: usize, reason: ExitReason) {
        let (pid, tid) = {
            let process = self.processes[index]
                .as_mut()
                .expect("occupied process slot");
            process.exited = true;
            process.exit_reason = reason;
            (process.pid, process.tid)
        };
        let _ = self.scheduler.exit(tid, reason);
        let _ = self.process_table.exit(pid, reason);
        serial::put_str("[process-manager] exit pid=0x");
        serial::put_hex(pid.0);
        serial::put_str(" status=");
        serial::put_u32(reason.status as u32);
        serial::put_str(" exception=");
        serial::put_u32(reason.exception as u32);
        serial::put_str("\n");
    }

    fn schedule_next(&mut self, frame: &mut TrapFrame) -> u64 {
        let previous = self.current;
        let next = match self.scheduler.schedule(0) {
            Ok(Some(next)) => next,
            _ => {
                apic::stop_timer();
                arch::traps::set_user_result(0);
                return 1;
            }
        };
        let Some(index) = self.index_by_tid(next) else {
            arch::traps::set_user_result(status::DEADLOCK as u64);
            return 1;
        };
        let process = self.processes[index]
            .as_ref()
            .expect("scheduled occupied process");
        process.context.restore(frame);
        unsafe { arch::write_cr3(process.address_space.root()) };
        self.current = next;
        if previous != next {
            self.context_switches = self.context_switches.saturating_add(1);
        }
        0
    }

    fn vfs_stat(&self, index: usize, handle: Handle, path: u64, length: u64) -> i64 {
        let process = self.processes[index]
            .as_ref()
            .expect("occupied process slot");
        if let Err(error) = process.resolve(handle, CapabilityKind::VfsRoot, Rights::READ) {
            return error;
        }
        let Ok(length) = usize::try_from(length) else {
            return status::INVALID_ARGUMENT;
        };
        if length == 0
            || length > 95
            || !process
                .address_space
                .contains_user_range(path, length, false)
        {
            return status::INVALID_ARGUMENT;
        }
        let mut buffer = [0u8; 96];
        if process
            .address_space
            .copy_from_user(path, &mut buffer[..length])
            .is_err()
        {
            return status::INVALID_ARGUMENT;
        }
        let Ok(path) = str::from_utf8(&buffer[..length]) else {
            return status::INVALID_ARGUMENT;
        };
        match fs::initramfs_file(process.initramfs, path) {
            Ok(file) => i64::try_from(file.len()).unwrap_or(i64::MAX),
            Err(_) => status::NOT_FOUND,
        }
    }

    fn ipc_receive(&mut self, index: usize, handle: Handle, user_buffer: u64) -> ReceiveResult {
        let endpoint_id = {
            let process = self.processes[index]
                .as_ref()
                .expect("occupied process slot");
            if !process
                .address_space
                .contains_user_range(user_buffer, size_of::<Message>(), true)
            {
                return ReceiveResult::Return(status::INVALID_ARGUMENT);
            }
            match process.resolve_endpoint(handle, Rights::RECEIVE) {
                Ok(endpoint) => endpoint,
                Err(error) => return ReceiveResult::Return(error),
            }
        };
        let endpoint_index = endpoint_id as usize;
        if endpoint_index >= MAX_ENDPOINTS {
            return ReceiveResult::Return(status::BAD_HANDLE);
        }
        if let Some(message) = self.endpoints[endpoint_index].queue.pop() {
            let bytes = message_bytes(&message);
            let process = self.processes[index]
                .as_ref()
                .expect("occupied process slot");
            if process
                .address_space
                .copy_to_user(user_buffer, bytes)
                .is_err()
            {
                return ReceiveResult::Return(status::INVALID_ARGUMENT);
            }
            return ReceiveResult::Return(status::OK);
        }
        let tid = self.processes[index]
            .as_ref()
            .expect("occupied process slot")
            .tid;
        if self.scheduler.block(tid).is_err() {
            return ReceiveResult::Return(status::INVALID_ARGUMENT);
        }
        self.processes[index]
            .as_mut()
            .expect("occupied process slot")
            .pending_receive = Some(PendingReceive {
            endpoint: endpoint_id,
            user_buffer,
        });
        self.blocked_receives = self.blocked_receives.saturating_add(1);
        ReceiveResult::Blocked
    }

    fn ipc_send(&mut self, sender_index: usize, handle: Handle, user_message: u64) -> i64 {
        let (endpoint_id, sender_pid) = {
            let sender = self.processes[sender_index]
                .as_ref()
                .expect("occupied process slot");
            if !sender
                .address_space
                .contains_user_range(user_message, size_of::<Message>(), false)
            {
                return status::INVALID_ARGUMENT;
            }
            let endpoint_id = match sender.resolve_endpoint(handle, Rights::SEND) {
                Ok(endpoint) => endpoint,
                Err(error) => return error,
            };
            (endpoint_id, sender.pid)
        };
        let endpoint_index = endpoint_id as usize;
        if endpoint_index >= MAX_ENDPOINTS {
            return status::BAD_HANDLE;
        }
        let mut message = Message::EMPTY;
        let bytes = unsafe {
            slice::from_raw_parts_mut(
                (&mut message as *mut Message).cast::<u8>(),
                size_of::<Message>(),
            )
        };
        if self.processes[sender_index]
            .as_ref()
            .expect("occupied process slot")
            .address_space
            .copy_from_user(user_message, bytes)
            .is_err()
        {
            return status::INVALID_ARGUMENT;
        }
        if prepare_message(sender_pid, &mut message).is_err() {
            return status::INVALID_ARGUMENT;
        }
        let receiver_pid = self.endpoints[endpoint_index].receiver;
        let Some(receiver_index) = self.index_by_pid(receiver_pid) else {
            return status::BAD_HANDLE;
        };
        let pending = self.processes[receiver_index]
            .as_ref()
            .and_then(|process| process.pending_receive);
        if pending
            .filter(|pending| pending.endpoint == endpoint_id)
            .is_none()
            && self.endpoints[endpoint_index].queue.is_full()
        {
            return status::QUEUE_FULL;
        }
        if let Err(error) = self.transfer_handles(sender_index, receiver_index, &mut message) {
            return error;
        }
        if let Some(pending) = pending.filter(|pending| pending.endpoint == endpoint_id) {
            let bytes = message_bytes(&message);
            let receiver = self.processes[receiver_index]
                .as_mut()
                .expect("occupied process slot");
            if receiver
                .address_space
                .copy_to_user(pending.user_buffer, bytes)
                .is_err()
            {
                return status::INVALID_ARGUMENT;
            }
            receiver.pending_receive = None;
            receiver.context.rax = status::OK as u64;
            let _ = self.scheduler.wake(receiver.tid);
        } else if let Err(error) = self.endpoints[endpoint_index].queue.push(message) {
            return match error {
                IpcQueueError::InvalidMessage => status::INVALID_ARGUMENT,
                IpcQueueError::QueueFull => status::QUEUE_FULL,
            };
        }
        status::OK
    }

    fn transfer_handles(
        &mut self,
        sender_index: usize,
        receiver_index: usize,
        message: &mut Message,
    ) -> Result<(), i64> {
        let count = message.header.handle_count as usize;
        let mut destination_slots = [0usize; IPC_MAX_HANDLES];
        let mut entries = [EMPTY_CAPABILITY; IPC_MAX_HANDLES];
        let mut reserved = [false; MAX_CAPABILITIES];

        for item in 0..count {
            let transfer = message.handles[item];
            if transfer.reserved != 0 || transfer.handle == Handle::INVALID {
                return Err(status::INVALID_ARGUMENT);
            }
            let source = self.processes[sender_index]
                .as_ref()
                .expect("occupied process slot")
                .capabilities
                .get(transfer.handle.0 as usize)
                .copied()
                .ok_or(status::BAD_HANDLE)?;
            if source.kind == CapabilityKind::Empty {
                return Err(status::BAD_HANDLE);
            }
            let rights =
                derive_capability_rights(source.rights, transfer.rights).map_err(|error| {
                    match error {
                        CapabilityTransferError::EmptyRights => status::INVALID_ARGUMENT,
                        CapabilityTransferError::MissingTransferRight
                        | CapabilityTransferError::RightsAmplification => status::ACCESS_DENIED,
                    }
                })?;
            let receiver = self.processes[receiver_index]
                .as_ref()
                .expect("occupied process slot");
            let slot = receiver
                .free_capability_slot()
                .filter(|slot| !reserved[*slot])
                .or_else(|| {
                    (1..MAX_CAPABILITIES).find(|slot| {
                        receiver.capabilities[*slot].kind == CapabilityKind::Empty
                            && !reserved[*slot]
                    })
                })
                .ok_or(status::QUEUE_FULL)?;
            reserved[slot] = true;
            destination_slots[item] = slot;
            entries[item] = CapabilityEntry {
                kind: source.kind,
                rights,
            };
        }

        let receiver = self.processes[receiver_index]
            .as_mut()
            .expect("occupied process slot");
        for item in 0..count {
            let slot = destination_slots[item];
            receiver.capabilities[slot] = entries[item];
            message.handles[item].handle = Handle(slot as u32);
            self.transferred_capabilities = self.transferred_capabilities.saturating_add(1);
        }
        Ok(())
    }

    fn index_by_tid(&self, tid: ThreadId) -> Option<usize> {
        self.processes
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|process| process.tid == tid))
    }

    fn index_by_pid(&self, pid: ProcessId) -> Option<usize> {
        self.processes
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|process| process.pid == pid))
    }

    fn cleanup(&mut self) {
        for slot in &mut self.processes {
            if let Some(process) = slot.take() {
                let _ = self.scheduler.reap(process.tid);
                let _ = self.process_table.reap(process.pid);
                drop(process);
            }
        }
    }
}

enum ReceiveResult {
    Return(i64),
    Blocked,
}

static mut MANAGER: ProcessManager = ProcessManager::empty();
static mut ACTIVE_MANAGER: *mut ProcessManager = ptr::null_mut();

pub(super) fn handle_active_trap(frame: &mut TrapFrame) -> Option<u64> {
    let manager = unsafe { ACTIVE_MANAGER.as_mut() }?;
    Some(manager.handle_trap(frame))
}

pub(super) fn run_milestone(info: &rustos_abi::BootInfo) -> Result<(), ProcessError> {
    let apic = arch::apic::initialize_local().map_err(|error| {
        serial::put_str("[apic] initialization failed: ");
        serial::put_str(match error {
            arch::apic::ApicError::MissingApic => "APIC unavailable",
            arch::apic::ApicError::UnsupportedMmioBase => "unsupported xAPIC MMIO base",
        });
        serial::put_str("\n");
        ProcessError::UnexpectedExit
    })?;
    serial::put_str("[apic] local APIC mode=");
    serial::put_str(if apic.uses_x2apic { "x2APIC" } else { "xAPIC" });
    serial::put_str(" BSP id=");
    serial::put_u32(apic.id);
    serial::put_str(" TSC MHz=");
    serial::put_u32((apic.tsc_hz / 1_000_000) as u32);
    serial::put_str(if apic.uses_tsc_deadline {
        " timer=tsc-deadline"
    } else {
        " timer=periodic"
    });
    serial::put_str("\n");
    let smp =
        arch::smp::start_application_processors(info.acpi_rsdp, apic.tsc_hz).map_err(|_| {
            serial::put_str("[smp] AP startup failed\n");
            ProcessError::UnexpectedExit
        })?;
    serial::put_str("[smp] MADT discovered=");
    serial::put_u32(smp.discovered_cpus as u32);
    serial::put_str(" online=");
    serial::put_u32(smp.online_cpus as u32);
    serial::put_str(" APs parked safely\n");

    let free_before = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    let manager = unsafe { &mut *ptr::addr_of_mut!(MANAGER) };
    manager.initialize(info.initramfs, apic.tsc_hz);
    manager.spawn(
        "system/bin/preempt-a.elf",
        PriorityClass::Interactive,
        [0, syscall::ABI_VERSION, 0],
        21,
        0,
        [EMPTY_CAPABILITY; MAX_CAPABILITIES],
    )?;
    manager.spawn(
        "system/bin/preempt-b.elf",
        PriorityClass::Interactive,
        [0, syscall::ABI_VERSION, 0],
        22,
        0,
        [EMPTY_CAPABILITY; MAX_CAPABILITIES],
    )?;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    if manager.timer_ticks < 2 || manager.context_switches < 2 {
        manager.cleanup();
        return Err(ProcessError::UnexpectedExit);
    }
    serial::put_str("[preempt] APIC timer ticks=");
    serial::put_u32(manager.timer_ticks as u32);
    serial::put_str(" context-switches=");
    serial::put_u32(manager.context_switches as u32);
    serial::put_str("\n");
    manager.cleanup();

    manager.begin_phase();
    manager.spawn(
        "system/bin/fault-test.elf",
        PriorityClass::Interactive,
        [0, syscall::ABI_VERSION, 0],
        status::FAULT as i32,
        6,
        [EMPTY_CAPABILITY; MAX_CAPABILITIES],
    )?;
    manager.spawn(
        "system/bin/preempt-b.elf",
        PriorityClass::Interactive,
        [0, syscall::ABI_VERSION, 0],
        22,
        0,
        [EMPTY_CAPABILITY; MAX_CAPABILITIES],
    )?;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    serial::put_str("[isolation] concurrent #UD terminated one process; survivor exited=22\n");
    manager.cleanup();

    manager.begin_phase();
    let endpoint_id = 0u8;
    let mut receiver_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    receiver_caps[ENDPOINT_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(endpoint_id),
        rights: Rights::RECEIVE,
    };
    let receiver = manager.spawn(
        "system/bin/ipc-receiver.elf",
        PriorityClass::System,
        [ENDPOINT_SLOT as u64, syscall::ABI_VERSION, 0],
        0,
        0,
        receiver_caps,
    )?;
    manager.endpoints[endpoint_id as usize].receiver = receiver;

    let mut sender_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    sender_caps[VFS_ROOT_SLOT] = CapabilityEntry {
        kind: CapabilityKind::VfsRoot,
        rights: Rights::READ.union(Rights::TRANSFER),
    };
    sender_caps[ENDPOINT_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(endpoint_id),
        rights: Rights::SEND,
    };
    manager.spawn(
        "system/bin/ipc-sender.elf",
        PriorityClass::System,
        [
            ENDPOINT_SLOT as u64,
            VFS_ROOT_SLOT as u64,
            syscall::ABI_VERSION,
        ],
        0,
        0,
        sender_caps,
    )?;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    if manager.blocked_receives != 1 || manager.transferred_capabilities != 1 {
        manager.cleanup();
        return Err(ProcessError::UnexpectedExit);
    }
    serial::put_str("[ipc] queued block/wake and attenuated VFS capability verified\n");
    manager.cleanup();

    let free_after = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    if free_after != free_before {
        return Err(ProcessError::FrameLeak);
    }
    serial::put_str("[process-manager] dynamic create/exit/reap reclaimed all frames\n");
    Ok(())
}

fn message_bytes(message: &Message) -> &[u8] {
    unsafe {
        slice::from_raw_parts(
            (message as *const Message).cast::<u8>(),
            size_of::<Message>(),
        )
    }
}

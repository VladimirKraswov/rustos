//! CPU0 process manager ABI v4: процессы, несколько потоков, VM, shared
//! memory, capability IPC и монотонные часы.
//!
//! Здесь сознательно используются ограниченные статические таблицы: раннее
//! микроядро не должно зависеть от собственного heap allocator. Лимиты
//! являются политикой текущей bootstrap-сборки, а PID/TID/ABI от них не
//! зависят; позднее таблицы переедут в pageable kernel slabs.

use core::{
    mem::{size_of, MaybeUninit},
    ptr, slice, str,
};

use rustos_abi::{
    block::{BlockIoRequest, BLOCK_ABI_VERSION},
    bootinfo::BootInitramfs,
    ipc::{Message, IPC_MAX_HANDLES},
    memory::{SharedMemoryCreate, SharedMemoryMap, VmFlags, VmMapRequest, MEMORY_ABI_VERSION},
    process::{
        ProcessSpawnRequest, ProcessSpawnResult, ProcessStartInfo, SpawnCapability,
        ThreadCreateRequest, ThreadCreateResult, PROCESS_ABI_VERSION,
        PROCESS_SPAWN_MAX_CAPABILITIES, PROCESS_START_INFO_ADDRESS,
    },
    syscall::{self, status},
    ExitReason, Handle, PriorityClass, ProcessId, Rights, ThreadId, PAGE_SIZE,
};
use rustos_microkernel::{
    derive_capability_rights, prepare_message, CapabilityTransferError, EndpointQueue,
    IpcQueueError, ProcessTable, Scheduler, ThreadState,
};

use crate::{
    arch::{self, TrapFrame, TrapKind, UserContext},
    block, fs,
    memory::{self, AddressSpace, FrameBlock, UserPageBacking, UserPageFlags},
    serial,
};

use super::{
    elf, CapabilityEntry, CapabilityKind, ProcessError, EMPTY_CAPABILITY, MAX_CAPABILITIES,
    VFS_ROOT_SLOT,
};

const MAX_PROCESSES: usize = 12;
const MAX_THREADS: usize = 24;
const MAX_ENDPOINTS: usize = 4;
const ENDPOINT_QUEUE_CAPACITY: usize = 8;
const ENDPOINT_SLOT: usize = 2;
const MAX_SHARED_OBJECTS: usize = 8;
const MAX_SHARED_PAGES: usize = 64;
const MAX_VM_SYSCALL_PAGES: u64 = 256;
const MAX_PATH_BYTES: usize = 255;
const MAX_ARGUMENT_BYTES: usize = 2048;
const MAX_ENVIRONMENT_BYTES: usize = 2048;

const NO_EXIT: ExitReason = ExitReason {
    status: 0,
    exception: 0,
    flags: 0,
    fault_address: 0,
};

#[derive(Clone, Copy)]
struct ExpectedExit {
    status: i32,
    exception: u16,
}

#[derive(Clone, Copy)]
struct PendingReceive {
    endpoint: u8,
    user_buffer: u64,
}

#[derive(Clone, Copy)]
struct PendingWait {
    target: ProcessId,
    user_reason: u64,
}

#[derive(Clone, Copy)]
struct PendingJoin {
    target: ThreadId,
    user_reason: u64,
}

#[derive(Clone, Copy)]
enum PendingOperation {
    None,
    Receive(PendingReceive),
    ProcessWait(PendingWait),
    ThreadJoin(PendingJoin),
}

struct ManagedThread {
    tid: ThreadId,
    pid: ProcessId,
    context: UserContext,
    pending: PendingOperation,
    exited: bool,
    exit_reason: ExitReason,
}

struct ManagedProcess {
    pid: ProcessId,
    address_space: AddressSpace,
    capabilities: [CapabilityEntry; MAX_CAPABILITIES],
    initramfs: BootInitramfs,
    exited: bool,
    exit_reason: ExitReason,
    expected: Option<ExpectedExit>,
}

impl ManagedProcess {
    fn capability(&self, handle: Handle, rights: Rights) -> Result<CapabilityEntry, i64> {
        let Some(entry) = self.capabilities.get(handle.0 as usize).copied() else {
            return Err(status::BAD_HANDLE);
        };
        if entry.kind == CapabilityKind::Empty {
            return Err(status::BAD_HANDLE);
        }
        if !entry.rights.contains(rights) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(entry)
    }

    fn resolve(&self, handle: Handle, kind: CapabilityKind, rights: Rights) -> Result<(), i64> {
        let entry = self.capability(handle, rights)?;
        if entry.kind != kind {
            return Err(status::ACCESS_DENIED);
        }
        Ok(())
    }

    fn resolve_endpoint(&self, handle: Handle, rights: Rights) -> Result<u8, i64> {
        let entry = self.capability(handle, rights)?;
        let CapabilityKind::Endpoint(endpoint) = entry.kind else {
            return Err(status::ACCESS_DENIED);
        };
        Ok(endpoint)
    }

    fn free_capability_slot(&self) -> Option<usize> {
        (1..MAX_CAPABILITIES).find(|slot| self.capabilities[*slot].kind == CapabilityKind::Empty)
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

const EMPTY_FRAME: FrameBlock = FrameBlock { phys: 0, frames: 0 };

#[derive(Clone, Copy)]
struct SharedMemoryObject {
    generation: u8,
    used: bool,
    frames: [FrameBlock; MAX_SHARED_PAGES],
    pages: usize,
    capability_refs: usize,
    mapping_refs: usize,
    maximum_flags: VmFlags,
}

impl SharedMemoryObject {
    const EMPTY: Self = Self {
        generation: 1,
        used: false,
        frames: [EMPTY_FRAME; MAX_SHARED_PAGES],
        pages: 0,
        capability_refs: 0,
        mapping_refs: 0,
        maximum_flags: VmFlags(0),
    };
}

struct SharedMemoryPool {
    objects: [SharedMemoryObject; MAX_SHARED_OBJECTS],
}

impl SharedMemoryPool {
    const fn new() -> Self {
        Self {
            objects: [SharedMemoryObject::EMPTY; MAX_SHARED_OBJECTS],
        }
    }

    fn create(&mut self, pages: usize, flags: VmFlags) -> Result<u16, i64> {
        if pages == 0 || pages > MAX_SHARED_PAGES {
            return Err(status::LIMIT_REACHED);
        }
        let Some(index) = self.objects.iter().position(|object| !object.used) else {
            return Err(status::LIMIT_REACHED);
        };
        let generation = self.objects[index].generation;
        let mut frames = [EMPTY_FRAME; MAX_SHARED_PAGES];
        for slot in frames.iter_mut().take(pages) {
            match memory::allocate(1, 1) {
                Ok(block) => {
                    unsafe { (block.phys as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
                    *slot = block;
                }
                Err(_) => {
                    for allocated in frames.iter().take_while(|frame| frame.frames != 0) {
                        let _ = memory::free(*allocated);
                    }
                    return Err(status::OUT_OF_MEMORY);
                }
            }
        }
        self.objects[index] = SharedMemoryObject {
            generation,
            used: true,
            frames,
            pages,
            capability_refs: 1,
            mapping_refs: 0,
            maximum_flags: flags,
        };
        Ok(shared_id(index, generation))
    }

    fn get(&self, id: u16) -> Result<&SharedMemoryObject, i64> {
        let index = shared_index(id);
        let Some(object) = self.objects.get(index) else {
            return Err(status::BAD_HANDLE);
        };
        if !object.used || object.generation != shared_generation(id) {
            return Err(status::BAD_HANDLE);
        }
        Ok(object)
    }

    fn retain_capability(&mut self, id: u16) -> Result<(), i64> {
        let index = shared_index(id);
        let object = self.objects.get_mut(index).ok_or(status::BAD_HANDLE)?;
        if !object.used || object.generation != shared_generation(id) {
            return Err(status::BAD_HANDLE);
        }
        object.capability_refs = object.capability_refs.saturating_add(1);
        Ok(())
    }

    /// Однонаправленный переход `RW -> R/RX`. Пока объект writable, его
    /// нельзя исполнять; после seal физические кадры уже нельзя менять ни
    /// через один capability. Это сохраняет W^X при совместном RX mapping.
    fn seal(&mut self, id: u16, flags: VmFlags) -> Result<(), i64> {
        let index = shared_index(id);
        let object = self.objects.get_mut(index).ok_or(status::BAD_HANDLE)?;
        if !object.used || object.generation != shared_generation(id) {
            return Err(status::BAD_HANDLE);
        }
        if object.capability_refs != 1 || object.mapping_refs != 0 {
            return Err(status::BUSY);
        }
        if !object.maximum_flags.contains(VmFlags::WRITE)
            || !flags.contains(VmFlags::READ)
            || flags.contains(VmFlags::WRITE)
        {
            return Err(status::INVALID_ARGUMENT);
        }
        object.maximum_flags = flags;
        Ok(())
    }

    fn release_capability(&mut self, id: u16) {
        let index = shared_index(id);
        let Some(object) = self.objects.get_mut(index) else {
            return;
        };
        if object.used && object.generation == shared_generation(id) {
            object.capability_refs = object.capability_refs.saturating_sub(1);
            self.destroy_if_unused(index);
        }
    }

    fn retain_mapping(&mut self, id: u16) -> Result<(), i64> {
        let index = shared_index(id);
        let object = self.objects.get_mut(index).ok_or(status::BAD_HANDLE)?;
        if !object.used || object.generation != shared_generation(id) {
            return Err(status::BAD_HANDLE);
        }
        object.mapping_refs = object.mapping_refs.saturating_add(1);
        Ok(())
    }

    fn release_mappings(&mut self, id: u16, count: usize) {
        let index = shared_index(id);
        let Some(object) = self.objects.get_mut(index) else {
            return;
        };
        if object.used && object.generation == shared_generation(id) {
            object.mapping_refs = object.mapping_refs.saturating_sub(count);
            self.destroy_if_unused(index);
        }
    }

    fn destroy_if_unused(&mut self, index: usize) {
        let object = &mut self.objects[index];
        if !object.used || object.capability_refs != 0 || object.mapping_refs != 0 {
            return;
        }
        for frame in object.frames.iter().take(object.pages) {
            let _ = memory::free(*frame);
        }
        let generation = next_u8_generation(object.generation);
        *object = SharedMemoryObject::EMPTY;
        object.generation = generation;
    }

    fn cleanup(&mut self) {
        for index in 0..MAX_SHARED_OBJECTS {
            if self.objects[index].used {
                self.objects[index].capability_refs = 0;
                self.objects[index].mapping_refs = 0;
                self.destroy_if_unused(index);
            }
        }
    }
}

struct SpawnData {
    arguments: [u8; MAX_ARGUMENT_BYTES],
    argument_length: usize,
    argument_count: u32,
    environment: [u8; MAX_ENVIRONMENT_BYTES],
    environment_length: usize,
    environment_count: u32,
}

#[derive(Clone, Copy)]
struct SpawnOptions {
    parent: ProcessId,
    priority: PriorityClass,
    boot_arguments: [u64; 3],
    expected: Option<ExpectedExit>,
}

struct ProcessManager {
    processes: [Option<ManagedProcess>; MAX_PROCESSES],
    threads: [Option<ManagedThread>; MAX_THREADS],
    process_table: ProcessTable<MAX_PROCESSES>,
    scheduler: Scheduler<MAX_THREADS, 1>,
    endpoints: [Endpoint; MAX_ENDPOINTS],
    shared: SharedMemoryPool,
    current: ThreadId,
    kernel_root: u64,
    initramfs: BootInitramfs,
    counter_hz: u64,
    timer_ticks: u64,
    context_switches: u64,
    blocked_receives: u64,
    transferred_capabilities: u64,
    deferred_process_reap: Option<ProcessId>,
    deferred_thread_reap: Option<ThreadId>,
}

impl ProcessManager {
    const fn empty() -> Self {
        Self {
            processes: [const { None }; MAX_PROCESSES],
            threads: [const { None }; MAX_THREADS],
            process_table: ProcessTable::new(),
            scheduler: Scheduler::new(),
            endpoints: [Endpoint::EMPTY; MAX_ENDPOINTS],
            shared: SharedMemoryPool::new(),
            current: ThreadId::INVALID,
            kernel_root: 0,
            initramfs: BootInitramfs {
                phys_addr: 0,
                size: 0,
            },
            counter_hz: 0,
            timer_ticks: 0,
            context_switches: 0,
            blocked_receives: 0,
            transferred_capabilities: 0,
            deferred_process_reap: None,
            deferred_thread_reap: None,
        }
    }

    fn initialize(&mut self, initramfs: BootInitramfs, counter_hz: u64) {
        self.process_table = ProcessTable::new();
        self.scheduler = Scheduler::new();
        self.shared = SharedMemoryPool::new();
        self.initramfs = initramfs;
        self.counter_hz = counter_hz;
        self.begin_phase();
    }

    fn begin_phase(&mut self) {
        self.endpoints = [Endpoint::EMPTY; MAX_ENDPOINTS];
        self.current = ThreadId::INVALID;
        self.kernel_root = arch::current_address_space_root();
        self.timer_ticks = 0;
        self.context_switches = 0;
        self.blocked_receives = 0;
        self.transferred_capabilities = 0;
        self.deferred_process_reap = None;
        self.deferred_thread_reap = None;
    }

    /// Kernel bootstrap helper. Динамический syscall использует тот же loader
    /// через `spawn_internal`, но передаёт реального parent PID и start info.
    fn spawn(
        &mut self,
        path: &str,
        priority: PriorityClass,
        arguments: [u64; 3],
        expected_status: i32,
        expected_exception: u16,
        capabilities: [CapabilityEntry; MAX_CAPABILITIES],
    ) -> Result<ProcessId, ProcessError> {
        self.spawn_internal(
            path,
            SpawnOptions {
                parent: ProcessId::KERNEL,
                priority,
                boot_arguments: arguments,
                expected: Some(ExpectedExit {
                    status: expected_status,
                    exception: expected_exception,
                }),
            },
            capabilities,
            None,
        )
    }

    fn spawn_internal(
        &mut self,
        path: &str,
        options: SpawnOptions,
        capabilities: [CapabilityEntry; MAX_CAPABILITIES],
        start: Option<&SpawnData>,
    ) -> Result<ProcessId, ProcessError> {
        let image = fs::initramfs_file(self.initramfs, path).map_err(|_| {
            serial::put_str("[process-manager] spawn failed: image not found\n");
            ProcessError::MissingElf
        })?;
        let process_slot = self
            .processes
            .iter()
            .position(Option::is_none)
            .ok_or(ProcessError::AddressSpace)?;
        let thread_slot = self
            .threads
            .iter()
            .position(Option::is_none)
            .ok_or(ProcessError::AddressSpace)?;
        let mut address_space = AddressSpace::new(self.kernel_root).map_err(|error| {
            serial::put_str("[process-manager] spawn failed: address-space root\n");
            serial::put_str("[process-manager] address-space error=");
            serial::put_str(match error {
                crate::memory::AddressSpaceError::OutOfMemory => "out-of-memory",
                crate::memory::AddressSpaceError::TooManyMappings => "metadata-limit",
                _ => "invalid-root",
            });
            match memory::stats() {
                Ok(stats) => {
                    serial::put_str(" free-frames=0x");
                    serial::put_hex(stats.free_frames);
                    serial::put_str(" extents=");
                    serial::put_u32(stats.extents as u32);
                }
                Err(error) => {
                    serial::put_str(" allocator=");
                    serial::put_str(frame_error_name(error));
                }
            }
            serial::put_str("\n");
            ProcessError::AddressSpace
        })?;
        let loaded = elf::load(&mut address_space, image).map_err(|_| {
            serial::put_str("[process-manager] spawn failed: ELF mapping\n");
            ProcessError::InvalidElf
        })?;
        let pid = self
            .process_table
            .create(options.parent)
            .map_err(|_| ProcessError::AddressSpace)?;
        let tid = match self.scheduler.spawn(pid, options.priority, 1) {
            Ok(tid) => tid,
            Err(_) => {
                let _ = self.process_table.exit(pid, NO_EXIT);
                let _ = self.process_table.reap(pid);
                return Err(ProcessError::AddressSpace);
            }
        };
        let arguments = if let Some(start) = start {
            if self
                .install_start_info(&mut address_space, pid, tid, start)
                .is_err()
            {
                serial::put_str("[process-manager] spawn failed: start-info mapping\n");
                let _ = self.scheduler.exit(tid, NO_EXIT);
                let _ = self.scheduler.reap(tid);
                let _ = self.process_table.exit(pid, NO_EXIT);
                let _ = self.process_table.reap(pid);
                return Err(ProcessError::AddressSpace);
            }
            [PROCESS_START_INFO_ADDRESS, syscall::ABI_VERSION, 0]
        } else {
            options.boot_arguments
        };
        self.processes[process_slot] = Some(ManagedProcess {
            pid,
            address_space,
            capabilities,
            initramfs: self.initramfs,
            exited: false,
            exit_reason: NO_EXIT,
            expected: options.expected,
        });
        self.threads[thread_slot] = Some(ManagedThread {
            tid,
            pid,
            context: UserContext::initial(loaded.entry, loaded.stack_pointer, arguments),
            pending: PendingOperation::None,
            exited: false,
            exit_reason: NO_EXIT,
        });
        for entry in capabilities {
            self.retain_capability(entry);
        }
        serial::put_str("[process-manager] create pid=0x");
        serial::put_hex(pid.0);
        serial::put_str(" tid=0x");
        serial::put_hex(tid.0);
        serial::put_str(" image=/boot/");
        serial::put_str(path);
        serial::put_str("\n");
        Ok(pid)
    }

    fn install_start_info(
        &mut self,
        address_space: &mut AddressSpace,
        pid: ProcessId,
        tid: ThreadId,
        start: &SpawnData,
    ) -> Result<(), ()> {
        let header_size = size_of::<ProcessStartInfo>();
        let total = header_size
            .checked_add(start.argument_length)
            .and_then(|value| value.checked_add(start.environment_length))
            .ok_or(())?;
        let pages = (total as u64).div_ceil(PAGE_SIZE);
        for page in 0..pages {
            address_space
                .map_page(
                    PROCESS_START_INFO_ADDRESS + page * PAGE_SIZE,
                    UserPageFlags::READ_WRITE,
                )
                .map_err(|_| ())?;
        }
        let arguments_address = PROCESS_START_INFO_ADDRESS + header_size as u64;
        let environment_address = arguments_address + start.argument_length as u64;
        let info = ProcessStartInfo {
            version: PROCESS_ABI_VERSION,
            size: header_size as u32,
            pid,
            tid,
            page_size: PAGE_SIZE,
            monotonic_hz: self.counter_hz,
            arguments_address,
            arguments_length: start.argument_length as u32,
            argument_count: start.argument_count,
            environment_address,
            environment_length: start.environment_length as u32,
            environment_count: start.environment_count,
        };
        address_space
            .copy_into_user(PROCESS_START_INFO_ADDRESS, bytes_of(&info))
            .map_err(|_| ())?;
        address_space
            .copy_into_user(arguments_address, &start.arguments[..start.argument_length])
            .map_err(|_| ())?;
        address_space
            .copy_into_user(
                environment_address,
                &start.environment[..start.environment_length],
            )
            .map_err(|_| ())?;
        for page in 0..pages {
            address_space
                .protect_page(
                    PROCESS_START_INFO_ADDRESS + page * PAGE_SIZE,
                    UserPageFlags {
                        writable: false,
                        executable: false,
                    },
                )
                .map_err(|_| ())?;
        }
        Ok(())
    }

    fn run(&mut self) -> Result<(), ProcessError> {
        let first = self
            .scheduler
            .schedule(0)
            .map_err(|_| ProcessError::UnexpectedExit)?
            .ok_or(ProcessError::UnexpectedExit)?;
        self.current = first;
        let thread_index = self
            .thread_index(first)
            .ok_or(ProcessError::UnexpectedExit)?;
        let thread = self.threads[thread_index]
            .as_ref()
            .ok_or(ProcessError::UnexpectedExit)?;
        let process_index = self
            .process_index(thread.pid)
            .ok_or(ProcessError::UnexpectedExit)?;
        let process = self.processes[process_index]
            .as_ref()
            .ok_or(ProcessError::UnexpectedExit)?;
        let context = thread.context;
        let root = process.address_space.root();

        unsafe { ACTIVE_MANAGER = self };
        arch::set_user_thread_pointer(context.thread_pointer());
        arch::start_scheduler_timer(self.counter_hz);
        let _ = unsafe {
            arch::enter_user(
                context.entry(),
                context.stack_pointer(),
                context.arguments(),
                root,
                true,
            )
        };
        arch::stop_scheduler_timer();
        unsafe { ACTIVE_MANAGER = ptr::null_mut() };
        self.current = ThreadId::INVALID;

        for process in self.processes.iter().flatten() {
            if let Some(expected) = process.expected {
                if !process.exited
                    || process.exit_reason.exception != expected.exception
                    || process.exit_reason.status != expected.status
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
        }
        Ok(())
    }

    fn handle_trap(&mut self, frame: &mut TrapFrame) -> u64 {
        let kind = frame.kind();
        if kind == TrapKind::Spurious {
            return 0;
        }
        if !frame.is_from_user() {
            serial::put_str("[trap] FATAL preemptive kernel exception\n");
            crate::boot::exit_kernel(0x7e);
        }
        let Some(thread_index) = self.thread_index(self.current) else {
            crate::boot::exit_kernel(0x7d);
        };
        self.threads[thread_index]
            .as_mut()
            .expect("current thread")
            .context
            .save(frame);

        if kind == TrapKind::Timer {
            arch::end_of_interrupt();
            self.timer_ticks = self.timer_ticks.saturating_add(1);
            arch::rearm_scheduler_timer(self.counter_hz);
            return self.schedule_next(frame);
        }
        if kind == TrapKind::Syscall {
            return self.handle_syscall(thread_index, frame);
        }
        let TrapKind::Exception {
            number,
            code,
            fault_address,
            ..
        } = kind
        else {
            crate::boot::exit_kernel(0x7c);
        };
        let reason = ExitReason {
            status: status::FAULT as i32,
            exception: number,
            flags: code,
            fault_address,
        };
        let pid = self.threads[thread_index]
            .as_ref()
            .expect("current thread")
            .pid;
        self.finish_process(pid, reason);
        self.schedule_next(frame)
    }

    fn handle_syscall(&mut self, thread_index: usize, frame: &mut TrapFrame) -> u64 {
        let [arg0, arg1, arg2] = frame.syscall_arguments();
        let pid = self.threads[thread_index]
            .as_ref()
            .expect("current thread")
            .pid;
        let process_index = self.process_index(pid).expect("current process");
        match frame.syscall_number() {
            syscall::number::THREAD_YIELD => {
                frame.set_syscall_result(status::OK);
                self.threads[thread_index]
                    .as_mut()
                    .expect("current thread")
                    .context
                    .set_syscall_result(status::OK);
                self.schedule_next(frame)
            }
            syscall::number::PROCESS_EXIT => {
                self.finish_process(pid, normal_exit(arg0));
                self.schedule_next(frame)
            }
            syscall::number::THREAD_EXIT => {
                self.finish_thread(self.current, normal_exit(arg0));
                if !self.has_live_threads(pid) {
                    self.finish_process(pid, normal_exit(arg0));
                }
                self.schedule_next(frame)
            }
            syscall::number::VFS_STAT => {
                let result = self.vfs_stat(process_index, Handle(arg0 as u32), arg1, arg2);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::IPC_SEND => {
                let result = self.ipc_send(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::IPC_RECEIVE => {
                match self.ipc_receive(thread_index, Handle(arg0 as u32), arg1) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::PROCESS_SPAWN => {
                let result = self.process_spawn(process_index, arg0, arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::PROCESS_WAIT => {
                match self.process_wait(thread_index, Handle(arg0 as u32), arg1) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::PROCESS_KILL => {
                let result =
                    self.process_kill(process_index, Handle(arg0 as u32), arg1 as i64 as i32);
                frame.set_syscall_result(result);
                if self.threads[thread_index]
                    .as_ref()
                    .is_some_and(|thread| thread.exited)
                {
                    self.schedule_next(frame)
                } else {
                    0
                }
            }
            syscall::number::THREAD_CREATE => {
                let result = self.thread_create(process_index, arg0, arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::THREAD_JOIN => {
                match self.thread_join(thread_index, Handle(arg0 as u32), arg1) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::THREAD_SET_TLS => {
                let result = self.thread_set_tls(thread_index, arg0);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::VM_MAP => {
                let result = self.vm_map(process_index, arg0);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::VM_UNMAP => {
                let result = self.vm_unmap(process_index, arg0, arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::VM_PROTECT => {
                let result = self.vm_protect(process_index, arg0, arg1, VmFlags(arg2));
                frame.set_syscall_result(result);
                0
            }
            syscall::number::SHARED_MEMORY_CREATE => {
                let result = self.shared_memory_create(process_index, arg0);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::SHARED_MEMORY_MAP => {
                let result = self.shared_memory_map(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::SHARED_MEMORY_SEAL => {
                let result =
                    self.shared_memory_seal(process_index, Handle(arg0 as u32), VmFlags(arg1));
                frame.set_syscall_result(result);
                0
            }
            syscall::number::HANDLE_CLOSE => {
                let result = self.handle_close(process_index, Handle(arg0 as u32));
                frame.set_syscall_result(result);
                0
            }
            syscall::number::CLOCK_MONOTONIC => {
                frame.set_syscall_result(self.monotonic_nanoseconds());
                0
            }
            syscall::number::BLOCK_GET_SIZE => {
                frame.set_syscall_result(self.block_get_size(process_index, Handle(arg0 as u32)));
                0
            }
            syscall::number::BLOCK_READ => {
                frame.set_syscall_result(self.block_io(
                    process_index,
                    Handle(arg0 as u32),
                    arg1,
                    false,
                ));
                0
            }
            syscall::number::BLOCK_WRITE => {
                frame.set_syscall_result(self.block_io(
                    process_index,
                    Handle(arg0 as u32),
                    arg1,
                    true,
                ));
                0
            }
            syscall::number::BLOCK_FLUSH => {
                frame.set_syscall_result(self.block_flush(process_index, Handle(arg0 as u32)));
                0
            }
            _ => {
                frame.set_syscall_result(status::NOT_SUPPORTED);
                0
            }
        }
    }

    fn schedule_next(&mut self, frame: &mut TrapFrame) -> u64 {
        let previous = self.current;
        let next = match self.scheduler.schedule(0) {
            Ok(Some(next)) => next,
            _ => {
                arch::stop_scheduler_timer();
                arch::set_user_run_result(0);
                return 1;
            }
        };
        let Some(thread_index) = self.thread_index(next) else {
            arch::set_user_run_result(status::DEADLOCK as u64);
            return 1;
        };
        let (pid, context) = {
            let thread = self.threads[thread_index]
                .as_ref()
                .expect("scheduled thread");
            (thread.pid, thread.context)
        };
        let process_index = self.process_index(pid).expect("scheduled process");
        let root = self.processes[process_index]
            .as_ref()
            .expect("scheduled process")
            .address_space
            .root();
        context.restore(frame);
        arch::set_user_thread_pointer(context.thread_pointer());
        unsafe { arch::switch_address_space(root) };
        self.current = next;
        if previous != next {
            self.context_switches = self.context_switches.saturating_add(1);
        }
        if let Some(pid) = self.deferred_process_reap.take() {
            self.reap_process(pid);
        }
        if let Some(tid) = self.deferred_thread_reap.take() {
            self.reap_thread(tid);
        }
        0
    }

    fn finish_thread(&mut self, tid: ThreadId, reason: ExitReason) {
        let Some(index) = self.thread_index(tid) else {
            return;
        };
        if self.threads[index]
            .as_ref()
            .is_some_and(|thread| thread.exited)
        {
            return;
        }
        let _ = self.scheduler.exit(tid, reason);
        let thread = self.threads[index].as_mut().expect("thread");
        thread.exited = true;
        thread.exit_reason = reason;
        thread.pending = PendingOperation::None;
        let mut woke_waiter = false;
        for waiter_index in 0..MAX_THREADS {
            let pending = self.threads[waiter_index]
                .as_ref()
                .map(|thread| thread.pending);
            if let Some(PendingOperation::ThreadJoin(wait)) = pending {
                if wait.target == tid {
                    let result =
                        self.write_reason_for_thread(waiter_index, wait.user_reason, reason);
                    let waiter = self.threads[waiter_index].as_mut().expect("join waiter");
                    waiter.pending = PendingOperation::None;
                    waiter.context.set_syscall_result(result);
                    let _ = self.scheduler.wake(waiter.tid);
                    woke_waiter = result == status::OK;
                }
            }
        }
        if woke_waiter {
            self.deferred_thread_reap = Some(tid);
        }
    }

    fn finish_process(&mut self, pid: ProcessId, reason: ExitReason) {
        let Some(process_index) = self.process_index(pid) else {
            return;
        };
        if self.processes[process_index]
            .as_ref()
            .is_some_and(|process| process.exited)
        {
            return;
        }
        for index in 0..MAX_THREADS {
            let should_finish = self.threads[index]
                .as_ref()
                .is_some_and(|thread| thread.pid == pid && !thread.exited);
            if should_finish {
                let tid = self.threads[index].as_ref().expect("thread").tid;
                let _ = self.scheduler.exit(tid, reason);
                let thread = self.threads[index].as_mut().expect("thread");
                thread.exited = true;
                thread.exit_reason = reason;
                thread.pending = PendingOperation::None;
            }
        }
        let process = self.processes[process_index].as_mut().expect("process");
        process.exited = true;
        process.exit_reason = reason;
        let _ = self.process_table.exit(pid, reason);
        let mut woke_waiter = false;
        for waiter_index in 0..MAX_THREADS {
            let pending = self.threads[waiter_index]
                .as_ref()
                .map(|thread| thread.pending);
            if let Some(PendingOperation::ProcessWait(wait)) = pending {
                if wait.target == pid {
                    let result =
                        self.write_reason_for_thread(waiter_index, wait.user_reason, reason);
                    let waiter = self.threads[waiter_index].as_mut().expect("process waiter");
                    waiter.pending = PendingOperation::None;
                    waiter.context.set_syscall_result(result);
                    let _ = self.scheduler.wake(waiter.tid);
                    woke_waiter = result == status::OK;
                }
            }
        }
        if woke_waiter {
            self.deferred_process_reap = Some(pid);
            self.deferred_thread_reap = None;
        }
        serial::put_str("[process-manager] exit pid=0x");
        serial::put_hex(pid.0);
        serial::put_str(" status=");
        serial::put_u32(reason.status as u32);
        serial::put_str(" exception=");
        serial::put_u32(reason.exception as u32);
        serial::put_str("\n");
    }

    fn process_spawn(
        &mut self,
        parent_index: usize,
        request_address: u64,
        result_address: u64,
    ) -> i64 {
        if let Err(error) = memory::stats() {
            serial::put_str("[process-manager] allocator unavailable before spawn: ");
            serial::put_str(frame_error_name(error));
            serial::put_str("\n");
        }
        let request = match self.read_struct::<ProcessSpawnRequest>(parent_index, request_address) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if request.version != PROCESS_ABI_VERSION
            || request.flags != 0
            || request.reserved0 != [0; 3]
            || request.path_length == 0
            || request.path_length as usize > MAX_PATH_BYTES
            || request.capability_count as usize > PROCESS_SPAWN_MAX_CAPABILITIES
            || !self.user_writable(
                parent_index,
                result_address,
                size_of::<ProcessSpawnResult>(),
            )
        {
            return status::INVALID_ARGUMENT;
        }
        let parent = self.processes[parent_index].as_ref().expect("parent");
        if let Err(error) = parent.resolve(
            request.namespace,
            CapabilityKind::VfsRoot,
            Rights::READ.union(Rights::EXECUTE),
        ) {
            return error;
        }
        let Some(priority) = user_priority(request.priority) else {
            return status::ACCESS_DENIED;
        };
        let mut path_bytes = [0u8; MAX_PATH_BYTES];
        if self
            .copy_from_process(
                parent_index,
                request.path_address,
                &mut path_bytes[..request.path_length as usize],
            )
            .is_err()
        {
            return status::INVALID_ARGUMENT;
        }
        let Ok(raw_path) = str::from_utf8(&path_bytes[..request.path_length as usize]) else {
            return status::INVALID_ARGUMENT;
        };
        let path = normalize_boot_path(raw_path);
        if path.is_empty() {
            return status::INVALID_ARGUMENT;
        }

        let mut start = SpawnData {
            arguments: [0; MAX_ARGUMENT_BYTES],
            argument_length: request.arguments_length as usize,
            argument_count: request.argument_count,
            environment: [0; MAX_ENVIRONMENT_BYTES],
            environment_length: request.environment_length as usize,
            environment_count: request.environment_count,
        };
        if start.argument_length > MAX_ARGUMENT_BYTES
            || start.environment_length > MAX_ENVIRONMENT_BYTES
        {
            return status::LIMIT_REACHED;
        }
        if self
            .copy_from_process(
                parent_index,
                request.arguments_address,
                &mut start.arguments[..start.argument_length],
            )
            .is_err()
            || self
                .copy_from_process(
                    parent_index,
                    request.environment_address,
                    &mut start.environment[..start.environment_length],
                )
                .is_err()
            || !valid_string_table(
                &start.arguments[..start.argument_length],
                start.argument_count,
            )
            || !valid_string_table(
                &start.environment[..start.environment_length],
                start.environment_count,
            )
        {
            return status::INVALID_ARGUMENT;
        }

        let mut transfers = [SpawnCapability {
            source: Handle::INVALID,
            target_slot: 0,
            rights: Rights::NONE,
        }; PROCESS_SPAWN_MAX_CAPABILITIES];
        let transfer_count = request.capability_count as usize;
        if transfer_count != 0 {
            let byte_len = transfer_count * size_of::<SpawnCapability>();
            let bytes =
                unsafe { slice::from_raw_parts_mut(transfers.as_mut_ptr().cast::<u8>(), byte_len) };
            if self
                .copy_from_process(parent_index, request.capabilities_address, bytes)
                .is_err()
            {
                return status::INVALID_ARGUMENT;
            }
        }
        let mut child_capabilities = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
        for transfer in transfers.iter().take(transfer_count) {
            let slot = transfer.target_slot as usize;
            if slot == 0
                || slot >= MAX_CAPABILITIES
                || child_capabilities[slot].kind != CapabilityKind::Empty
            {
                return status::INVALID_ARGUMENT;
            }
            let source = match self.processes[parent_index]
                .as_ref()
                .expect("parent")
                .capability(transfer.source, Rights::TRANSFER)
            {
                Ok(source) => source,
                Err(error) => return error,
            };
            let rights = match derive_capability_rights(source.rights, transfer.rights) {
                Ok(rights) => rights,
                Err(CapabilityTransferError::EmptyRights) => return status::INVALID_ARGUMENT,
                Err(_) => return status::ACCESS_DENIED,
            };
            child_capabilities[slot] = CapabilityEntry {
                kind: source.kind,
                rights,
            };
        }
        let Some(parent_capability_slot) = self.processes[parent_index]
            .as_ref()
            .expect("parent")
            .free_capability_slot()
        else {
            return status::LIMIT_REACHED;
        };
        let parent_pid = self.processes[parent_index].as_ref().expect("parent").pid;
        let pid = match self.spawn_internal(
            path,
            SpawnOptions {
                parent: parent_pid,
                priority,
                boot_arguments: [0; 3],
                expected: None,
            },
            child_capabilities,
            Some(&start),
        ) {
            Ok(pid) => pid,
            Err(ProcessError::MissingElf) => return status::NOT_FOUND,
            Err(ProcessError::AddressSpace) => return status::OUT_OF_MEMORY,
            Err(_) => return status::INVALID_ARGUMENT,
        };
        self.processes[parent_index]
            .as_mut()
            .expect("parent")
            .capabilities[parent_capability_slot] = CapabilityEntry {
            kind: CapabilityKind::Process(pid),
            rights: Rights::WAIT.union(Rights::DESTROY).union(Rights::TRANSFER),
        };
        let result = ProcessSpawnResult {
            process: Handle(parent_capability_slot as u32),
            reserved: 0,
            pid,
        };
        if self
            .write_struct(parent_index, result_address, &result)
            .is_err()
        {
            self.finish_process(pid, normal_exit(status::INVALID_ARGUMENT as u64));
            self.reap_process(pid);
            return status::INVALID_ARGUMENT;
        }
        status::OK
    }

    fn process_wait(
        &mut self,
        thread_index: usize,
        handle: Handle,
        user_reason: u64,
    ) -> BlockingResult {
        let pid = self.threads[thread_index].as_ref().expect("waiter").pid;
        let process_index = self.process_index(pid).expect("waiter process");
        if !self.user_writable(process_index, user_reason, size_of::<ExitReason>()) {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let target = match self.processes[process_index]
            .as_ref()
            .expect("waiter process")
            .capability(handle, Rights::WAIT)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::Process(pid),
                ..
            }) => pid,
            Ok(_) => return BlockingResult::Return(status::ACCESS_DENIED),
            Err(error) => return BlockingResult::Return(error),
        };
        if target == pid {
            return BlockingResult::Return(status::BUSY);
        }
        let Some(target_index) = self.process_index(target) else {
            return BlockingResult::Return(status::BAD_HANDLE);
        };
        let target_process = self.processes[target_index].as_ref().expect("target");
        if target_process.exited {
            let reason = target_process.exit_reason;
            let result = self
                .write_struct(process_index, user_reason, &reason)
                .map(|_| status::OK)
                .unwrap_or(status::INVALID_ARGUMENT);
            if result == status::OK {
                self.reap_process(target);
            }
            return BlockingResult::Return(result);
        }
        if self.threads.iter().flatten().any(|thread| matches!(thread.pending, PendingOperation::ProcessWait(wait) if wait.target == target)) {
            return BlockingResult::Return(status::BUSY);
        }
        let tid = self.threads[thread_index].as_ref().expect("waiter").tid;
        if self.scheduler.block(tid).is_err() {
            return BlockingResult::Return(status::BUSY);
        }
        self.threads[thread_index].as_mut().expect("waiter").pending =
            PendingOperation::ProcessWait(PendingWait {
                target,
                user_reason,
            });
        BlockingResult::Blocked
    }

    fn process_kill(&mut self, process_index: usize, handle: Handle, exit_status: i32) -> i64 {
        let target = match self.processes[process_index]
            .as_ref()
            .expect("caller")
            .capability(handle, Rights::DESTROY)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::Process(pid),
                ..
            }) => pid,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        let Some(target_index) = self.process_index(target) else {
            return status::BAD_HANDLE;
        };
        if self.processes[target_index]
            .as_ref()
            .expect("target")
            .exited
        {
            return status::BUSY;
        }
        self.finish_process(target, normal_exit(exit_status as i64 as u64));
        status::OK
    }

    fn thread_create(
        &mut self,
        process_index: usize,
        request_address: u64,
        result_address: u64,
    ) -> i64 {
        let request = match self.read_struct::<ThreadCreateRequest>(process_index, request_address)
        {
            Ok(request) => request,
            Err(error) => return error,
        };
        if request.version != PROCESS_ABI_VERSION
            || request.flags != 0
            || request.reserved != [0; 7]
            || request.stack_pointer == 0
            || !self.user_writable(
                process_index,
                result_address,
                size_of::<ThreadCreateResult>(),
            )
        {
            return status::INVALID_ARGUMENT;
        }
        let Some(priority) = user_priority(request.priority) else {
            return status::ACCESS_DENIED;
        };
        let process = self.processes[process_index].as_ref().expect("process");
        if !process.address_space.is_executable(request.entry)
            || !process.address_space.is_writable(request.stack_pointer - 1)
            || (request.thread_pointer != 0
                && !process
                    .address_space
                    .contains_user_range(request.thread_pointer, 1, false))
        {
            return status::INVALID_ARGUMENT;
        }
        let Some(thread_slot) = self.threads.iter().position(Option::is_none) else {
            return status::LIMIT_REACHED;
        };
        let Some(capability_slot) = process.free_capability_slot() else {
            return status::LIMIT_REACHED;
        };
        let pid = process.pid;
        let tid = match self.scheduler.spawn(pid, priority, 1) {
            Ok(tid) => tid,
            Err(_) => return status::LIMIT_REACHED,
        };
        let mut context = UserContext::initial(
            request.entry,
            request.stack_pointer,
            [request.argument, 0, 0],
        );
        context.set_thread_pointer(request.thread_pointer);
        self.threads[thread_slot] = Some(ManagedThread {
            tid,
            pid,
            context,
            pending: PendingOperation::None,
            exited: false,
            exit_reason: NO_EXIT,
        });
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[capability_slot] = CapabilityEntry {
            kind: CapabilityKind::Thread(tid),
            rights: Rights::WAIT.union(Rights::TRANSFER),
        };
        let result = ThreadCreateResult {
            thread: Handle(capability_slot as u32),
            reserved: 0,
            tid,
        };
        if self
            .write_struct(process_index, result_address, &result)
            .is_err()
        {
            self.processes[process_index]
                .as_mut()
                .expect("process")
                .capabilities[capability_slot] = EMPTY_CAPABILITY;
            let _ = self.scheduler.exit(tid, NO_EXIT);
            let _ = self.scheduler.reap(tid);
            self.threads[thread_slot] = None;
            return status::INVALID_ARGUMENT;
        }
        status::OK
    }

    fn thread_join(
        &mut self,
        waiter_index: usize,
        handle: Handle,
        user_reason: u64,
    ) -> BlockingResult {
        let pid = self.threads[waiter_index].as_ref().expect("waiter").pid;
        let process_index = self.process_index(pid).expect("waiter process");
        if !self.user_writable(process_index, user_reason, size_of::<ExitReason>()) {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let target = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, Rights::WAIT)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::Thread(tid),
                ..
            }) => tid,
            Ok(_) => return BlockingResult::Return(status::ACCESS_DENIED),
            Err(error) => return BlockingResult::Return(error),
        };
        if target == self.current {
            return BlockingResult::Return(status::BUSY);
        }
        let Some(target_index) = self.thread_index(target) else {
            return BlockingResult::Return(status::BAD_HANDLE);
        };
        if self.threads[target_index].as_ref().expect("target").exited {
            let reason = self.threads[target_index]
                .as_ref()
                .expect("target")
                .exit_reason;
            let result = self
                .write_struct(process_index, user_reason, &reason)
                .map(|_| status::OK)
                .unwrap_or(status::INVALID_ARGUMENT);
            if result == status::OK {
                self.reap_thread(target);
            }
            return BlockingResult::Return(result);
        }
        if self.threads.iter().flatten().any(|thread| matches!(thread.pending, PendingOperation::ThreadJoin(wait) if wait.target == target)) {
            return BlockingResult::Return(status::BUSY);
        }
        let waiter_tid = self.threads[waiter_index].as_ref().expect("waiter").tid;
        if self.scheduler.block(waiter_tid).is_err() {
            return BlockingResult::Return(status::BUSY);
        }
        self.threads[waiter_index].as_mut().expect("waiter").pending =
            PendingOperation::ThreadJoin(PendingJoin {
                target,
                user_reason,
            });
        BlockingResult::Blocked
    }

    fn thread_set_tls(&mut self, thread_index: usize, address: u64) -> i64 {
        let pid = self.threads[thread_index].as_ref().expect("thread").pid;
        let process_index = self.process_index(pid).expect("process");
        if address != 0
            && !self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .contains_user_range(address, 1, false)
        {
            return status::INVALID_ARGUMENT;
        }
        self.threads[thread_index]
            .as_mut()
            .expect("thread")
            .context
            .set_thread_pointer(address);
        arch::set_user_thread_pointer(address);
        status::OK
    }

    fn vm_map(&mut self, process_index: usize, request_address: u64) -> i64 {
        let request = match self.read_struct::<VmMapRequest>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return error,
        };
        let flags = match valid_vm_flags(request.flags) {
            Some(flags) => flags,
            None => return status::INVALID_ARGUMENT,
        };
        let pages = match checked_page_count(request.length) {
            Ok(pages) => pages,
            Err(error) => return error,
        };
        let address = {
            let space = &self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space;
            if request.version != MEMORY_ABI_VERSION || request.reserved != 0 {
                return status::INVALID_ARGUMENT;
            }
            if request.address == 0 {
                match space.find_free_range(request.length) {
                    Ok(address) => address,
                    Err(_) => return status::LIMIT_REACHED,
                }
            } else if space.range_is_free(request.address, request.length) {
                request.address
            } else {
                return status::INVALID_ARGUMENT;
            }
        };
        let page_flags = page_flags(flags);
        let mut mapped = 0u64;
        while mapped < pages {
            let result = self.processes[process_index]
                .as_mut()
                .expect("process")
                .address_space
                .map_page(address + mapped * PAGE_SIZE, page_flags);
            if result.is_err() {
                for rollback in 0..mapped {
                    let _ = self.processes[process_index]
                        .as_mut()
                        .expect("process")
                        .address_space
                        .unmap_page(address + rollback * PAGE_SIZE);
                }
                return status::OUT_OF_MEMORY;
            }
            mapped += 1;
        }
        self.flush_process(process_index);
        address as i64
    }

    fn vm_unmap(&mut self, process_index: usize, address: u64, length: u64) -> i64 {
        let pages = match checked_page_count(length) {
            Ok(pages) => pages,
            Err(error) => return error,
        };
        if !address.is_multiple_of(PAGE_SIZE)
            || !self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .contains_user_range(address, length as usize, false)
        {
            return status::INVALID_ARGUMENT;
        }
        for page in 0..pages {
            let backing = self.processes[process_index]
                .as_mut()
                .expect("process")
                .address_space
                .unmap_page(address + page * PAGE_SIZE);
            match backing {
                Ok(UserPageBacking::Shared(object)) => self.shared.release_mappings(object, 1),
                Ok(UserPageBacking::Private) => {}
                Err(_) => return status::INVALID_ARGUMENT,
            }
        }
        self.flush_process(process_index);
        status::OK
    }

    fn vm_protect(
        &mut self,
        process_index: usize,
        address: u64,
        length: u64,
        flags: VmFlags,
    ) -> i64 {
        let page_flags = match valid_vm_flags(flags) {
            Some(flags) => page_flags(flags),
            None => return status::INVALID_ARGUMENT,
        };
        let pages = match checked_page_count(length) {
            Ok(pages) => pages,
            Err(error) => return error,
        };
        if !address.is_multiple_of(PAGE_SIZE)
            || !self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .contains_user_range(address, length as usize, false)
        {
            return status::INVALID_ARGUMENT;
        }
        for page in 0..pages {
            let result = self.processes[process_index]
                .as_mut()
                .expect("process")
                .address_space
                .protect_page(address + page * PAGE_SIZE, page_flags);
            if let Err(crate::memory::AddressSpaceError::AccessDenied) = result {
                return status::ACCESS_DENIED;
            }
            if result.is_err() {
                return status::INVALID_ARGUMENT;
            }
        }
        self.flush_process(process_index);
        status::OK
    }

    fn shared_memory_create(&mut self, process_index: usize, request_address: u64) -> i64 {
        let request = match self.read_struct::<SharedMemoryCreate>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return error,
        };
        let Some(flags) = valid_vm_flags(request.flags) else {
            return status::INVALID_ARGUMENT;
        };
        if request.version != MEMORY_ABI_VERSION
            || request.reserved != 0
            || flags.contains(VmFlags::EXECUTE)
        {
            return status::INVALID_ARGUMENT;
        }
        let pages = match checked_page_count(request.length) {
            Ok(pages) => pages as usize,
            Err(error) => return error,
        };
        if pages > MAX_SHARED_PAGES {
            return status::LIMIT_REACHED;
        }
        let Some(capability_slot) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .free_capability_slot()
        else {
            return status::LIMIT_REACHED;
        };
        let object = match self.shared.create(pages, flags) {
            Ok(object) => object,
            Err(error) => return error,
        };
        let mut rights = Rights::MAP.union(Rights::TRANSFER);
        if flags.contains(VmFlags::READ) {
            rights = rights.union(Rights::READ);
        }
        if flags.contains(VmFlags::WRITE) {
            rights = rights.union(Rights::WRITE);
        }
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[capability_slot] = CapabilityEntry {
            kind: CapabilityKind::SharedMemory(object),
            rights,
        };
        capability_slot as i64
    }

    fn shared_memory_map(
        &mut self,
        process_index: usize,
        handle: Handle,
        request_address: u64,
    ) -> i64 {
        let request = match self.read_struct::<SharedMemoryMap>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return error,
        };
        let Some(flags) = valid_vm_flags(request.flags) else {
            return status::INVALID_ARGUMENT;
        };
        if request.version != MEMORY_ABI_VERSION
            || request.reserved != 0
            || !request.offset.is_multiple_of(PAGE_SIZE)
        {
            return status::INVALID_ARGUMENT;
        }
        let pages = match checked_page_count(request.length) {
            Ok(pages) => pages,
            Err(error) => return error,
        };
        let required_rights = vm_rights(flags).union(Rights::MAP);
        let object_id = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, required_rights)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::SharedMemory(object),
                ..
            }) => object,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        let offset_pages = request.offset / PAGE_SIZE;
        let (maximum_flags, object_pages) = match self.shared.get(object_id) {
            Ok(object) => (object.maximum_flags, object.pages as u64),
            Err(error) => return error,
        };
        if flags.0 & !maximum_flags.0 != 0
            || offset_pages
                .checked_add(pages)
                .is_none_or(|end| end > object_pages)
        {
            return status::ACCESS_DENIED;
        }
        let address = {
            let space = &self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space;
            if request.address == 0 {
                match space.find_free_range(request.length) {
                    Ok(address) => address,
                    Err(_) => return status::LIMIT_REACHED,
                }
            } else if space.range_is_free(request.address, request.length) {
                request.address
            } else {
                return status::INVALID_ARGUMENT;
            }
        };
        let actual_flags = page_flags(flags);
        let allowed_flags = page_flags(maximum_flags);
        let mut mapped = 0u64;
        while mapped < pages {
            let physical = match self.shared.get(object_id) {
                Ok(object) => object.frames[(offset_pages + mapped) as usize].phys,
                Err(error) => return error,
            };
            let result = self.processes[process_index]
                .as_mut()
                .expect("process")
                .address_space
                .map_shared_page(
                    address + mapped * PAGE_SIZE,
                    physical,
                    actual_flags,
                    allowed_flags,
                    object_id,
                );
            if result.is_err() || self.shared.retain_mapping(object_id).is_err() {
                for rollback in 0..mapped {
                    let _ = self.processes[process_index]
                        .as_mut()
                        .expect("process")
                        .address_space
                        .unmap_page(address + rollback * PAGE_SIZE);
                    self.shared.release_mappings(object_id, 1);
                }
                return status::OUT_OF_MEMORY;
            }
            mapped += 1;
        }
        self.flush_process(process_index);
        address as i64
    }

    fn shared_memory_seal(
        &mut self,
        process_index: usize,
        handle: Handle,
        requested: VmFlags,
    ) -> i64 {
        let Some(flags) = valid_vm_flags(requested) else {
            return status::INVALID_ARGUMENT;
        };
        if flags.contains(VmFlags::WRITE) {
            return status::INVALID_ARGUMENT;
        }
        let entry = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, Rights::WRITE.union(Rights::MAP))
        {
            Ok(entry) => entry,
            Err(error) => return error,
        };
        let CapabilityKind::SharedMemory(object) = entry.kind else {
            return status::ACCESS_DENIED;
        };
        if let Err(error) = self.shared.seal(object, flags) {
            return error;
        }

        // `capability_refs == 1` гарантирует, что это единственный handle на
        // объект. Меняем его authority атомарно вместе с object policy.
        let mut rights = Rights::MAP.union(Rights::TRANSFER).union(Rights::READ);
        if flags.contains(VmFlags::EXECUTE) {
            rights = rights.union(Rights::EXECUTE);
        }
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[handle.0 as usize]
            .rights = rights;
        status::OK
    }

    fn handle_close(&mut self, process_index: usize, handle: Handle) -> i64 {
        let slot = handle.0 as usize;
        if slot == 0 || slot >= MAX_CAPABILITIES {
            return status::BAD_HANDLE;
        }
        let entry = self.processes[process_index]
            .as_ref()
            .expect("process")
            .capabilities[slot];
        if entry.kind == CapabilityKind::Empty {
            return status::BAD_HANDLE;
        }
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slot] = EMPTY_CAPABILITY;
        self.release_capability(entry);
        status::OK
    }

    fn monotonic_nanoseconds(&self) -> i64 {
        if self.counter_hz == 0 {
            return status::NOT_SUPPORTED;
        }
        let ticks = arch::read_monotonic_counter();
        let seconds = ticks / self.counter_hz;
        let remainder = ticks % self.counter_hz;
        let nanoseconds = seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(remainder.saturating_mul(1_000_000_000) / self.counter_hz);
        i64::try_from(nanoseconds).unwrap_or(i64::MAX)
    }

    fn block_get_size(&self, process_index: usize, handle: Handle) -> i64 {
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(handle, CapabilityKind::BlockDevice(0), Rights::READ)
        {
            return error;
        }
        block::info()
            .ok()
            .and_then(|info| i64::try_from(info.blocks).ok())
            .unwrap_or(status::IO_ERROR)
    }

    /// Единственная страница за вызов держит kernel stack и DMA latency
    /// ограниченными. `vfsd` строит streaming поверх последовательности этих
    /// операций, а обычные приложения вообще не видят block capability.
    fn block_io(
        &self,
        process_index: usize,
        handle: Handle,
        request_address: u64,
        write: bool,
    ) -> i64 {
        let required = if write { Rights::WRITE } else { Rights::READ };
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(handle, CapabilityKind::BlockDevice(0), required)
        {
            return error;
        }
        let request = match self.read_struct::<BlockIoRequest>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if request.version != BLOCK_ABI_VERSION
            || request.flags != 0
            || request.block_count != 1
            || request.reserved != 0
        {
            return status::INVALID_ARGUMENT;
        }
        let mut page = [0u8; PAGE_SIZE as usize];
        if write {
            if self
                .copy_from_process(process_index, request.buffer_address, &mut page)
                .is_err()
            {
                return status::INVALID_ARGUMENT;
            }
            block::write_block(request.block, &page)
                .map(|_| status::OK)
                .unwrap_or(status::IO_ERROR)
        } else {
            if block::read_block(request.block, &mut page).is_err() {
                return status::IO_ERROR;
            }
            self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .copy_to_user(request.buffer_address, &page)
                .map(|_| status::OK)
                .unwrap_or(status::INVALID_ARGUMENT)
        }
    }

    fn block_flush(&self, process_index: usize, handle: Handle) -> i64 {
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(handle, CapabilityKind::BlockDevice(0), Rights::WRITE)
        {
            return error;
        }
        block::flush()
            .map(|_| status::OK)
            .unwrap_or(status::IO_ERROR)
    }

    fn vfs_stat(&self, process_index: usize, handle: Handle, path: u64, length: u64) -> i64 {
        let process = self.processes[process_index].as_ref().expect("process");
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

    fn ipc_receive(
        &mut self,
        thread_index: usize,
        handle: Handle,
        user_buffer: u64,
    ) -> BlockingResult {
        let pid = self.threads[thread_index].as_ref().expect("thread").pid;
        let process_index = self.process_index(pid).expect("process");
        let endpoint_id = {
            let process = self.processes[process_index].as_ref().expect("process");
            if !process
                .address_space
                .contains_user_range(user_buffer, size_of::<Message>(), true)
            {
                return BlockingResult::Return(status::INVALID_ARGUMENT);
            }
            match process.resolve_endpoint(handle, Rights::RECEIVE) {
                Ok(endpoint) => endpoint,
                Err(error) => return BlockingResult::Return(error),
            }
        };
        let endpoint_index = endpoint_id as usize;
        if endpoint_index >= MAX_ENDPOINTS {
            return BlockingResult::Return(status::BAD_HANDLE);
        }
        if let Some(message) = self.endpoints[endpoint_index].queue.pop() {
            let result = self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .copy_to_user(user_buffer, message_bytes(&message));
            return BlockingResult::Return(if result.is_ok() {
                status::OK
            } else {
                status::INVALID_ARGUMENT
            });
        }
        let tid = self.threads[thread_index].as_ref().expect("thread").tid;
        if self.scheduler.block(tid).is_err() {
            return BlockingResult::Return(status::BUSY);
        }
        self.threads[thread_index].as_mut().expect("thread").pending =
            PendingOperation::Receive(PendingReceive {
                endpoint: endpoint_id,
                user_buffer,
            });
        self.blocked_receives = self.blocked_receives.saturating_add(1);
        BlockingResult::Blocked
    }

    fn ipc_send(&mut self, sender_index: usize, handle: Handle, user_message: u64) -> i64 {
        let (endpoint_id, sender_pid) = {
            let sender = self.processes[sender_index].as_ref().expect("sender");
            if !sender
                .address_space
                .contains_user_range(user_message, size_of::<Message>(), false)
            {
                return status::INVALID_ARGUMENT;
            }
            let endpoint = match sender.resolve_endpoint(handle, Rights::SEND) {
                Ok(endpoint) => endpoint,
                Err(error) => return error,
            };
            (endpoint, sender.pid)
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
            .expect("sender")
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
        let Some(receiver_index) = self.process_index(receiver_pid) else {
            return status::BAD_HANDLE;
        };
        let pending_thread = self.threads.iter().position(|slot| {
            slot.as_ref().is_some_and(|thread| {
                thread.pid == receiver_pid
                    && matches!(thread.pending, PendingOperation::Receive(pending) if pending.endpoint == endpoint_id)
            })
        });
        if pending_thread.is_none() && self.endpoints[endpoint_index].queue.is_full() {
            return status::QUEUE_FULL;
        }
        if let Err(error) = self.transfer_handles(sender_index, receiver_index, &mut message) {
            return error;
        }
        if let Some(thread_index) = pending_thread {
            let PendingOperation::Receive(pending) = self.threads[thread_index]
                .as_ref()
                .expect("receiver")
                .pending
            else {
                return status::BUSY;
            };
            if self.processes[receiver_index]
                .as_ref()
                .expect("receiver")
                .address_space
                .copy_to_user(pending.user_buffer, message_bytes(&message))
                .is_err()
            {
                return status::INVALID_ARGUMENT;
            }
            let receiver = self.threads[thread_index].as_mut().expect("receiver");
            receiver.pending = PendingOperation::None;
            receiver.context.set_syscall_result(status::OK);
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
                .expect("sender")
                .capability(transfer.handle, Rights::TRANSFER)?;
            let rights =
                derive_capability_rights(source.rights, transfer.rights).map_err(|error| {
                    match error {
                        CapabilityTransferError::EmptyRights => status::INVALID_ARGUMENT,
                        _ => status::ACCESS_DENIED,
                    }
                })?;
            let receiver = self.processes[receiver_index].as_ref().expect("receiver");
            let slot = (1..MAX_CAPABILITIES)
                .find(|slot| {
                    receiver.capabilities[*slot].kind == CapabilityKind::Empty && !reserved[*slot]
                })
                .ok_or(status::LIMIT_REACHED)?;
            reserved[slot] = true;
            destination_slots[item] = slot;
            entries[item] = CapabilityEntry {
                kind: source.kind,
                rights,
            };
        }
        for item in 0..count {
            let slot = destination_slots[item];
            self.processes[receiver_index]
                .as_mut()
                .expect("receiver")
                .capabilities[slot] = entries[item];
            self.retain_capability(entries[item]);
            message.handles[item].handle = Handle(slot as u32);
            self.transferred_capabilities = self.transferred_capabilities.saturating_add(1);
        }
        Ok(())
    }

    fn reap_process(&mut self, pid: ProcessId) {
        let Some(process_index) = self.process_index(pid) else {
            return;
        };
        if !self.processes[process_index]
            .as_ref()
            .is_some_and(|process| process.exited)
        {
            return;
        }
        for shared_index in 0..MAX_SHARED_OBJECTS {
            let object = shared_id(shared_index, self.shared.objects[shared_index].generation);
            let count = self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .shared_mapping_pages(object);
            if count != 0 {
                self.shared.release_mappings(object, count);
            }
        }
        let capabilities = self.processes[process_index]
            .as_ref()
            .expect("process")
            .capabilities;
        for entry in capabilities {
            self.release_capability(entry);
        }
        for index in 0..MAX_THREADS {
            if self.threads[index]
                .as_ref()
                .is_some_and(|thread| thread.pid == pid)
            {
                let tid = self.threads[index].as_ref().expect("thread").tid;
                if self
                    .scheduler
                    .info(tid)
                    .is_ok_and(|info| info.state == ThreadState::Exited)
                {
                    let _ = self.scheduler.reap(tid);
                }
                self.threads[index] = None;
                self.invalidate_capability_kind(CapabilityKind::Thread(tid));
            }
        }
        let _ = self.process_table.reap(pid);
        self.processes[process_index] = None;
        self.invalidate_capability_kind(CapabilityKind::Process(pid));
    }

    fn reap_thread(&mut self, tid: ThreadId) {
        let Some(index) = self.thread_index(tid) else {
            return;
        };
        if !self.threads[index]
            .as_ref()
            .is_some_and(|thread| thread.exited)
        {
            return;
        }
        if self
            .scheduler
            .info(tid)
            .is_ok_and(|info| info.state == ThreadState::Exited)
        {
            let _ = self.scheduler.reap(tid);
        }
        self.threads[index] = None;
        self.invalidate_capability_kind(CapabilityKind::Thread(tid));
    }

    fn invalidate_capability_kind(&mut self, kind: CapabilityKind) {
        for process in self.processes.iter_mut().flatten() {
            for entry in &mut process.capabilities {
                if entry.kind == kind {
                    *entry = EMPTY_CAPABILITY;
                }
            }
        }
    }

    fn retain_capability(&mut self, entry: CapabilityEntry) {
        if let CapabilityKind::SharedMemory(object) = entry.kind {
            let _ = self.shared.retain_capability(object);
        }
    }

    fn release_capability(&mut self, entry: CapabilityEntry) {
        if let CapabilityKind::SharedMemory(object) = entry.kind {
            self.shared.release_capability(object);
        }
    }

    fn write_reason_for_thread(
        &self,
        thread_index: usize,
        address: u64,
        reason: ExitReason,
    ) -> i64 {
        let pid = self.threads[thread_index].as_ref().expect("waiter").pid;
        let Some(process_index) = self.process_index(pid) else {
            return status::BAD_HANDLE;
        };
        self.write_struct(process_index, address, &reason)
            .map(|_| status::OK)
            .unwrap_or(status::INVALID_ARGUMENT)
    }

    fn read_struct<T: Copy>(&self, process_index: usize, address: u64) -> Result<T, i64> {
        let mut value = MaybeUninit::<T>::uninit();
        let bytes =
            unsafe { slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
        self.copy_from_process(process_index, address, bytes)?;
        Ok(unsafe { value.assume_init() })
    }

    fn write_struct<T>(&self, process_index: usize, address: u64, value: &T) -> Result<(), i64> {
        self.processes[process_index]
            .as_ref()
            .expect("process")
            .address_space
            .copy_to_user(address, bytes_of(value))
            .map_err(|_| status::INVALID_ARGUMENT)
    }

    fn copy_from_process(
        &self,
        process_index: usize,
        address: u64,
        output: &mut [u8],
    ) -> Result<(), i64> {
        if output.is_empty() {
            return Ok(());
        }
        self.processes[process_index]
            .as_ref()
            .expect("process")
            .address_space
            .copy_from_user(address, output)
            .map_err(|_| status::INVALID_ARGUMENT)
    }

    fn user_writable(&self, process_index: usize, address: u64, length: usize) -> bool {
        self.processes[process_index]
            .as_ref()
            .expect("process")
            .address_space
            .contains_user_range(address, length, true)
    }

    fn flush_process(&self, process_index: usize) {
        let root = self.processes[process_index]
            .as_ref()
            .expect("process")
            .address_space
            .root();
        unsafe { arch::switch_address_space(root) };
    }

    fn process_index(&self, pid: ProcessId) -> Option<usize> {
        self.processes
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|process| process.pid == pid))
    }

    fn thread_index(&self, tid: ThreadId) -> Option<usize> {
        self.threads
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|thread| thread.tid == tid))
    }

    fn has_live_threads(&self, pid: ProcessId) -> bool {
        self.threads
            .iter()
            .flatten()
            .any(|thread| thread.pid == pid && !thread.exited)
    }

    fn cleanup(&mut self) {
        for index in 0..MAX_THREADS {
            if let Some(thread) = self.threads[index].take() {
                if self
                    .scheduler
                    .info(thread.tid)
                    .is_ok_and(|info| info.state != ThreadState::Exited)
                {
                    let _ = self.scheduler.exit(thread.tid, NO_EXIT);
                }
                let _ = self.scheduler.reap(thread.tid);
            }
        }
        for index in 0..MAX_PROCESSES {
            if let Some(process) = self.processes[index].take() {
                for shared_index in 0..MAX_SHARED_OBJECTS {
                    let object =
                        shared_id(shared_index, self.shared.objects[shared_index].generation);
                    let count = process.address_space.shared_mapping_pages(object);
                    if count != 0 {
                        self.shared.release_mappings(object, count);
                    }
                }
                for entry in process.capabilities {
                    self.release_capability(entry);
                }
                if self
                    .process_table
                    .info(process.pid)
                    .is_ok_and(|info| info.state == rustos_microkernel::ProcessState::Alive)
                {
                    let _ = self.process_table.exit(process.pid, NO_EXIT);
                }
                let _ = self.process_table.reap(process.pid);
                drop(process);
            }
        }
        self.shared.cleanup();
    }
}

enum BlockingResult {
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
    let hardware = arch::initialize_scheduler_hardware().map_err(|_| {
        serial::put_str("[irq] interrupt controller/timer initialization failed\n");
        ProcessError::UnexpectedExit
    })?;
    serial::put_str("[irq] controller=");
    serial::put_str(hardware.interrupt_controller);
    serial::put_str(" boot-cpu=");
    serial::put_u32(hardware.boot_cpu_id);
    serial::put_str(" counter-MHz=");
    serial::put_u32((hardware.counter_hz / 1_000_000) as u32);
    serial::put_str(" timer=");
    serial::put_str(hardware.timer);
    serial::put_str("\n");
    let smp = arch::start_secondary_cpus(info, hardware.counter_hz).map_err(|_| {
        serial::put_str("[smp] secondary CPU startup failed\n");
        ProcessError::UnexpectedExit
    })?;
    serial::put_str("[smp] discovery=");
    serial::put_str(smp.discovery);
    serial::put_str(" discovered=");
    serial::put_u32(smp.discovered_cpus as u32);
    serial::put_str(" online=");
    serial::put_u32(smp.online_cpus as u32);
    serial::put_str(" APs parked safely\n");

    let free_before = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    let manager = unsafe { &mut *ptr::addr_of_mut!(MANAGER) };
    manager.initialize(info.initramfs, hardware.counter_hz);
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
    serial::put_str("[preempt] timer ticks=");
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
        arch::ILLEGAL_INSTRUCTION_EXCEPTION,
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

    manager.begin_phase();
    let mut lifecycle_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    lifecycle_caps[VFS_ROOT_SLOT] = CapabilityEntry {
        kind: CapabilityKind::VfsRoot,
        rights: Rights::READ.union(Rights::EXECUTE).union(Rights::TRANSFER),
    };
    manager.spawn(
        "system/bin/abi-lifecycle.elf",
        PriorityClass::Interactive,
        [VFS_ROOT_SLOT as u64, syscall::ABI_VERSION, 0],
        0,
        0,
        lifecycle_caps,
    )?;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    serial::put_str("[abi-v4] spawn/wait/kill threads VM shared-memory TLS clock verified\n");
    manager.cleanup();

    // Настоящий VFS vertical slice: filesystem parser и pathname policy живут
    // в ring 3. Только vfsd получает raw block capability, клиент видит лишь
    // endpoint и `vfs.dll` API. Второй запуск server доказывает persistence.
    run_vfs_phase(manager, "system/bin/vfs-test.elf", false)?;
    serial::put_str(
        "[vfsd] open/read/write/seek/readdir/create/rename over shared memory verified\n",
    );
    run_vfs_phase(manager, "system/bin/vfs-persistence.elf", false)?;
    serial::put_str("[vfsd] restart recovered committed VaraniaFS metadata and file data\n");
    run_vfs_phase(manager, "system/bin/loader-test.elf", true)?;
    serial::put_str(
        "[loader] DT_NEEDED symbols RELA TLS RELRO and cross-process shared RX verified\n",
    );

    let free_after = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    if free_after != free_before {
        return Err(ProcessError::FrameLeak);
    }
    serial::put_str("[process-manager] dynamic create/exit/reap reclaimed all frames\n");
    serial::put_str("[process-manager] ABI v4 VM/shared-memory frames reclaimed\n");
    Ok(())
}

fn run_vfs_phase(
    manager: &mut ProcessManager,
    client_image: &str,
    executable_namespace: bool,
) -> Result<(), ProcessError> {
    const SERVER_ENDPOINT: u8 = 1;
    const REPLY_ENDPOINT: u8 = 2;
    const SERVER_SLOT: usize = 2;
    const DEVICE_OR_REPLY_SLOT: usize = 3;

    manager.begin_phase();
    let mut server_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    server_caps[SERVER_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(SERVER_ENDPOINT),
        rights: Rights::RECEIVE,
    };
    server_caps[DEVICE_OR_REPLY_SLOT] = CapabilityEntry {
        kind: CapabilityKind::BlockDevice(0),
        rights: Rights::READ.union(Rights::WRITE),
    };
    let server = manager.spawn(
        "system/bin/vfsd.elf",
        PriorityClass::System,
        [
            SERVER_SLOT as u64,
            DEVICE_OR_REPLY_SLOT as u64,
            syscall::ABI_VERSION,
        ],
        0,
        0,
        server_caps,
    )?;
    manager.endpoints[SERVER_ENDPOINT as usize].receiver = server;

    let mut client_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    if executable_namespace {
        client_caps[VFS_ROOT_SLOT] = CapabilityEntry {
            kind: CapabilityKind::VfsRoot,
            rights: Rights::READ.union(Rights::EXECUTE),
        };
    }
    client_caps[SERVER_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(SERVER_ENDPOINT),
        rights: Rights::SEND,
    };
    client_caps[DEVICE_OR_REPLY_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(REPLY_ENDPOINT),
        rights: Rights::SEND.union(Rights::RECEIVE).union(Rights::TRANSFER),
    };
    let client = manager.spawn(
        client_image,
        PriorityClass::Interactive,
        [
            SERVER_SLOT as u64,
            DEVICE_OR_REPLY_SLOT as u64,
            syscall::ABI_VERSION,
        ],
        0,
        0,
        client_caps,
    )?;
    manager.endpoints[REPLY_ENDPOINT as usize].receiver = client;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    manager.cleanup();
    Ok(())
}

fn bytes_of<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn message_bytes(message: &Message) -> &[u8] {
    bytes_of(message)
}

fn normal_exit(status_value: u64) -> ExitReason {
    ExitReason {
        status: status_value as i64 as i32,
        exception: 0,
        flags: 0,
        fault_address: 0,
    }
}

fn user_priority(value: u8) -> Option<PriorityClass> {
    match value {
        value if value == PriorityClass::Interactive as u8 => Some(PriorityClass::Interactive),
        value if value == PriorityClass::Batch as u8 => Some(PriorityClass::Batch),
        value if value == PriorityClass::Idle as u8 => Some(PriorityClass::Idle),
        _ => None,
    }
}

fn valid_vm_flags(flags: VmFlags) -> Option<VmFlags> {
    let valid = VmFlags::READ
        .union(VmFlags::WRITE)
        .union(VmFlags::EXECUTE)
        .0;
    if flags.0 & !valid != 0
        || !flags.contains(VmFlags::READ)
        || (flags.contains(VmFlags::WRITE) && flags.contains(VmFlags::EXECUTE))
    {
        None
    } else {
        Some(flags)
    }
}

fn page_flags(flags: VmFlags) -> UserPageFlags {
    UserPageFlags {
        writable: flags.contains(VmFlags::WRITE),
        executable: flags.contains(VmFlags::EXECUTE),
    }
}

fn vm_rights(flags: VmFlags) -> Rights {
    let mut rights = Rights::NONE;
    if flags.contains(VmFlags::READ) {
        rights = rights.union(Rights::READ);
    }
    if flags.contains(VmFlags::WRITE) {
        rights = rights.union(Rights::WRITE);
    }
    if flags.contains(VmFlags::EXECUTE) {
        rights = rights.union(Rights::EXECUTE);
    }
    rights
}

fn checked_page_count(length: u64) -> Result<u64, i64> {
    if length == 0 || !length.is_multiple_of(PAGE_SIZE) {
        return Err(status::INVALID_ARGUMENT);
    }
    let pages = length / PAGE_SIZE;
    if pages > MAX_VM_SYSCALL_PAGES {
        return Err(status::LIMIT_REACHED);
    }
    Ok(pages)
}

fn normalize_boot_path(path: &str) -> &str {
    path.strip_prefix("/boot/")
        .or_else(|| path.strip_prefix('/'))
        .unwrap_or(path)
}

fn valid_string_table(bytes: &[u8], expected_count: u32) -> bool {
    if bytes.is_empty() {
        return expected_count == 0;
    }
    if bytes.last() != Some(&0) {
        return false;
    }
    let mut count = 0u32;
    for string in bytes
        .split(|byte| *byte == 0)
        .take_while(|string| !string.is_empty())
    {
        if str::from_utf8(string).is_err() {
            return false;
        }
        count = count.saturating_add(1);
    }
    count == expected_count
        && bytes.iter().filter(|byte| **byte == 0).count() == expected_count as usize
}

fn shared_id(index: usize, generation: u8) -> u16 {
    (u16::from(generation) << 8) | index as u16
}

fn shared_index(id: u16) -> usize {
    usize::from(id & 0xff)
}

fn shared_generation(id: u16) -> u8 {
    (id >> 8) as u8
}

fn next_u8_generation(generation: u8) -> u8 {
    let next = generation.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

fn frame_error_name(error: crate::memory::FrameAllocatorError) -> &'static str {
    match error {
        crate::memory::FrameAllocatorError::AlreadyInitialized => "already-initialized",
        crate::memory::FrameAllocatorError::NotInitialized => "not-initialized",
        crate::memory::FrameAllocatorError::InvalidRequest => "invalid-request",
        crate::memory::FrameAllocatorError::OutOfMemory => "out-of-memory",
        crate::memory::FrameAllocatorError::TooFragmented => "too-fragmented",
        crate::memory::FrameAllocatorError::InvalidFree => "invalid-free",
    }
}

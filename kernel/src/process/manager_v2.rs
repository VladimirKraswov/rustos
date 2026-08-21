//! CPU0 process manager ABI v7: процессы, несколько потоков, VM, shared
//! memory, capability IPC, graphics objects и монотонные часы.
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
    display::{DisplayAtomicPresent, DisplayScanoutInfo, DisplayVblankWait},
    gpu::{
        GpuContextCreate, GpuDeviceInfo, GpuResourceCreate, GpuResourceImport, GpuSubmit,
        GPU_MAX_COMMAND_BYTES,
    },
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain},
    ipc::{Message, IPC_MAX_HANDLES},
    memory::{SharedMemoryCreate, SharedMemoryMap, VmFlags, VmMapRequest, MEMORY_ABI_VERSION},
    pipe::{PipeCreateResult, PIPE_ABI_VERSION},
    process::{
        ProcessSpawnRequest, ProcessSpawnResult, ProcessStartInfo, SpawnCapability,
        StartupCapability, StartupRole, ThreadCreateRequest, ThreadCreateResult,
        PROCESS_ABI_VERSION, PROCESS_SPAWN_MAX_CAPABILITIES, PROCESS_START_INFO_ADDRESS,
    },
    sync::{
        SyncPoint, SyncTimelineCreate, SyncTimelineSignal, SyncTimelineWait, SyncWaitMany,
        SyncWaitMode, SYNC_MAX_WAIT_POINTS, SYNC_TIMEOUT_INFINITE,
    },
    syscall::{self, status},
    ExitReason, Handle, PriorityClass, ProcessId, Rights, ThreadId, PAGE_SIZE,
};
use rustos_microkernel::{
    derive_capability_rights, prepare_message, CapabilityTransferError, EndpointQueue,
    IpcQueueError, ProcessTable, Scheduler, ThreadState, TimelineError, TimelineId, TimelineTable,
};

use crate::{
    arch::{self, TrapFrame, TrapKind, UserContext},
    block,
    display::scanout::{self, DisplayBrokerError},
    fs,
    memory::{self, AddressSpace, FrameBlock, UserPageBacking, UserPageFlags},
    serial,
};

use super::{
    graphics_objects::{make_id as graphics_id, GraphicsBufferPool, MAX_GRAPHICS_BUFFERS},
    load_executable, CapabilityEntry, CapabilityKind, InteractiveExit, ProcessError,
    EMPTY_CAPABILITY, MAX_CAPABILITIES, VFS_ROOT_SLOT,
};

const MAX_PROCESSES: usize = 12;
const MAX_THREADS: usize = 24;
const MAX_ENDPOINTS: usize = 6;
const ENDPOINT_QUEUE_CAPACITY: usize = 8;
const ENDPOINT_SLOT: usize = 2;
const MAX_SHARED_OBJECTS: usize = 8;
const MAX_SHARED_PAGES: usize = 64;
const MAX_SYNC_TIMELINES: usize = 32;
const MAX_SYNC_WAITS: usize = MAX_THREADS;
const MAX_GPU_IMPORTS: usize = 4;
/// Один mapping ограничен 1 GiB: достаточно для compiler arenas, но ошибка в
/// user-space всё ещё не может одним syscall переполнить арифметику/metadata.
const MAX_VM_SYSCALL_PAGES: u64 = 256 * 1024;
const MAX_PATH_BYTES: usize = 255;
const MAX_ARGUMENT_BYTES: usize = 2048;
const MAX_ENVIRONMENT_BYTES: usize = 2048;
const MAX_PIPES: usize = 8;
const PIPE_BUFFER_BYTES: usize = 4096;
const INITIAL_USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
/// Initial thread получает страницы по требованию, но не может бесконечно
/// поглощать address space из-за runaway recursion.
const MAX_GROWING_STACK_BYTES: u64 = 8 * 1024 * 1024;
/// LLVM stack probing на разных ISA может обратиться не к строго соседней
/// странице. Разрешаем только короткий gap до непрерывной mapped части.
const MAX_STACK_FAULT_GAP_PAGES: u64 = 16;

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
struct PendingFutex {
    address: u64,
    deadline_ns: u64,
}

#[derive(Clone, Copy)]
struct PendingPipe {
    pipe: u16,
    write: bool,
}

#[derive(Clone, Copy)]
struct PendingVblank {
    sequence: u64,
    present_deadline_ns: u64,
    timeout_deadline_ns: u64,
}

#[derive(Clone, Copy)]
struct PendingGpuSubmission {
    fence: u64,
    timeline: TimelineId,
    value: u64,
}

#[derive(Clone, Copy)]
struct PendingSyncPoint {
    timeline: TimelineId,
    value: u64,
}

impl PendingSyncPoint {
    const EMPTY: Self = Self {
        timeline: TimelineId(0),
        value: 0,
    };
}

#[derive(Clone, Copy)]
struct SyncWaitSlot {
    used: bool,
    thread: ThreadId,
    points: [PendingSyncPoint; SYNC_MAX_WAIT_POINTS as usize],
    point_count: usize,
    mode: SyncWaitMode,
    deadline_ns: u64,
}

impl SyncWaitSlot {
    const EMPTY: Self = Self {
        used: false,
        thread: ThreadId::INVALID,
        points: [PendingSyncPoint::EMPTY; SYNC_MAX_WAIT_POINTS as usize],
        point_count: 0,
        mode: SyncWaitMode::ALL,
        deadline_ns: SYNC_TIMEOUT_INFINITE,
    };
}

#[derive(Clone, Copy)]
enum PendingOperation {
    None,
    Receive(PendingReceive),
    ProcessWait(PendingWait),
    ThreadJoin(PendingJoin),
    Futex(PendingFutex),
    Pipe(PendingPipe),
    Sync(u8),
    DisplayVblank(PendingVblank),
}

struct ManagedThread {
    tid: ThreadId,
    pid: ProcessId,
    context: UserContext,
    pending: PendingOperation,
    exited: bool,
    exit_reason: ExitReason,
    detached: bool,
    reclaim_address: u64,
    reclaim_length: u64,
}

struct ManagedProcess {
    pid: ProcessId,
    parent: ProcessId,
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

    /// Копирует небольшой control-plane диапазон прямо из объекта. Это нужно
    /// bounded wait-many parser'у: kernel не доверяет user mapping и не
    /// хранит process-local pointer после блокировки.
    fn copy_bytes(&self, id: u16, offset: u64, output: &mut [u8]) -> Result<(), i64> {
        let object = self.get(id)?;
        let length = u64::try_from(output.len()).map_err(|_| status::INVALID_ARGUMENT)?;
        if offset
            .checked_add(length)
            .is_none_or(|end| end > object.pages as u64 * PAGE_SIZE)
        {
            return Err(status::INVALID_ARGUMENT);
        }
        let mut copied = 0usize;
        while copied < output.len() {
            let absolute = offset + copied as u64;
            let page = (absolute / PAGE_SIZE) as usize;
            let page_offset = (absolute % PAGE_SIZE) as usize;
            let count = (PAGE_SIZE as usize - page_offset).min(output.len() - copied);
            // SAFETY: object удерживается capability вызывающего процесса,
            // page проверен диапазоном выше, physical frame identity-mapped.
            unsafe {
                ptr::copy_nonoverlapping(
                    (object.frames[page].phys as *const u8).add(page_offset),
                    output[copied..copied + count].as_mut_ptr(),
                    count,
                );
            }
            copied += count;
        }
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

struct PipeObject {
    generation: u8,
    used: bool,
    buffer: [u8; PIPE_BUFFER_BYTES],
    read_offset: usize,
    length: usize,
    readers: usize,
    writers: usize,
}

impl PipeObject {
    const EMPTY: Self = Self {
        generation: 1,
        used: false,
        buffer: [0; PIPE_BUFFER_BYTES],
        read_offset: 0,
        length: 0,
        readers: 0,
        writers: 0,
    };

    fn read(&mut self, destination: &mut [u8]) -> usize {
        let count = destination.len().min(self.length);
        for (index, byte) in destination.iter_mut().take(count).enumerate() {
            *byte = self.buffer[(self.read_offset + index) % PIPE_BUFFER_BYTES];
        }
        self.read_offset = (self.read_offset + count) % PIPE_BUFFER_BYTES;
        self.length -= count;
        count
    }

    fn write(&mut self, source: &[u8]) -> usize {
        let count = source.len().min(PIPE_BUFFER_BYTES - self.length);
        let write_offset = (self.read_offset + self.length) % PIPE_BUFFER_BYTES;
        for (index, byte) in source.iter().take(count).enumerate() {
            self.buffer[(write_offset + index) % PIPE_BUFFER_BYTES] = *byte;
        }
        self.length += count;
        count
    }
}

struct PipePool {
    objects: [PipeObject; MAX_PIPES],
}

impl PipePool {
    const fn new() -> Self {
        Self {
            objects: [const { PipeObject::EMPTY }; MAX_PIPES],
        }
    }

    fn create(&mut self) -> Result<u16, i64> {
        let Some(index) = self.objects.iter().position(|object| !object.used) else {
            return Err(status::LIMIT_REACHED);
        };
        let generation = self.objects[index].generation;
        self.objects[index] = PipeObject {
            generation,
            used: true,
            buffer: [0; PIPE_BUFFER_BYTES],
            read_offset: 0,
            length: 0,
            readers: 1,
            writers: 1,
        };
        Ok(pipe_id(index, generation))
    }

    fn get_mut(&mut self, id: u16) -> Result<&mut PipeObject, i64> {
        let object = self
            .objects
            .get_mut(pipe_index(id))
            .ok_or(status::BAD_HANDLE)?;
        if !object.used || object.generation != pipe_generation(id) {
            return Err(status::BAD_HANDLE);
        }
        Ok(object)
    }

    fn retain(&mut self, id: u16, rights: Rights) -> Result<(), i64> {
        let object = self.get_mut(id)?;
        if rights.contains(Rights::READ) {
            object.readers = object.readers.saturating_add(1);
        }
        if rights.contains(Rights::WRITE) {
            object.writers = object.writers.saturating_add(1);
        }
        Ok(())
    }

    fn release(&mut self, id: u16, rights: Rights) {
        let Ok(object) = self.get_mut(id) else { return };
        if rights.contains(Rights::READ) {
            object.readers = object.readers.saturating_sub(1);
        }
        if rights.contains(Rights::WRITE) {
            object.writers = object.writers.saturating_sub(1);
        }
        self.destroy_if_unused(id);
    }

    fn destroy_if_unused(&mut self, id: u16) {
        let index = pipe_index(id);
        let Some(object) = self.objects.get_mut(index) else {
            return;
        };
        if !object.used
            || object.generation != pipe_generation(id)
            || object.readers != 0
            || object.writers != 0
        {
            return;
        }
        let generation = next_u8_generation(object.generation);
        *object = PipeObject::EMPTY;
        object.generation = generation;
    }

    fn cleanup(&mut self) {
        for object in &mut self.objects {
            if object.used {
                let generation = next_u8_generation(object.generation);
                *object = PipeObject::EMPTY;
                object.generation = generation;
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
    capabilities: [StartupCapability; PROCESS_SPAWN_MAX_CAPABILITIES],
    capability_count: usize,
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
    graphics: GraphicsBufferPool,
    timelines: TimelineTable<MAX_SYNC_TIMELINES>,
    sync_waits: [SyncWaitSlot; MAX_SYNC_WAITS],
    pipes: PipePool,
    current: ThreadId,
    kernel_root: u64,
    initramfs: BootInitramfs,
    counter_hz: u64,
    timer_ticks: u64,
    context_switches: u64,
    blocked_receives: u64,
    transferred_capabilities: u64,
    display_present_sequence: u64,
    display_completed_sequence: u64,
    display_present_deadline_ns: u64,
    gpu_context_active: bool,
    gpu_imports: [Option<u16>; MAX_GPU_IMPORTS],
    gpu_submission: Option<PendingGpuSubmission>,
    gpu_last_fence: u64,
    gpu_last_status: i64,
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
            graphics: GraphicsBufferPool::new(),
            timelines: TimelineTable::new(),
            sync_waits: [SyncWaitSlot::EMPTY; MAX_SYNC_WAITS],
            pipes: PipePool::new(),
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
            display_present_sequence: 0,
            display_completed_sequence: 0,
            display_present_deadline_ns: 0,
            gpu_context_active: false,
            gpu_imports: [None; MAX_GPU_IMPORTS],
            gpu_submission: None,
            gpu_last_fence: 0,
            gpu_last_status: status::NOT_FOUND,
            deferred_process_reap: None,
            deferred_thread_reap: None,
        }
    }

    fn initialize(&mut self, initramfs: BootInitramfs, counter_hz: u64) {
        self.process_table = ProcessTable::new();
        self.scheduler = Scheduler::new();
        self.shared = SharedMemoryPool::new();
        self.graphics = GraphicsBufferPool::new();
        self.timelines = TimelineTable::new();
        self.sync_waits = [SyncWaitSlot::EMPTY; MAX_SYNC_WAITS];
        self.pipes = PipePool::new();
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
        self.display_present_sequence = 0;
        self.display_completed_sequence = 0;
        self.display_present_deadline_ns = 0;
        self.gpu_context_active = false;
        self.gpu_imports = [None; MAX_GPU_IMPORTS];
        self.gpu_submission = None;
        self.gpu_last_fence = 0;
        self.gpu_last_status = status::NOT_FOUND;
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
            ProcessError::MissingImage
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
        let loaded = load_executable(&mut address_space, image).map_err(|_| {
            serial::put_str("[process-manager] spawn failed: executable mapping\n");
            ProcessError::InvalidImage
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
                .install_start_info(&mut address_space, pid, tid, start, &loaded)
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
            parent: options.parent,
            address_space,
            capabilities,
            initramfs: self.initramfs,
            exited: false,
            exit_reason: NO_EXIT,
            expected: options.expected,
        });
        let mut context = UserContext::initial(loaded.entry, loaded.stack_pointer, arguments);
        context.set_thread_pointer(loaded.thread_pointer);
        self.threads[thread_slot] = Some(ManagedThread {
            tid,
            pid,
            context,
            pending: PendingOperation::None,
            exited: false,
            exit_reason: NO_EXIT,
            detached: false,
            reclaim_address: 0,
            reclaim_length: 0,
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
        loaded: &super::LoadedImage,
    ) -> Result<(), ()> {
        let header_size = size_of::<ProcessStartInfo>();
        let capability_address = align_up_usize(
            header_size
                .checked_add(start.argument_length)
                .and_then(|value| value.checked_add(start.environment_length))
                .ok_or(())?,
            core::mem::align_of::<StartupCapability>(),
        )
        .ok_or(())?;
        let capabilities_end = capability_address
            .checked_add(
                start
                    .capability_count
                    .checked_mul(size_of::<StartupCapability>())
                    .ok_or(())?,
            )
            .ok_or(())?;
        let tls_alignment = loaded
            .tls_template
            .and_then(|template| usize::try_from(template.alignment).ok())
            .unwrap_or(1);
        let tls_address_offset = align_up_usize(capabilities_end, tls_alignment).ok_or(())?;
        let total = tls_address_offset
            .checked_add(
                loaded
                    .tls_template
                    .map(|template| template.bytes.len())
                    .unwrap_or(0),
            )
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
        let capabilities_address = PROCESS_START_INFO_ADDRESS + capability_address as u64;
        let tls_template_address = loaded
            .tls_template
            .map(|_| PROCESS_START_INFO_ADDRESS + tls_address_offset as u64)
            .unwrap_or(0);
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
            capabilities_address,
            capability_count: start.capability_count as u32,
            reserved: 0,
            tls_template_address,
            tls_file_size: loaded
                .tls_template
                .map(|template| template.bytes.len() as u64)
                .unwrap_or(0),
            tls_memory_size: loaded
                .tls_template
                .map(|template| template.memory_size)
                .unwrap_or(0),
            tls_alignment: loaded
                .tls_template
                .map(|template| template.alignment as u32)
                .unwrap_or(0),
            #[cfg(target_arch = "x86_64")]
            tls_variant: loaded.tls_template.map(|_| 2).unwrap_or(0),
            #[cfg(target_arch = "aarch64")]
            tls_variant: loaded.tls_template.map(|_| 1).unwrap_or(0),
            tls_reserved: 0,
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
        if start.capability_count != 0 {
            let capability_bytes = unsafe {
                slice::from_raw_parts(
                    start.capabilities.as_ptr().cast::<u8>(),
                    start.capability_count * size_of::<StartupCapability>(),
                )
            };
            address_space
                .copy_into_user(capabilities_address, capability_bytes)
                .map_err(|_| ())?;
        }
        if let Some(template) = loaded.tls_template {
            address_space
                .copy_into_user(tls_template_address, template.bytes)
                .map_err(|_| ())?;
        }
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
            self.wake_expired_futexes();
            self.poll_gpu_completion();
            self.wake_expired_sync_waiters();
            self.wake_display_vblank_waiters();
            return self.schedule_next(frame);
        }
        if kind == TrapKind::Syscall {
            return self.handle_syscall(thread_index, frame);
        }
        let TrapKind::Exception {
            number,
            code,
            instruction_pointer,
            fault_address,
        } = kind
        else {
            crate::boot::exit_kernel(0x7c);
        };
        let pid = self.threads[thread_index]
            .as_ref()
            .expect("current thread")
            .pid;
        if self.try_grow_initial_stack(pid, number, code, fault_address) {
            return 0;
        }
        let reason = ExitReason {
            status: status::FAULT as i32,
            exception: number,
            flags: code,
            fault_address,
        };
        serial::put_str("[process-manager] fault pid=0x");
        serial::put_hex(pid.0);
        serial::put_str(" exception=");
        serial::put_u32(u32::from(number));
        serial::put_str(" code=0x");
        serial::put_hex(u64::from(code));
        serial::put_str(" ip=0x");
        serial::put_hex(instruction_pointer);
        serial::put_str(" address=0x");
        serial::put_hex(fault_address);
        serial::put_str("\n");
        self.finish_process(pid, reason);
        self.schedule_next(frame)
    }

    /// Обрабатывает только translation fault в коротком непрерывном диапазоне
    /// под уже отображённой частью основного стека. Такое правило учитывает
    /// ISA-specific stack probing, но не позволяет ошибочному указателю
    /// превратить произвольный дальний адрес в новую mapping.
    fn try_grow_initial_stack(
        &mut self,
        pid: ProcessId,
        exception: u16,
        code: u16,
        fault_address: u64,
    ) -> bool {
        if !is_stack_translation_fault(exception, code) {
            return false;
        }
        let page = fault_address & !(PAGE_SIZE - 1);
        let lower_limit = INITIAL_USER_STACK_TOP - MAX_GROWING_STACK_BYTES;
        if page < lower_limit || page >= INITIAL_USER_STACK_TOP {
            return false;
        }
        let Some(process_index) = self.process_index(pid) else {
            return false;
        };
        let process = self.processes[process_index].as_mut().expect("process");
        if process.address_space.is_writable(page) {
            return false;
        }
        let mut mapped_boundary = page.saturating_add(PAGE_SIZE);
        let mut gap_pages = 1u64;
        while mapped_boundary < INITIAL_USER_STACK_TOP
            && !process.address_space.is_writable(mapped_boundary)
            && gap_pages < MAX_STACK_FAULT_GAP_PAGES
        {
            mapped_boundary = mapped_boundary.saturating_add(PAGE_SIZE);
            gap_pages += 1;
        }
        if mapped_boundary >= INITIAL_USER_STACK_TOP
            || !process.address_space.is_writable(mapped_boundary)
        {
            return false;
        }
        let mut candidate = page;
        while candidate < mapped_boundary {
            if process
                .address_space
                .map_page(candidate, UserPageFlags::READ_WRITE)
                .is_err()
            {
                return false;
            }
            candidate += PAGE_SIZE;
        }
        // Новый PTE относится к активному address space. Перезагрузка того же
        // root выполняет обязательную TLB-инвалидацию на обеих архитектурах.
        unsafe { arch::switch_address_space(process.address_space.root()) };
        true
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
            syscall::number::PROCESS_TRY_WAIT => {
                let result = self.process_try_wait(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
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
            syscall::number::THREAD_DETACH => {
                let result = self.thread_detach(process_index, Handle(arg0 as u32));
                frame.set_syscall_result(result);
                0
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
            syscall::number::GRAPHICS_BUFFER_CREATE => {
                let result = self.graphics_buffer_create(process_index, arg0);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::GRAPHICS_BUFFER_MAP => {
                let result = self.graphics_buffer_map(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::GRAPHICS_BUFFER_GET_INFO => {
                let result =
                    self.graphics_buffer_get_info(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::SYNC_TIMELINE_CREATE => {
                let result = self.sync_timeline_create(process_index, arg0);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::SYNC_TIMELINE_SIGNAL => {
                let result = self.sync_timeline_signal(process_index, arg0);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::SYNC_TIMELINE_WAIT => {
                match self.sync_timeline_wait(thread_index, arg0) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::SYNC_TIMELINE_WAIT_MANY => {
                match self.sync_timeline_wait_many(thread_index, arg0) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::DISPLAY_GET_INFO => {
                let result = self.display_get_info(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::DISPLAY_ATOMIC_PRESENT => {
                let result = self.display_atomic_present(
                    process_index,
                    Handle(arg0 as u32),
                    Handle(arg1 as u32),
                    arg2,
                );
                frame.set_syscall_result(result);
                0
            }
            syscall::number::DISPLAY_WAIT_VBLANK => {
                match self.display_wait_vblank(thread_index, Handle(arg0 as u32), arg1) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::GPU_GET_INFO => {
                let result = self.gpu_get_info(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::GPU_CONTEXT_CREATE => {
                let result = self.gpu_context_create(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::GPU_RESOURCE_IMPORT => {
                let result = self.gpu_resource_import(
                    process_index,
                    Handle(arg0 as u32),
                    Handle(arg1 as u32),
                    arg2,
                );
                frame.set_syscall_result(result);
                0
            }
            syscall::number::GPU_RESOURCE_CREATE => {
                let result = self.gpu_resource_create(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::GPU_SUBMIT => {
                let result = self.gpu_submit(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::GPU_COMPLETION_STATUS => {
                let result = self.gpu_completion_status(process_index, Handle(arg0 as u32), arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::FUTEX_WAIT => {
                match self.futex_wait(thread_index, arg0, arg1 as u32, arg2) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::FUTEX_WAKE => {
                let result = self.futex_wake(process_index, arg0, arg1);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::PIPE_CREATE => {
                let result = self.pipe_create(process_index, arg0);
                frame.set_syscall_result(result);
                0
            }
            syscall::number::PIPE_READ => {
                match self.pipe_read(thread_index, Handle(arg0 as u32), arg1, arg2) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::PIPE_WRITE => {
                match self.pipe_write(thread_index, Handle(arg0 as u32), arg1, arg2) {
                    BlockingResult::Return(result) => {
                        frame.set_syscall_result(result);
                        0
                    }
                    BlockingResult::Blocked => self.schedule_next(frame),
                }
            }
            syscall::number::HANDLE_DUPLICATE => {
                let result =
                    self.handle_duplicate(process_index, Handle(arg0 as u32), Rights(arg1));
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
        let mut next = self.scheduler.schedule(0).ok().flatten();
        if next.is_none() && self.complete_idle_gpu_submission() {
            next = self.scheduler.schedule(0).ok().flatten();
        }
        if next.is_none() && self.complete_idle_estimated_vblank() {
            next = self.scheduler.schedule(0).ok().flatten();
        }
        let Some(next) = next else {
            arch::stop_scheduler_timer();
            arch::set_user_run_result(0);
            return 1;
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

    /// Virtio-gpu 2D не экспортирует vblank IRQ. Когда displayd — последний
    /// runnable thread, нельзя оставить всю систему blocked в ожидании
    /// несуществующего interrupt. Завершаем именно estimated wait событием
    /// fenced FLUSH; ABI feedback явно несёт ESTIMATED_VBLANK. При наличии
    /// другой runnable работы обычный timer path сохраняет frame pacing.
    fn complete_idle_estimated_vblank(&mut self) -> bool {
        let Some(index) = self.threads.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|thread| matches!(thread.pending, PendingOperation::DisplayVblank(_)))
        }) else {
            return false;
        };
        let PendingOperation::DisplayVblank(wait) =
            self.threads[index].as_ref().expect("vblank waiter").pending
        else {
            return false;
        };
        self.display_completed_sequence = self.display_completed_sequence.max(wait.sequence);
        let thread = self.threads[index].as_mut().expect("vblank waiter");
        thread.pending = PendingOperation::None;
        thread.context.set_syscall_result(status::OK);
        self.scheduler.wake(thread.tid).is_ok()
    }

    /// Снимает незавершённое estimated-vblank состояние умершего display
    /// master'а. `display_atomic_present` возвращается только после fenced
    /// virtio FLUSH, поэтому незавершённой DMA-команды здесь уже нет: остаётся
    /// лишь scheduler deadline. Sequence не обнуляется, чтобы feedback после
    /// supervisor restart сохранял глобальную монотонность.
    fn recover_display_master_after_exit(&mut self) {
        self.display_completed_sequence = self.display_present_sequence;
        self.display_present_deadline_ns = 0;
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
        self.cancel_sync_wait_for_thread(tid);
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
        if woke_waiter
            || self.threads[index]
                .as_ref()
                .is_some_and(|thread| thread.detached)
        {
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
                self.cancel_sync_wait_for_thread(tid);
                let _ = self.scheduler.exit(tid, reason);
                let thread = self.threads[index].as_mut().expect("thread");
                thread.exited = true;
                thread.exit_reason = reason;
                thread.pending = PendingOperation::None;
            }
        }
        // Pipe endpoints закрываются в момент exit, а не только после wait/reap:
        // иначе родитель, читающий `Command::output`, никогда не увидит EOF и
        // не сможет дойти до wait. Остальные capabilities живут до reap.
        for slot in 1..MAX_CAPABILITIES {
            let entry = self.processes[process_index]
                .as_ref()
                .expect("process")
                .capabilities[slot];
            if matches!(entry.kind, CapabilityKind::Pipe(_)) {
                self.processes[process_index]
                    .as_mut()
                    .expect("process")
                    .capabilities[slot] = EMPTY_CAPABILITY;
                self.release_capability(entry);
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
            capabilities: [StartupCapability::EMPTY; PROCESS_SPAWN_MAX_CAPABILITIES],
            capability_count: 0,
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
            role: StartupRole::NONE,
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
            if transfer.role != StartupRole::NONE {
                if start.capabilities[..start.capability_count]
                    .iter()
                    .any(|capability| capability.role == transfer.role)
                {
                    return status::INVALID_ARGUMENT;
                }
                start.capabilities[start.capability_count] = StartupCapability {
                    role: transfer.role,
                    flags: 0,
                    handle: Handle(transfer.target_slot as u32),
                    rights,
                };
                start.capability_count += 1;
            }
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
            Err(ProcessError::MissingImage) => return status::NOT_FOUND,
            Err(ProcessError::AddressSpace) => return status::OUT_OF_MEMORY,
            Err(_) => return status::INVALID_ARGUMENT,
        };
        // RECEIVE endpoint имеет одного активного владельца. При передаче
        // ребёнку (типичный VFS reply channel) маршрутизация временно следует
        // за ребёнком; reap восстановит родителя для последовательного spawn.
        for transfer in transfers.iter().take(transfer_count) {
            if transfer.rights.contains(Rights::RECEIVE) {
                if let Ok(CapabilityEntry {
                    kind: CapabilityKind::Endpoint(endpoint),
                    ..
                }) = self.processes[parent_index]
                    .as_ref()
                    .expect("parent")
                    .capability(transfer.source, Rights::TRANSFER)
                {
                    self.endpoints[endpoint as usize].receiver = pid;
                }
            }
        }
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

    fn process_try_wait(&mut self, process_index: usize, handle: Handle, user_reason: u64) -> i64 {
        if !self.user_writable(process_index, user_reason, size_of::<ExitReason>()) {
            return status::INVALID_ARGUMENT;
        }
        let target = match self.processes[process_index]
            .as_ref()
            .expect("caller")
            .capability(handle, Rights::WAIT)
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
        let target_process = self.processes[target_index].as_ref().expect("target");
        if !target_process.exited {
            return status::BUSY;
        }
        let reason = target_process.exit_reason;
        if self
            .write_struct(process_index, user_reason, &reason)
            .is_err()
        {
            return status::INVALID_ARGUMENT;
        }
        self.reap_process(target);
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
        let has_reclaim = request.reclaim_address != 0 || request.reclaim_length != 0;
        if !process.address_space.is_executable(request.entry)
            || !process.address_space.is_writable(request.stack_pointer - 1)
            || (request.thread_pointer != 0
                && !process
                    .address_space
                    .contains_user_range(request.thread_pointer, 1, false))
            || (has_reclaim
                && (request.reclaim_address == 0
                    || request.reclaim_length == 0
                    || !request.reclaim_address.is_multiple_of(PAGE_SIZE)
                    || !request.reclaim_length.is_multiple_of(PAGE_SIZE)
                    || !process.address_space.contains_user_range(
                        request.reclaim_address,
                        request.reclaim_length as usize,
                        true,
                    )
                    || request.stack_pointer <= request.reclaim_address
                    || request.stack_pointer
                        > request
                            .reclaim_address
                            .saturating_add(request.reclaim_length)
                    || (request.thread_pointer != 0
                        && (request.thread_pointer < request.reclaim_address
                            || request.thread_pointer
                                >= request
                                    .reclaim_address
                                    .saturating_add(request.reclaim_length)))))
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
            detached: false,
            reclaim_address: request.reclaim_address,
            reclaim_length: request.reclaim_length,
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

    fn thread_detach(&mut self, process_index: usize, handle: Handle) -> i64 {
        let slot = handle.0 as usize;
        if slot == 0 || slot >= MAX_CAPABILITIES {
            return status::BAD_HANDLE;
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
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slot] = EMPTY_CAPABILITY;
        let still_joinable = self.processes.iter().flatten().any(|process| {
            process
                .capabilities
                .iter()
                .any(|entry| entry.kind == CapabilityKind::Thread(target))
        });
        if !still_joinable {
            let Some(thread_index) = self.thread_index(target) else {
                return status::OK;
            };
            let exited = self.threads[thread_index]
                .as_ref()
                .is_some_and(|thread| thread.exited);
            self.threads[thread_index]
                .as_mut()
                .expect("thread")
                .detached = true;
            if exited {
                self.reap_thread(target);
            }
        }
        status::OK
    }

    fn futex_wait(
        &mut self,
        thread_index: usize,
        address: u64,
        expected: u32,
        timeout_ns: u64,
    ) -> BlockingResult {
        if !address.is_multiple_of(core::mem::align_of::<u32>() as u64) {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let pid = self.threads[thread_index].as_ref().expect("thread").pid;
        let process_index = self.process_index(pid).expect("process");
        let mut bytes = [0u8; size_of::<u32>()];
        if self
            .copy_from_process(process_index, address, &mut bytes)
            .is_err()
        {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        if u32::from_ne_bytes(bytes) != expected {
            return BlockingResult::Return(status::OK);
        }
        if timeout_ns == 0 {
            return BlockingResult::Return(status::TIMED_OUT);
        }
        let now = self.monotonic_nanoseconds().max(0) as u64;
        let deadline_ns = if timeout_ns == u64::MAX {
            u64::MAX
        } else {
            now.saturating_add(timeout_ns)
        };
        let tid = self.threads[thread_index].as_ref().expect("thread").tid;
        if self.scheduler.block(tid).is_err() {
            return BlockingResult::Return(status::BUSY);
        }
        self.threads[thread_index].as_mut().expect("thread").pending =
            PendingOperation::Futex(PendingFutex {
                address,
                deadline_ns,
            });
        BlockingResult::Blocked
    }

    fn futex_wake(&mut self, process_index: usize, address: u64, count: u64) -> i64 {
        if !address.is_multiple_of(core::mem::align_of::<u32>() as u64)
            || count == 0
            || !self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .contains_user_range(address, size_of::<u32>(), false)
        {
            return status::INVALID_ARGUMENT;
        }
        let pid = self.processes[process_index].as_ref().expect("process").pid;
        let limit = usize::try_from(count).unwrap_or(usize::MAX);
        let mut woken = 0usize;
        for thread in self.threads.iter_mut().flatten() {
            if woken == limit || thread.pid != pid {
                continue;
            }
            if matches!(thread.pending, PendingOperation::Futex(wait) if wait.address == address) {
                thread.pending = PendingOperation::None;
                thread.context.set_syscall_result(status::OK);
                let _ = self.scheduler.wake(thread.tid);
                woken += 1;
            }
        }
        i64::try_from(woken).unwrap_or(i64::MAX)
    }

    fn wake_expired_futexes(&mut self) {
        let now = self.monotonic_nanoseconds().max(0) as u64;
        for thread in self.threads.iter_mut().flatten() {
            if matches!(thread.pending, PendingOperation::Futex(wait) if wait.deadline_ns != u64::MAX && wait.deadline_ns <= now)
            {
                thread.pending = PendingOperation::None;
                thread.context.set_syscall_result(status::TIMED_OUT);
                let _ = self.scheduler.wake(thread.tid);
            }
        }
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
                Ok(UserPageBacking::Graphics(object)) => self.graphics.release_mappings(object, 1),
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

    fn graphics_buffer_create(&mut self, process_index: usize, descriptor_address: u64) -> i64 {
        let descriptor =
            match self.read_struct::<GraphicsBufferDesc>(process_index, descriptor_address) {
                Ok(descriptor) => descriptor,
                Err(error) => return error,
            };
        if descriptor.validate().is_err() {
            return status::INVALID_ARGUMENT;
        }
        let Some(slot) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .free_capability_slot()
        else {
            return status::LIMIT_REACHED;
        };
        let object = match self.graphics.create(descriptor) {
            Ok(object) => object,
            Err(error) => return error,
        };
        let mut rights = Rights::NONE;
        if descriptor.usage.contains(BufferUsage::CPU_READ)
            || descriptor.usage.contains(BufferUsage::SCANOUT)
            || descriptor.usage.contains(BufferUsage::TRANSFER_SOURCE)
        {
            rights = rights.union(Rights::READ);
        }
        if descriptor.usage.contains(BufferUsage::CPU_WRITE)
            || descriptor.usage.contains(BufferUsage::RENDER_TARGET)
        {
            rights = rights.union(Rights::WRITE);
        }
        if descriptor.usage.contains(BufferUsage::CPU_READ)
            || descriptor.usage.contains(BufferUsage::CPU_WRITE)
        {
            rights = rights.union(Rights::MAP);
        }
        if descriptor.memory_domains.contains(MemoryDomain::SHARED) {
            rights = rights.union(Rights::TRANSFER);
        }
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slot] = CapabilityEntry {
            kind: CapabilityKind::GraphicsBuffer(object),
            rights,
        };
        slot as i64
    }

    fn graphics_buffer_map(
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
            || flags.contains(VmFlags::EXECUTE)
        {
            return status::INVALID_ARGUMENT;
        }
        let pages = match checked_page_count(request.length) {
            Ok(pages) => pages,
            Err(error) => return error,
        };
        let required = vm_rights(flags).union(Rights::MAP);
        let object = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, required)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::GraphicsBuffer(object),
                ..
            }) => object,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        let offset_pages = request.offset / PAGE_SIZE;
        let (object_pages, maximum_flags) = match self.graphics.pages_and_flags(object) {
            Ok(info) => info,
            Err(error) => return error,
        };
        if flags.0 & !maximum_flags.0 != 0
            || offset_pages
                .checked_add(pages)
                .is_none_or(|end| end > object_pages as u64)
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
            let physical = match self
                .graphics
                .physical_page(object, (offset_pages + mapped) as usize)
            {
                Ok(physical) => physical,
                Err(error) => return error,
            };
            let result = self.processes[process_index]
                .as_mut()
                .expect("process")
                .address_space
                .map_graphics_page(
                    address + mapped * PAGE_SIZE,
                    physical,
                    actual_flags,
                    allowed_flags,
                    object,
                );
            if result.is_err() {
                for rollback in 0..mapped {
                    let _ = self.processes[process_index]
                        .as_mut()
                        .expect("process")
                        .address_space
                        .unmap_page(address + rollback * PAGE_SIZE);
                    self.graphics.release_mappings(object, 1);
                }
                return status::OUT_OF_MEMORY;
            }
            if self.graphics.retain_mapping(object).is_err() {
                let _ = self.processes[process_index]
                    .as_mut()
                    .expect("process")
                    .address_space
                    .unmap_page(address + mapped * PAGE_SIZE);
                for rollback in 0..mapped {
                    let _ = self.processes[process_index]
                        .as_mut()
                        .expect("process")
                        .address_space
                        .unmap_page(address + rollback * PAGE_SIZE);
                    self.graphics.release_mappings(object, 1);
                }
                return status::LIMIT_REACHED;
            }
            mapped += 1;
        }
        self.flush_process(process_index);
        address as i64
    }

    fn graphics_buffer_get_info(
        &self,
        process_index: usize,
        handle: Handle,
        descriptor_address: u64,
    ) -> i64 {
        if !self.user_writable(
            process_index,
            descriptor_address,
            size_of::<GraphicsBufferDesc>(),
        ) {
            return status::INVALID_ARGUMENT;
        }
        let object = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, Rights::READ)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::GraphicsBuffer(object),
                ..
            }) => object,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        let descriptor = match self.graphics.descriptor(object) {
            Ok(descriptor) => descriptor,
            Err(error) => return error,
        };
        self.write_struct(process_index, descriptor_address, &descriptor)
            .map(|_| status::OK)
            .unwrap_or(status::INVALID_ARGUMENT)
    }

    fn display_get_info(&self, process_index: usize, handle: Handle, info_address: u64) -> i64 {
        if !self.user_writable(process_index, info_address, size_of::<DisplayScanoutInfo>()) {
            return status::INVALID_ARGUMENT;
        }
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(handle, CapabilityKind::DisplayScanout(0), Rights::READ)
        {
            return error;
        }
        let info = match scanout::info() {
            Ok(info) => info,
            Err(error) => return display_status(error),
        };
        self.write_struct(process_index, info_address, &info)
            .map(|_| status::OK)
            .unwrap_or(status::INVALID_ARGUMENT)
    }

    fn display_atomic_present(
        &mut self,
        process_index: usize,
        scanout_handle: Handle,
        buffer_handle: Handle,
        request_address: u64,
    ) -> i64 {
        let request = match self.read_struct::<DisplayAtomicPresent>(process_index, request_address)
        {
            Ok(request) => request,
            Err(error) => return error,
        };
        if request.validate().is_err() {
            return status::INVALID_ARGUMENT;
        }
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(
                scanout_handle,
                CapabilityKind::DisplayScanout(0),
                Rights::WRITE,
            )
        {
            return error;
        }
        let object = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(buffer_handle, Rights::READ)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::GraphicsBuffer(object),
                ..
            }) => object,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        if self.display_present_sequence != self.display_completed_sequence {
            return status::BUSY;
        }
        let descriptor = match self.graphics.descriptor(object) {
            Ok(descriptor) => descriptor,
            Err(error) => return error,
        };
        let info = match scanout::info() {
            Ok(info) => info,
            Err(error) => return display_status(error),
        };
        if request.expected_mode_generation != info.mode_generation
            || descriptor.width != info.width
            || descriptor.height != info.height
            || descriptor.planes[0].stride_bytes < info.stride_bytes
            || !matches!(
                descriptor.format,
                rustos_abi::graphics_buffer::PixelFormatCode::B8G8R8X8_UNORM
                    | rustos_abi::graphics_buffer::PixelFormatCode::B8G8R8A8_UNORM
            )
        {
            return status::INVALID_ARGUMENT;
        }
        let sequence = self.display_present_sequence.wrapping_add(1).max(1);
        let graphics = &self.graphics;
        if let Err(error) = scanout::present_graphics(
            object,
            descriptor,
            |page| graphics.physical_page(object, page).ok(),
            sequence,
        ) {
            return display_status(error);
        }
        let now = self.monotonic_nanoseconds().max(0) as u64;
        let interval = match scanout::refresh_interval_ns() {
            Ok(interval) => interval,
            Err(error) => return display_status(error),
        };
        self.display_present_sequence = sequence;
        self.display_present_deadline_ns =
            next_refresh_deadline(now, request.target_time_ns.max(now), interval);
        sequence as i64
    }

    fn display_wait_vblank(
        &mut self,
        thread_index: usize,
        scanout_handle: Handle,
        request_address: u64,
    ) -> BlockingResult {
        let process_index = self
            .process_index(self.threads[thread_index].as_ref().expect("thread").pid)
            .expect("process");
        let request = match self.read_struct::<DisplayVblankWait>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return BlockingResult::Return(error),
        };
        if request.validate().is_err() {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(
                scanout_handle,
                CapabilityKind::DisplayScanout(0),
                Rights::WAIT,
            )
        {
            return BlockingResult::Return(error);
        }
        if request.sequence <= self.display_completed_sequence {
            return BlockingResult::Return(status::OK);
        }
        if request.sequence != self.display_present_sequence {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let now = self.monotonic_nanoseconds().max(0) as u64;
        if now >= self.display_present_deadline_ns {
            self.display_completed_sequence = request.sequence;
            return BlockingResult::Return(status::OK);
        }
        if request.timeout_ns == 0 {
            return BlockingResult::Return(status::TIMED_OUT);
        }
        let timeout_deadline_ns = if request.timeout_ns == SYNC_TIMEOUT_INFINITE {
            SYNC_TIMEOUT_INFINITE
        } else {
            now.saturating_add(request.timeout_ns)
        };
        let tid = self.threads[thread_index].as_ref().expect("thread").tid;
        if self.scheduler.block(tid).is_err() {
            return BlockingResult::Return(status::BUSY);
        }
        self.threads[thread_index].as_mut().expect("thread").pending =
            PendingOperation::DisplayVblank(PendingVblank {
                sequence: request.sequence,
                present_deadline_ns: self.display_present_deadline_ns,
                timeout_deadline_ns,
            });
        BlockingResult::Blocked
    }

    fn wake_display_vblank_waiters(&mut self) {
        let now = self.monotonic_nanoseconds().max(0) as u64;
        for index in 0..MAX_THREADS {
            let pending = self.threads[index].as_ref().map(|thread| thread.pending);
            let Some(PendingOperation::DisplayVblank(wait)) = pending else {
                continue;
            };
            let result = if now >= wait.present_deadline_ns {
                self.display_completed_sequence =
                    self.display_completed_sequence.max(wait.sequence);
                Some(status::OK)
            } else if wait.timeout_deadline_ns != SYNC_TIMEOUT_INFINITE
                && now >= wait.timeout_deadline_ns
            {
                Some(status::TIMED_OUT)
            } else {
                None
            };
            if let Some(result) = result {
                let thread = self.threads[index].as_mut().expect("thread");
                thread.pending = PendingOperation::None;
                thread.context.set_syscall_result(result);
                let _ = self.scheduler.wake(thread.tid);
            }
        }
    }

    fn gpu_get_info(&self, process_index: usize, handle: Handle, address: u64) -> i64 {
        if !self.user_writable(process_index, address, size_of::<GpuDeviceInfo>()) {
            return status::INVALID_ARGUMENT;
        }
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(handle, CapabilityKind::GpuRender(0), Rights::READ)
        {
            return error;
        }
        let info = match scanout::render_info() {
            Ok(info) => info,
            Err(error) => return gpu_status(error),
        };
        self.write_struct(process_index, address, &info)
            .map(|_| status::OK)
            .unwrap_or(status::INVALID_ARGUMENT)
    }

    fn gpu_context_create(
        &mut self,
        process_index: usize,
        render: Handle,
        request_address: u64,
    ) -> i64 {
        let request = match self.read_struct::<GpuContextCreate>(process_index, request_address) {
            Ok(request) if request.validate().is_ok() => request,
            _ => return status::INVALID_ARGUMENT,
        };
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(render, CapabilityKind::GpuRender(0), Rights::WRITE)
        {
            return error;
        }
        if self.gpu_context_active {
            return status::BUSY;
        }
        let Some(slot) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .free_capability_slot()
        else {
            return status::LIMIT_REACHED;
        };
        let name_length = request
            .debug_name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(request.debug_name.len());
        if let Err(error) = scanout::create_render_context(1, &request.debug_name[..name_length]) {
            return gpu_status(error);
        }
        self.gpu_context_active = true;
        self.gpu_imports = [None; MAX_GPU_IMPORTS];
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slot] = CapabilityEntry {
            kind: CapabilityKind::GpuContext(1),
            rights: Rights::READ.union(Rights::WRITE),
        };
        slot as i64
    }

    fn gpu_resource_import(
        &mut self,
        process_index: usize,
        context_handle: Handle,
        buffer_handle: Handle,
        request_address: u64,
    ) -> i64 {
        let _request = match self.read_struct::<GpuResourceImport>(process_index, request_address) {
            Ok(request) if request.validate().is_ok() => request,
            _ => return status::INVALID_ARGUMENT,
        };
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(context_handle, CapabilityKind::GpuContext(1), Rights::WRITE)
        {
            return error;
        }
        let object = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(buffer_handle, Rights::WRITE)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::GraphicsBuffer(object),
                ..
            }) => object,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        if self
            .gpu_imports
            .iter()
            .flatten()
            .any(|import| *import == object)
        {
            return status::BUSY;
        }
        let descriptor = match self.graphics.descriptor(object) {
            Ok(descriptor) => descriptor,
            Err(error) => return error,
        };
        if !descriptor.usage.contains(BufferUsage::RENDER_TARGET)
            || !descriptor.usage.contains(BufferUsage::SCANOUT)
            || descriptor.usage.contains(BufferUsage::CPU_WRITE)
            || !descriptor.memory_domains.contains(MemoryDomain::SYSTEM)
            || !descriptor.memory_domains.contains(MemoryDomain::SHARED)
        {
            return status::ACCESS_DENIED;
        }
        let backing = match self.graphics.contiguous_backing(object) {
            Ok(backing) => backing,
            Err(error) => return error,
        };
        let Some(import_slot) = self.gpu_imports.iter().position(Option::is_none) else {
            return status::LIMIT_REACHED;
        };
        // Удерживаем backing до публикации resource устройству. Так даже
        // редкая ошибка refcount не оставит активный DMA на освобождённую RAM.
        if self.graphics.retain_capability(object).is_err() {
            return status::LIMIT_REACHED;
        }
        let resource = match scanout::import_render_target(1, object, descriptor, backing) {
            Ok(resource) => resource,
            Err(error) => {
                self.graphics.release_capability(object);
                return gpu_status(error);
            }
        };
        self.gpu_imports[import_slot] = Some(object);
        resource as i64
    }

    fn gpu_resource_create(
        &mut self,
        process_index: usize,
        context_handle: Handle,
        request_address: u64,
    ) -> i64 {
        let request = match self.read_struct::<GpuResourceCreate>(process_index, request_address) {
            Ok(request) if request.validate().is_ok() => request,
            _ => return status::INVALID_ARGUMENT,
        };
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(context_handle, CapabilityKind::GpuContext(1), Rights::WRITE)
        {
            return error;
        }
        scanout::create_render_resource(1, request)
            .map(i64::from)
            .unwrap_or_else(gpu_status)
    }

    fn gpu_submit(
        &mut self,
        process_index: usize,
        context_handle: Handle,
        request_address: u64,
    ) -> i64 {
        let request = match self.read_struct::<GpuSubmit>(process_index, request_address) {
            Ok(request) if request.validate().is_ok() => request,
            _ => return status::INVALID_ARGUMENT,
        };
        if self.gpu_submission.is_some() {
            return status::BUSY;
        }
        if let Err(error) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(context_handle, CapabilityKind::GpuContext(1), Rights::WRITE)
        {
            return error;
        }
        let timeline = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(request.completion_timeline, Rights::WRITE)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::SyncTimeline(timeline),
                ..
            }) => timeline,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        if match self.timelines.value(timeline) {
            Ok(value) => request.completion_value <= value,
            Err(_) => true,
        } {
            return status::INVALID_ARGUMENT;
        }
        let mut commands = [0u8; GPU_MAX_COMMAND_BYTES as usize];
        let command_length = request.command_bytes as usize;
        if let Err(error) = self.copy_from_process(
            process_index,
            request.commands_address,
            &mut commands[..command_length],
        ) {
            return error;
        }
        // Timeline reference должен существовать ещё до публикации descriptor
        // в Virtqueue: очень быстрое устройство вправе завершить команду сразу.
        if self.timelines.retain(timeline).is_err() {
            return status::LIMIT_REACHED;
        }
        let fence = match scanout::submit_render(1, &commands[..command_length]) {
            Ok(fence) => fence,
            Err(error) => {
                let _ = self.timelines.release(timeline);
                return gpu_status(error);
            }
        };
        self.gpu_submission = Some(PendingGpuSubmission {
            fence,
            timeline,
            value: request.completion_value,
        });
        fence as i64
    }

    fn poll_gpu_completion(&mut self) {
        let Some(pending) = self.gpu_submission else {
            return;
        };
        let completion = match scanout::poll_render() {
            Ok(Some(completion)) => completion,
            Ok(None) => return,
            Err(_) => {
                self.finish_gpu_submission(pending, status::IO_ERROR);
                return;
            }
        };
        if completion.fence_id != pending.fence {
            // Синхронная display-команда могла завершиться раньше render
            // fence. Ожидаемый submission остаётся активным и будет опрошен
            // на следующем tick.
            self.gpu_last_fence = completion.fence_id;
            self.gpu_last_status = status::IO_ERROR;
            return;
        }
        let result = if completion.succeeded {
            status::OK
        } else {
            status::IO_ERROR
        };
        self.finish_gpu_submission(pending, result);
    }

    fn finish_gpu_submission(&mut self, pending: PendingGpuSubmission, result: i64) {
        self.gpu_last_fence = pending.fence;
        self.gpu_last_status = result;
        let _ = self.timelines.signal(pending.timeline, pending.value);
        let _ = self.timelines.release(pending.timeline);
        self.gpu_submission = None;
    }

    /// Bootstrap scheduler пока не имеет отдельного kernel idle thread. Если
    /// runnable user threads закончились ровно на ожидании GPU timeline,
    /// осушаем только этот fence и сразу будим waiter. При наличии другой
    /// работы completion остаётся неблокирующим timer bottom half.
    fn complete_idle_gpu_submission(&mut self) -> bool {
        let Some(pending) = self.gpu_submission else {
            return false;
        };
        let result = match scanout::drain_render(pending.fence) {
            Ok(completion) if completion.succeeded => status::OK,
            Ok(_) | Err(_) => status::IO_ERROR,
        };
        self.finish_gpu_submission(pending, result);
        true
    }

    fn gpu_completion_status(
        &self,
        process_index: usize,
        context_handle: Handle,
        fence: u64,
    ) -> i64 {
        if self.processes[process_index]
            .as_ref()
            .expect("process")
            .resolve(context_handle, CapabilityKind::GpuContext(1), Rights::READ)
            .is_err()
        {
            return status::ACCESS_DENIED;
        }
        if self
            .gpu_submission
            .is_some_and(|submission| submission.fence == fence)
        {
            return status::BUSY;
        }
        if fence == self.gpu_last_fence {
            self.gpu_last_status
        } else {
            status::NOT_FOUND
        }
    }

    fn release_gpu_context(&mut self, context: u8) {
        if context != 1 || !self.gpu_context_active {
            return;
        }
        if let Some(pending) = self.gpu_submission.take() {
            // Нельзя освободить GraphicsBuffer, пока host renderer ещё может
            // писать в его кадры. На штатном пути fence уже снят timer'ом;
            // этот drain обслуживает kill/crash renderd.
            let completion = scanout::drain_render(pending.fence);
            self.gpu_last_fence = pending.fence;
            self.gpu_last_status = if completion.is_ok_and(|done| done.succeeded) {
                status::OK
            } else {
                status::IO_ERROR
            };
            let _ = self.timelines.signal(pending.timeline, pending.value);
            let _ = self.timelines.release(pending.timeline);
        }
        scanout::destroy_render_context(u32::from(context));
        for object in self.gpu_imports.iter_mut().filter_map(Option::take) {
            self.graphics.release_capability(object);
        }
        self.gpu_context_active = false;
        self.gpu_last_fence = 0;
        self.gpu_last_status = status::NOT_FOUND;
    }

    fn sync_timeline_create(&mut self, process_index: usize, request_address: u64) -> i64 {
        let request = match self.read_struct::<SyncTimelineCreate>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if request.validate().is_err() {
            return status::INVALID_ARGUMENT;
        }
        let Some(slot) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .free_capability_slot()
        else {
            return status::LIMIT_REACHED;
        };
        let timeline = match self.timelines.create(request.initial_value) {
            Ok(timeline) => timeline,
            Err(error) => return timeline_status(error),
        };
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slot] = CapabilityEntry {
            kind: CapabilityKind::SyncTimeline(timeline),
            rights: Rights::WAIT.union(Rights::WRITE).union(Rights::TRANSFER),
        };
        slot as i64
    }

    fn sync_timeline_signal(&mut self, process_index: usize, request_address: u64) -> i64 {
        let request = match self.read_struct::<SyncTimelineSignal>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if request.validate().is_err() {
            return status::INVALID_ARGUMENT;
        }
        let timeline = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(request.timeline, Rights::WRITE)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::SyncTimeline(timeline),
                ..
            }) => timeline,
            Ok(_) => return status::ACCESS_DENIED,
            Err(error) => return error,
        };
        if let Err(error) = self.timelines.signal(timeline, request.value) {
            return timeline_status(error);
        }
        self.wake_ready_sync_waiters();
        status::OK
    }

    fn sync_timeline_wait(&mut self, thread_index: usize, request_address: u64) -> BlockingResult {
        let process_index = self
            .process_index(self.threads[thread_index].as_ref().expect("thread").pid)
            .expect("process");
        let request = match self.read_struct::<SyncTimelineWait>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return BlockingResult::Return(error),
        };
        if request.validate().is_err() {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let timeline = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(request.timeline, Rights::WAIT)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::SyncTimeline(timeline),
                ..
            }) => timeline,
            Ok(_) => return BlockingResult::Return(status::ACCESS_DENIED),
            Err(error) => return BlockingResult::Return(error),
        };
        let slot = match self.prepare_sync_wait_slot() {
            Ok(slot) => slot,
            Err(error) => return BlockingResult::Return(error),
        };
        if let Err(error) = self.timelines.retain(timeline) {
            return BlockingResult::Return(timeline_status(error));
        }
        self.sync_waits[slot].points[0] = PendingSyncPoint {
            timeline,
            value: request.value,
        };
        self.sync_waits[slot].point_count = 1;
        self.commit_sync_wait(thread_index, slot, SyncWaitMode::ALL, request.timeout_ns)
    }

    fn sync_timeline_wait_many(
        &mut self,
        thread_index: usize,
        request_address: u64,
    ) -> BlockingResult {
        let process_index = self
            .process_index(self.threads[thread_index].as_ref().expect("thread").pid)
            .expect("process");
        let request = match self.read_struct::<SyncWaitMany>(process_index, request_address) {
            Ok(request) => request,
            Err(error) => return BlockingResult::Return(error),
        };
        if request.validate().is_err() {
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let memory = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(request.points_memory, Rights::READ)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::SharedMemory(memory),
                ..
            }) => memory,
            Ok(_) => return BlockingResult::Return(status::ACCESS_DENIED),
            Err(error) => return BlockingResult::Return(error),
        };
        let slot = match self.prepare_sync_wait_slot() {
            Ok(slot) => slot,
            Err(error) => return BlockingResult::Return(error),
        };
        for index in 0..request.point_count as usize {
            let offset = request.points_offset + index as u64 * size_of::<SyncPoint>() as u64;
            let point = match self.read_shared_struct::<SyncPoint>(memory, offset) {
                Ok(point) => point,
                Err(error) => {
                    self.release_sync_wait_slot(slot);
                    return BlockingResult::Return(error);
                }
            };
            if point.validate().is_err() || point.is_none() {
                self.release_sync_wait_slot(slot);
                return BlockingResult::Return(status::INVALID_ARGUMENT);
            }
            let timeline = match self.processes[process_index]
                .as_ref()
                .expect("process")
                .capability(point.timeline, Rights::WAIT)
            {
                Ok(CapabilityEntry {
                    kind: CapabilityKind::SyncTimeline(timeline),
                    ..
                }) => timeline,
                Ok(_) => {
                    self.release_sync_wait_slot(slot);
                    return BlockingResult::Return(status::ACCESS_DENIED);
                }
                Err(error) => {
                    self.release_sync_wait_slot(slot);
                    return BlockingResult::Return(error);
                }
            };
            if let Err(error) = self.timelines.retain(timeline) {
                self.release_sync_wait_slot(slot);
                return BlockingResult::Return(timeline_status(error));
            }
            self.sync_waits[slot].points[index] = PendingSyncPoint {
                timeline,
                value: point.value,
            };
            self.sync_waits[slot].point_count += 1;
        }
        self.commit_sync_wait(thread_index, slot, request.mode, request.timeout_ns)
    }

    fn prepare_sync_wait_slot(&self) -> Result<usize, i64> {
        self.sync_waits
            .iter()
            .position(|wait| !wait.used && wait.point_count == 0)
            .ok_or(status::LIMIT_REACHED)
    }

    fn commit_sync_wait(
        &mut self,
        thread_index: usize,
        slot: usize,
        mode: SyncWaitMode,
        timeout_ns: u64,
    ) -> BlockingResult {
        self.sync_waits[slot].mode = mode;
        if self.sync_wait_reached(slot) {
            self.release_sync_wait_slot(slot);
            return BlockingResult::Return(status::OK);
        }
        if timeout_ns == 0 {
            self.release_sync_wait_slot(slot);
            return BlockingResult::Return(status::TIMED_OUT);
        }
        let now = self.monotonic_nanoseconds().max(0) as u64;
        let deadline_ns = if timeout_ns == SYNC_TIMEOUT_INFINITE {
            SYNC_TIMEOUT_INFINITE
        } else {
            now.saturating_add(timeout_ns)
        };
        let tid = self.threads[thread_index].as_ref().expect("thread").tid;
        if self.scheduler.block(tid).is_err() {
            self.release_sync_wait_slot(slot);
            return BlockingResult::Return(status::BUSY);
        }
        self.sync_waits[slot].used = true;
        self.sync_waits[slot].thread = tid;
        self.sync_waits[slot].deadline_ns = deadline_ns;
        self.threads[thread_index].as_mut().expect("thread").pending =
            PendingOperation::Sync(slot as u8);
        BlockingResult::Blocked
    }

    fn sync_wait_reached(&self, slot: usize) -> bool {
        let wait = &self.sync_waits[slot];
        let reached = |point: &PendingSyncPoint| {
            self.timelines
                .reached(point.timeline, point.value)
                .unwrap_or(false)
        };
        if wait.mode == SyncWaitMode::ANY {
            wait.points[..wait.point_count].iter().any(reached)
        } else {
            wait.points[..wait.point_count].iter().all(reached)
        }
    }

    fn wake_ready_sync_waiters(&mut self) {
        for slot in 0..MAX_SYNC_WAITS {
            if self.sync_waits[slot].used && self.sync_wait_reached(slot) {
                self.finish_sync_wait(slot, status::OK);
            }
        }
    }

    fn wake_expired_sync_waiters(&mut self) {
        let now = self.monotonic_nanoseconds().max(0) as u64;
        for slot in 0..MAX_SYNC_WAITS {
            let wait = self.sync_waits[slot];
            if wait.used && wait.deadline_ns != SYNC_TIMEOUT_INFINITE && wait.deadline_ns <= now {
                self.finish_sync_wait(slot, status::TIMED_OUT);
            }
        }
    }

    fn finish_sync_wait(&mut self, slot: usize, result: i64) {
        let tid = self.sync_waits[slot].thread;
        self.release_sync_wait_slot(slot);
        let Some(thread_index) = self.thread_index(tid) else {
            return;
        };
        if matches!(self.threads[thread_index].as_ref().expect("thread").pending, PendingOperation::Sync(wait_slot) if wait_slot as usize == slot)
        {
            let thread = self.threads[thread_index].as_mut().expect("thread");
            thread.pending = PendingOperation::None;
            thread.context.set_syscall_result(result);
            let _ = self.scheduler.wake(tid);
        }
    }

    fn release_sync_wait_slot(&mut self, slot: usize) {
        let count = self.sync_waits[slot].point_count;
        for point in self.sync_waits[slot].points.iter().take(count) {
            let _ = self.timelines.release(point.timeline);
        }
        self.sync_waits[slot] = SyncWaitSlot::EMPTY;
    }

    fn cancel_sync_wait_for_thread(&mut self, tid: ThreadId) {
        let Some(thread_index) = self.thread_index(tid) else {
            return;
        };
        let PendingOperation::Sync(slot) =
            self.threads[thread_index].as_ref().expect("thread").pending
        else {
            return;
        };
        self.release_sync_wait_slot(slot as usize);
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

    fn handle_duplicate(&mut self, process_index: usize, handle: Handle, requested: Rights) -> i64 {
        let source = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, Rights::NONE)
        {
            Ok(source) => source,
            Err(error) => return error,
        };
        if matches!(
            source.kind,
            CapabilityKind::DisplayScanout(_)
                | CapabilityKind::GpuRender(_)
                | CapabilityKind::GpuContext(_)
        ) {
            return status::ACCESS_DENIED;
        }
        let rights = match derive_capability_rights(source.rights, requested) {
            Ok(rights) => rights,
            Err(CapabilityTransferError::EmptyRights) => return status::INVALID_ARGUMENT,
            Err(_) => return status::ACCESS_DENIED,
        };
        let Some(slot) = self.processes[process_index]
            .as_ref()
            .expect("process")
            .free_capability_slot()
        else {
            return status::LIMIT_REACHED;
        };
        let entry = CapabilityEntry {
            kind: source.kind,
            rights,
        };
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slot] = entry;
        self.retain_capability(entry);
        slot as i64
    }

    fn pipe_create(&mut self, process_index: usize, result_address: u64) -> i64 {
        if !self.user_writable(process_index, result_address, size_of::<PipeCreateResult>()) {
            return status::INVALID_ARGUMENT;
        }
        let mut slots = [0usize; 2];
        let mut count = 0usize;
        for slot in 1..MAX_CAPABILITIES {
            if self.processes[process_index]
                .as_ref()
                .expect("process")
                .capabilities[slot]
                .kind
                == CapabilityKind::Empty
            {
                slots[count] = slot;
                count += 1;
                if count == slots.len() {
                    break;
                }
            }
        }
        if count != slots.len() {
            return status::LIMIT_REACHED;
        }
        let pipe = match self.pipes.create() {
            Ok(pipe) => pipe,
            Err(error) => return error,
        };
        let reader = CapabilityEntry {
            kind: CapabilityKind::Pipe(pipe),
            rights: Rights::READ.union(Rights::TRANSFER),
        };
        let writer = CapabilityEntry {
            kind: CapabilityKind::Pipe(pipe),
            rights: Rights::WRITE.union(Rights::TRANSFER),
        };
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slots[0]] = reader;
        self.processes[process_index]
            .as_mut()
            .expect("process")
            .capabilities[slots[1]] = writer;
        let result = PipeCreateResult {
            reader: Handle(slots[0] as u32),
            writer: Handle(slots[1] as u32),
            version: PIPE_ABI_VERSION,
            reserved: 0,
        };
        if self
            .write_struct(process_index, result_address, &result)
            .is_err()
        {
            self.processes[process_index]
                .as_mut()
                .expect("process")
                .capabilities[slots[0]] = EMPTY_CAPABILITY;
            self.processes[process_index]
                .as_mut()
                .expect("process")
                .capabilities[slots[1]] = EMPTY_CAPABILITY;
            self.pipes.release(pipe, reader.rights);
            self.pipes.release(pipe, writer.rights);
            return status::INVALID_ARGUMENT;
        }
        status::OK
    }

    fn pipe_read(
        &mut self,
        thread_index: usize,
        handle: Handle,
        buffer: u64,
        length: u64,
    ) -> BlockingResult {
        let pid = self.threads[thread_index].as_ref().expect("thread").pid;
        let process_index = self.process_index(pid).expect("process");
        let pipe = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, Rights::READ)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::Pipe(pipe),
                ..
            }) => pipe,
            Ok(_) => {
                log_pipe_error("read-kind", status::ACCESS_DENIED);
                return BlockingResult::Return(status::ACCESS_DENIED);
            }
            Err(error) => {
                log_pipe_error("read-capability", error);
                return BlockingResult::Return(error);
            }
        };
        if length == 0 {
            return BlockingResult::Return(0);
        }
        let count = usize::try_from(length)
            .unwrap_or(usize::MAX)
            .min(PIPE_BUFFER_BYTES);
        if !self.user_writable(process_index, buffer, count) {
            log_pipe_error("read-buffer", status::INVALID_ARGUMENT);
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let mut temporary = [0u8; PIPE_BUFFER_BYTES];
        let read = {
            let object = match self.pipes.get_mut(pipe) {
                Ok(object) => object,
                Err(error) => {
                    log_pipe_error("read-object", error);
                    return BlockingResult::Return(error);
                }
            };
            if object.length == 0 {
                if object.writers == 0 {
                    return BlockingResult::Return(0);
                }
                let tid = self.threads[thread_index].as_ref().expect("thread").tid;
                if self.scheduler.block(tid).is_err() {
                    return BlockingResult::Return(status::BUSY);
                }
                self.threads[thread_index].as_mut().expect("thread").pending =
                    PendingOperation::Pipe(PendingPipe { pipe, write: false });
                return BlockingResult::Blocked;
            }
            object.read(&mut temporary[..count])
        };
        if self.processes[process_index]
            .as_ref()
            .expect("process")
            .address_space
            .copy_to_user(buffer, &temporary[..read])
            .is_err()
        {
            log_pipe_error("read-copy", status::INVALID_ARGUMENT);
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        self.wake_pipe_waiters(pipe, true);
        BlockingResult::Return(read as i64)
    }

    fn pipe_write(
        &mut self,
        thread_index: usize,
        handle: Handle,
        buffer: u64,
        length: u64,
    ) -> BlockingResult {
        let pid = self.threads[thread_index].as_ref().expect("thread").pid;
        let process_index = self.process_index(pid).expect("process");
        let pipe = match self.processes[process_index]
            .as_ref()
            .expect("process")
            .capability(handle, Rights::WRITE)
        {
            Ok(CapabilityEntry {
                kind: CapabilityKind::Pipe(pipe),
                ..
            }) => pipe,
            Ok(_) => {
                log_pipe_error("write-kind", status::ACCESS_DENIED);
                return BlockingResult::Return(status::ACCESS_DENIED);
            }
            Err(error) => {
                log_pipe_error("write-capability", error);
                return BlockingResult::Return(error);
            }
        };
        if length == 0 {
            return BlockingResult::Return(0);
        }
        let count = usize::try_from(length)
            .unwrap_or(usize::MAX)
            .min(PIPE_BUFFER_BYTES);
        let mut temporary = [0u8; PIPE_BUFFER_BYTES];
        if self
            .copy_from_process(process_index, buffer, &mut temporary[..count])
            .is_err()
        {
            log_pipe_error("write-buffer", status::INVALID_ARGUMENT);
            return BlockingResult::Return(status::INVALID_ARGUMENT);
        }
        let written = {
            let object = match self.pipes.get_mut(pipe) {
                Ok(object) => object,
                Err(error) => {
                    log_pipe_error("write-object", error);
                    return BlockingResult::Return(error);
                }
            };
            if object.readers == 0 {
                log_pipe_error("write-no-readers", status::IO_ERROR);
                return BlockingResult::Return(status::IO_ERROR);
            }
            if object.length == PIPE_BUFFER_BYTES {
                let tid = self.threads[thread_index].as_ref().expect("thread").tid;
                if self.scheduler.block(tid).is_err() {
                    return BlockingResult::Return(status::BUSY);
                }
                self.threads[thread_index].as_mut().expect("thread").pending =
                    PendingOperation::Pipe(PendingPipe { pipe, write: true });
                return BlockingResult::Blocked;
            }
            object.write(&temporary[..count])
        };
        self.wake_pipe_waiters(pipe, false);
        BlockingResult::Return(written as i64)
    }

    fn wake_pipe_waiters(&mut self, pipe: u16, writers: bool) {
        for thread in self.threads.iter_mut().flatten() {
            if matches!(thread.pending, PendingOperation::Pipe(wait) if wait.pipe == pipe && wait.write == writers)
            {
                thread.pending = PendingOperation::None;
                thread.context.set_syscall_result(status::BUSY);
                let _ = self.scheduler.wake(thread.tid);
            }
        }
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
        let parent = self.processes[process_index]
            .as_ref()
            .expect("process")
            .parent;
        for endpoint in 0..MAX_ENDPOINTS {
            if self.endpoints[endpoint].receiver != pid {
                continue;
            }
            let parent_can_receive = self.process_index(parent).is_some_and(|parent_index| {
                self.processes[parent_index]
                    .as_ref()
                    .expect("parent")
                    .capabilities
                    .iter()
                    .any(|entry| {
                        entry.kind == CapabilityKind::Endpoint(endpoint as u8)
                            && entry.rights.contains(Rights::RECEIVE)
                    })
            });
            self.endpoints[endpoint].receiver = if parent_can_receive {
                parent
            } else {
                ProcessId::KERNEL
            };
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
        for graphics_index in 0..MAX_GRAPHICS_BUFFERS {
            let object = graphics_id(graphics_index, self.graphics.generation_at(graphics_index));
            let count = self.processes[process_index]
                .as_ref()
                .expect("process")
                .address_space
                .graphics_mapping_pages(object);
            if count != 0 {
                self.graphics.release_mappings(object, count);
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
        let (pid, reclaim_address, reclaim_length) = {
            let thread = self.threads[index].as_ref().expect("thread");
            (thread.pid, thread.reclaim_address, thread.reclaim_length)
        };
        if reclaim_address != 0 && reclaim_length != 0 {
            if let Some(process_index) = self.process_index(pid) {
                let _ = self.vm_unmap(process_index, reclaim_address, reclaim_length);
            }
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
        match entry.kind {
            CapabilityKind::SharedMemory(object) => {
                let _ = self.shared.retain_capability(object);
            }
            CapabilityKind::GraphicsBuffer(object) => {
                let _ = self.graphics.retain_capability(object);
            }
            CapabilityKind::SyncTimeline(timeline) => {
                let _ = self.timelines.retain(timeline);
            }
            CapabilityKind::Pipe(pipe) => {
                let _ = self.pipes.retain(pipe, entry.rights);
            }
            _ => {}
        }
    }

    fn release_capability(&mut self, entry: CapabilityEntry) {
        match entry.kind {
            CapabilityKind::SharedMemory(object) => self.shared.release_capability(object),
            CapabilityKind::GraphicsBuffer(object) => self.graphics.release_capability(object),
            CapabilityKind::SyncTimeline(timeline) => {
                let _ = self.timelines.release(timeline);
            }
            CapabilityKind::Pipe(pipe) => {
                let had_read = entry.rights.contains(Rights::READ);
                let had_write = entry.rights.contains(Rights::WRITE);
                self.pipes.release(pipe, entry.rights);
                if had_write {
                    self.wake_pipe_waiters(pipe, false);
                }
                if had_read {
                    self.wake_pipe_waiters(pipe, true);
                }
            }
            CapabilityKind::GpuContext(context) => self.release_gpu_context(context),
            _ => {}
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

    fn read_shared_struct<T: Copy>(&self, object: u16, offset: u64) -> Result<T, i64> {
        let mut value = MaybeUninit::<T>::uninit();
        // SAFETY: `value` занимает ровно size_of::<T>(); объектный reader
        // полностью заполняет slice либо возвращает ошибку.
        let bytes =
            unsafe { slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
        self.shared.copy_bytes(object, offset, bytes)?;
        // SAFETY: все bytes T записаны выше; wire T ограничен Copy records.
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

    fn has_runnable_threads(&self) -> bool {
        self.threads.iter().flatten().any(|thread| {
            !thread.exited
                && self.scheduler.info(thread.tid).is_ok_and(|info| {
                    matches!(info.state, ThreadState::Ready | ThreadState::Running)
                })
        })
    }

    fn cleanup(&mut self) {
        for slot in 0..MAX_SYNC_WAITS {
            if self.sync_waits[slot].used || self.sync_waits[slot].point_count != 0 {
                self.release_sync_wait_slot(slot);
            }
        }
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
                for graphics_index in 0..MAX_GRAPHICS_BUFFERS {
                    let object =
                        graphics_id(graphics_index, self.graphics.generation_at(graphics_index));
                    let count = process.address_space.graphics_mapping_pages(object);
                    if count != 0 {
                        self.graphics.release_mappings(object, count);
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
        self.graphics.cleanup();
        self.pipes.cleanup();
    }
}

enum BlockingResult {
    Return(i64),
    Blocked,
}

static mut MANAGER: ProcessManager = ProcessManager::empty();
static mut ACTIVE_MANAGER: *mut ProcessManager = ptr::null_mut();
static mut INTERACTIVE_SERVICES_READY: bool = false;
static mut INTERACTIVE_GRAPHICS_SERVICES: Option<GraphicsServices> = None;
static mut GRAPHICS_RESTARTS: u8 = 0;

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
        "system/bin/preempt-a.rune",
        PriorityClass::Interactive,
        [0, syscall::ABI_VERSION, 0],
        21,
        0,
        [EMPTY_CAPABILITY; MAX_CAPABILITIES],
    )?;
    manager.spawn(
        "system/bin/preempt-b.rune",
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
        "system/bin/fault-test.rune",
        PriorityClass::Interactive,
        [0, syscall::ABI_VERSION, 0],
        status::FAULT as i32,
        arch::ILLEGAL_INSTRUCTION_EXCEPTION,
        [EMPTY_CAPABILITY; MAX_CAPABILITIES],
    )?;
    manager.spawn(
        "system/bin/preempt-b.rune",
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

    // Разный priority делает обе допустимые IPC-ветви детерминированными:
    // сначала RECEIVE -> block/wake, затем SEND -> bounded queue -> receive.
    // Раньше равный priority связывал результат теста с timer timing.
    run_ipc_phase(
        manager,
        PriorityClass::System,
        PriorityClass::Interactive,
        1,
    )?;
    run_ipc_phase(
        manager,
        PriorityClass::Interactive,
        PriorityClass::System,
        0,
    )?;
    serial::put_str("[ipc] queued block/wake and attenuated VFS capability verified\n");

    manager.begin_phase();
    let mut lifecycle_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    lifecycle_caps[VFS_ROOT_SLOT] = CapabilityEntry {
        kind: CapabilityKind::VfsRoot,
        rights: Rights::READ.union(Rights::EXECUTE).union(Rights::TRANSFER),
    };
    manager.spawn(
        "system/bin/abi-lifecycle.rune",
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

    if run_graphics_service_phase(manager)? {
        serial::put_str(
            "[graphics-abi-v7] graphics-buffer sync-timeline atomic-present supervisor-restart verified\n",
        );
        if scanout::render_info().is_ok() {
            serial::put_str(
                "[virgl] ring3 renderd async-fence triangle zero-copy scanout verified\n",
            );
        } else {
            serial::put_str("[virgl] unavailable: device did not negotiate VIRTIO_GPU_F_VIRGL\n");
        }
    } else {
        serial::put_str("[graphics] native scanout unavailable; kernel fallback remains active\n");
    }

    run_std_startup_phase(manager)?;
    serial::put_str("[std-startup] ordinary fn main argv and process-local environment verified\n");

    run_vfs_phase(manager, "system/bin/std-smoke.rune", true)?;
    serial::put_str(
        "[std] allocator fs threads futex process pipes stdio native SDK and VFS executable verified in ring3 RUNE\n",
    );

    // Настоящий VFS vertical slice: filesystem parser и pathname policy живут
    // в ring 3. Только vfsd получает raw block capability, клиент видит лишь
    // endpoint и `vfs.dll` API. Второй запуск server доказывает persistence.
    run_vfs_phase(manager, "system/bin/vfs-test.rune", false)?;
    serial::put_str(
        "[vfsd] open/read/write/seek/readdir/create/rename over shared memory verified\n",
    );
    run_vfs_phase(manager, "system/bin/vfs-persistence.rune", false)?;
    serial::put_str("[vfsd] restart recovered committed VaraniaFS metadata and file data\n");
    run_vfs_phase(manager, "system/bin/loader-test.rune", true)?;
    serial::put_str(
        "[loader] RUNE interfaces imports ABI TLS RELRO and cross-process shared RX verified\n",
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

pub(super) fn start_interactive_services() -> Result<(), ProcessError> {
    const SERVER_ENDPOINT: u8 = 1;
    const SERVER_SLOT: usize = 2;
    const DEVICE_SLOT: usize = 3;

    let manager = unsafe { &mut *ptr::addr_of_mut!(MANAGER) };
    manager.begin_phase();
    let mut capabilities = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    capabilities[SERVER_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(SERVER_ENDPOINT),
        rights: Rights::RECEIVE,
    };
    capabilities[DEVICE_SLOT] = CapabilityEntry {
        kind: CapabilityKind::BlockDevice(0),
        rights: Rights::READ.union(Rights::WRITE),
    };
    let server = manager.spawn_internal(
        "system/bin/vfsd.rune",
        SpawnOptions {
            parent: ProcessId::KERNEL,
            priority: PriorityClass::System,
            boot_arguments: [SERVER_SLOT as u64, DEVICE_SLOT as u64, syscall::ABI_VERSION],
            expected: None,
        },
        capabilities,
        None,
    )?;
    manager.endpoints[SERVER_ENDPOINT as usize].receiver = server;
    let graphics = if scanout::info().is_ok() {
        Some(spawn_graphics_services(manager)?)
    } else {
        None
    };

    // Все доступные сервисы доходят до blocking receive; отсутствие runnable
    // threads возвращает управление bootstrap GUI, не завершая процессы.
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    if graphics.is_some_and(|services| !graphics_services_blocked(manager, services)) {
        manager.cleanup();
        return Err(ProcessError::UnexpectedExit);
    }
    unsafe { INTERACTIVE_SERVICES_READY = true };
    unsafe { INTERACTIVE_GRAPHICS_SERVICES = graphics };
    unsafe { GRAPHICS_RESTARTS = 0 };
    serial::put_str("[services] persistent ring3 vfsd ready for GUI terminal\n");
    if graphics.is_some() {
        if scanout::render_info().is_ok() {
            serial::put_str(
                "[supervisor] persistent renderd/compositord/displayd services ready\n",
            );
        } else {
            serial::put_str(
                "[supervisor] persistent displayd/compositord atomic-present services ready\n",
            );
        }
    } else {
        serial::put_str("[supervisor] display services omitted: firmware scanout fallback\n");
    }
    Ok(())
}

/// Короткий supervisor tick из bootstrap GUI loop. В норме все graphics
/// threads blocked и функция ничего не планирует. После user fault пара
/// полностью reaped/restarted; kernel desktop при этом продолжает работать.
pub(super) fn pump_interactive_services() -> Result<(), ProcessError> {
    if !unsafe { INTERACTIVE_SERVICES_READY } {
        return Err(ProcessError::UnexpectedExit);
    }
    let manager = unsafe { &mut *ptr::addr_of_mut!(MANAGER) };
    let Some(services) = (unsafe { INTERACTIVE_GRAPHICS_SERVICES }) else {
        // Firmware framebuffer не имеет transferable hardware authority.
        // VFS и kernel recovery desktop продолжают работать без displayd.
        return Ok(());
    };
    if graphics_services_blocked(manager, services) {
        return Ok(());
    }
    if manager.has_runnable_threads() {
        manager.run()?;
        if graphics_services_blocked(manager, services) {
            return Ok(());
        }
    }
    let restarts = unsafe { GRAPHICS_RESTARTS };
    if restarts >= 3 {
        return Err(ProcessError::UnexpectedExit);
    }
    stop_service(manager, services.display, 81);
    stop_service(manager, services.compositor, 82);
    if let Some(render) = services.render {
        stop_service(manager, render, 83);
    }
    manager.recover_display_master_after_exit();
    manager.endpoints[0] = Endpoint::EMPTY;
    manager.endpoints[3] = Endpoint::EMPTY;
    manager.endpoints[4] = Endpoint::EMPTY;
    manager.endpoints[5] = Endpoint::EMPTY;
    let restarted = spawn_graphics_services(manager)?;
    manager.run()?;
    if !graphics_services_blocked(manager, restarted) {
        return Err(ProcessError::UnexpectedExit);
    }
    unsafe { INTERACTIVE_GRAPHICS_SERVICES = Some(restarted) };
    unsafe { GRAPHICS_RESTARTS = restarts + 1 };
    serial::put_str("[supervisor] display stack restarted count=");
    serial::put_u32(u32::from(restarts + 1));
    serial::put_str("\n");
    Ok(())
}

pub(super) fn run_interactive_command(
    command: &str,
    output: &mut [u8],
) -> Result<InteractiveExit, ProcessError> {
    const SERVER_ENDPOINT: u8 = 1;
    const REPLY_ENDPOINT: u8 = 2;
    const SERVER_SLOT: usize = 2;
    const REPLY_SLOT: usize = 3;
    const STDOUT_SLOT: usize = 4;
    const STDERR_SLOT: usize = 5;

    if !unsafe { INTERACTIVE_SERVICES_READY } {
        return Err(ProcessError::UnexpectedExit);
    }
    let mut words = command.split_ascii_whitespace();
    let target = words.next().ok_or(ProcessError::MissingImage)?;
    if target.is_empty() || !target.starts_with('/') || target.as_bytes().contains(&0) {
        return Err(ProcessError::MissingImage);
    }

    let mut start = SpawnData {
        arguments: [0; MAX_ARGUMENT_BYTES],
        argument_length: 0,
        argument_count: 0,
        environment: [0; MAX_ENVIRONMENT_BYTES],
        environment_length: 0,
        environment_count: 0,
        capabilities: [StartupCapability::EMPTY; PROCESS_SPAWN_MAX_CAPABILITIES],
        capability_count: 5,
    };
    for argument in core::iter::once("rune-runner")
        .chain(core::iter::once(target))
        .chain(words)
    {
        let end = start
            .argument_length
            .checked_add(argument.len() + 1)
            .ok_or(ProcessError::AddressSpace)?;
        if end > start.arguments.len() || argument.as_bytes().contains(&0) {
            return Err(ProcessError::AddressSpace);
        }
        start.arguments[start.argument_length..end - 1].copy_from_slice(argument.as_bytes());
        start.argument_length = end;
        start.argument_count += 1;
    }
    const ENVIRONMENT: &[u8] = b"PWD=/\0HOME=/home\0TMPDIR=/tmp\0";
    start.environment[..ENVIRONMENT.len()].copy_from_slice(ENVIRONMENT);
    start.environment_length = ENVIRONMENT.len();
    start.environment_count = 3;

    let manager = unsafe { &mut *ptr::addr_of_mut!(MANAGER) };
    if manager.endpoints[SERVER_ENDPOINT as usize].receiver == ProcessId::KERNEL {
        return Err(ProcessError::UnexpectedExit);
    }
    let pipe = manager
        .pipes
        .create()
        .map_err(|_| ProcessError::AddressSpace)?;
    let mut capabilities = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    capabilities[VFS_ROOT_SLOT] = CapabilityEntry {
        kind: CapabilityKind::VfsRoot,
        rights: Rights::READ.union(Rights::EXECUTE).union(Rights::TRANSFER),
    };
    capabilities[SERVER_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(SERVER_ENDPOINT),
        rights: Rights::SEND.union(Rights::TRANSFER),
    };
    capabilities[REPLY_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(REPLY_ENDPOINT),
        rights: Rights::SEND.union(Rights::RECEIVE).union(Rights::TRANSFER),
    };
    let writer = CapabilityEntry {
        kind: CapabilityKind::Pipe(pipe),
        rights: Rights::WRITE.union(Rights::TRANSFER),
    };
    capabilities[STDOUT_SLOT] = writer;
    capabilities[STDERR_SLOT] = writer;
    start.capabilities[0] = StartupCapability {
        role: StartupRole::EXECUTABLE_NAMESPACE,
        flags: 0,
        handle: Handle(VFS_ROOT_SLOT as u32),
        rights: capabilities[VFS_ROOT_SLOT].rights,
    };
    start.capabilities[1] = StartupCapability {
        role: StartupRole::VFS,
        flags: 0,
        handle: Handle(SERVER_SLOT as u32),
        rights: capabilities[SERVER_SLOT].rights,
    };
    start.capabilities[2] = StartupCapability {
        role: StartupRole::VFS_REPLY,
        flags: 0,
        handle: Handle(REPLY_SLOT as u32),
        rights: capabilities[REPLY_SLOT].rights,
    };
    start.capabilities[3] = StartupCapability {
        role: StartupRole::STDOUT,
        flags: 0,
        handle: Handle(STDOUT_SLOT as u32),
        rights: writer.rights,
    };
    start.capabilities[4] = StartupCapability {
        role: StartupRole::STDERR,
        flags: 0,
        handle: Handle(STDERR_SLOT as u32),
        rights: writer.rights,
    };

    let child = match manager.spawn_internal(
        "system/bin/rune-runner.rune",
        SpawnOptions {
            parent: ProcessId::KERNEL,
            priority: PriorityClass::Interactive,
            boot_arguments: [0; 3],
            expected: None,
        },
        capabilities,
        Some(&start),
    ) {
        Ok(child) => child,
        Err(error) => {
            manager
                .pipes
                .release(pipe, Rights::READ.union(Rights::WRITE));
            return Err(error);
        }
    };
    // create() резервирует исходную writer reference; две реальные writer
    // capabilities уже учтены spawn_internal, поэтому исходную закрываем.
    manager.pipes.release(pipe, Rights::WRITE);
    manager.endpoints[REPLY_ENDPOINT as usize].receiver = child;

    let mut captured = 0usize;
    let reason = loop {
        manager.run()?;
        let mut chunk = [0u8; PIPE_BUFFER_BYTES];
        let read = manager
            .pipes
            .get_mut(pipe)
            .map_err(|_| ProcessError::UnexpectedExit)?
            .read(&mut chunk);
        let copied = read.min(output.len().saturating_sub(captured));
        output[captured..captured + copied].copy_from_slice(&chunk[..copied]);
        captured += copied;
        if read != 0 {
            manager.wake_pipe_waiters(pipe, true);
        }
        let process = manager
            .processes
            .iter()
            .flatten()
            .find(|process| process.pid == child)
            .ok_or(ProcessError::UnexpectedExit)?;
        if process.exited {
            break process.exit_reason;
        }
        if read == 0 {
            return Err(ProcessError::UnexpectedExit);
        }
    };

    manager.reap_process(child);
    manager.pipes.release(pipe, Rights::READ);
    serial::put_str("[terminal-run] path=");
    serial::put_str(target);
    serial::put_str(" status=");
    serial::put_u32(reason.status as u32);
    serial::put_str(" exception=");
    serial::put_u32(u32::from(reason.exception));
    serial::put_str(" output=");
    serial::put_u32(captured as u32);
    serial::put_str("\n");
    Ok(InteractiveExit {
        output_length: captured,
        status: reason.status,
        exception: reason.exception,
    })
}

/// Запускает программу тем же путём, которым `process_spawn` создаёт обычное
/// приложение: с versioned ProcessStartInfo вместо приватных boot-регистров.
/// Благодаря этому тест защищает всю цепочку kernel -> RustOS CRT -> std::rt.
fn run_std_startup_phase(manager: &mut ProcessManager) -> Result<(), ProcessError> {
    const ARGUMENTS: &[u8] = b"std-main\0--self-test\0";
    const ENVIRONMENT: &[u8] = b"RUSTOS_MODE=developer\0";

    let mut start = SpawnData {
        arguments: [0; MAX_ARGUMENT_BYTES],
        argument_length: ARGUMENTS.len(),
        argument_count: 2,
        environment: [0; MAX_ENVIRONMENT_BYTES],
        environment_length: ENVIRONMENT.len(),
        environment_count: 1,
        capabilities: [StartupCapability::EMPTY; PROCESS_SPAWN_MAX_CAPABILITIES],
        capability_count: 0,
    };
    start.arguments[..ARGUMENTS.len()].copy_from_slice(ARGUMENTS);
    start.environment[..ENVIRONMENT.len()].copy_from_slice(ENVIRONMENT);

    manager.begin_phase();
    manager.spawn_internal(
        "system/bin/std-main.rune",
        SpawnOptions {
            parent: ProcessId::KERNEL,
            priority: PriorityClass::Interactive,
            boot_arguments: [0; 3],
            expected: Some(ExpectedExit {
                status: 0,
                exception: 0,
            }),
        },
        [EMPTY_CAPABILITY; MAX_CAPABILITIES],
        Some(&start),
    )?;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    manager.cleanup();
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
        "system/bin/vfsd.rune",
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
            rights: Rights::READ.union(Rights::EXECUTE).union(Rights::TRANSFER),
        };
    }
    client_caps[SERVER_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(SERVER_ENDPOINT),
        rights: Rights::SEND.union(Rights::TRANSFER),
    };
    client_caps[DEVICE_OR_REPLY_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(REPLY_ENDPOINT),
        rights: Rights::SEND.union(Rights::RECEIVE).union(Rights::TRANSFER),
    };
    let client = if client_image == "system/bin/std-smoke.rune" {
        const ARGUMENTS: &[u8] = b"std-smoke\0";
        let mut start = SpawnData {
            arguments: [0; MAX_ARGUMENT_BYTES],
            argument_length: ARGUMENTS.len(),
            argument_count: 1,
            environment: [0; MAX_ENVIRONMENT_BYTES],
            environment_length: 0,
            environment_count: 0,
            capabilities: [StartupCapability::EMPTY; PROCESS_SPAWN_MAX_CAPABILITIES],
            capability_count: if executable_namespace { 3 } else { 2 },
        };
        start.arguments[..ARGUMENTS.len()].copy_from_slice(ARGUMENTS);
        start.capabilities[0] = StartupCapability {
            role: StartupRole::VFS,
            flags: 0,
            handle: Handle(SERVER_SLOT as u32),
            rights: Rights::SEND.union(Rights::TRANSFER),
        };
        start.capabilities[1] = StartupCapability {
            role: StartupRole::VFS_REPLY,
            flags: 0,
            handle: Handle(DEVICE_OR_REPLY_SLOT as u32),
            rights: Rights::SEND.union(Rights::RECEIVE).union(Rights::TRANSFER),
        };
        if executable_namespace {
            start.capabilities[2] = StartupCapability {
                role: StartupRole::EXECUTABLE_NAMESPACE,
                flags: 0,
                handle: Handle(VFS_ROOT_SLOT as u32),
                rights: Rights::READ.union(Rights::EXECUTE).union(Rights::TRANSFER),
            };
        }
        manager.spawn_internal(
            client_image,
            SpawnOptions {
                parent: ProcessId::KERNEL,
                priority: PriorityClass::Interactive,
                boot_arguments: [0; 3],
                expected: Some(ExpectedExit {
                    status: 0,
                    exception: 0,
                }),
            },
            client_caps,
            Some(&start),
        )?
    } else {
        manager.spawn(
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
        )?
    };
    manager.endpoints[REPLY_ENDPOINT as usize].receiver = client;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    manager.cleanup();
    Ok(())
}

#[derive(Clone, Copy)]
struct GraphicsServices {
    display: ProcessId,
    compositor: ProcessId,
    render: Option<ProcessId>,
}

/// End-to-end графический data plane: exclusive scanout capability получает
/// только displayd, compositor владеет buffers/timelines и reply endpoint.
fn run_graphics_service_phase(manager: &mut ProcessManager) -> Result<bool, ProcessError> {
    if scanout::info().is_err() {
        return Ok(false);
    }
    manager.begin_phase();
    let first = spawn_graphics_services(manager)?;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    if !graphics_services_blocked(manager, first) {
        manager.cleanup();
        return Err(ProcessError::UnexpectedExit);
    }

    // Supervisor fault drill: завершаем оба blocked процесса, полностью
    // отзываем их handles/mappings и запускаем пару заново. Второй frame
    // должен пройти тот же atomic present/vblank path без stale capability.
    manager.finish_process(first.display, normal_exit(71));
    manager.finish_process(first.compositor, normal_exit(72));
    if let Some(render) = first.render {
        manager.finish_process(render, normal_exit(73));
    }
    manager.reap_process(first.display);
    manager.reap_process(first.compositor);
    if let Some(render) = first.render {
        manager.reap_process(render);
    }
    manager.recover_display_master_after_exit();
    manager.endpoints[0] = Endpoint::EMPTY;
    manager.endpoints[3] = Endpoint::EMPTY;
    manager.endpoints[4] = Endpoint::EMPTY;
    manager.endpoints[5] = Endpoint::EMPTY;
    let restarted = spawn_graphics_services(manager)?;
    if let Err(error) = manager.run() {
        manager.cleanup();
        return Err(error);
    }
    if !graphics_services_blocked(manager, restarted) {
        manager.cleanup();
        return Err(ProcessError::UnexpectedExit);
    }
    manager.cleanup();
    Ok(true)
}

fn spawn_graphics_services(manager: &mut ProcessManager) -> Result<GraphicsServices, ProcessError> {
    const DISPLAY_ENDPOINT: u8 = 0;
    const FEEDBACK_ENDPOINT: u8 = 3;
    const GPU_FRAME_ENDPOINT: u8 = 4;
    const GPU_CONTROL_ENDPOINT: u8 = 5;
    const DISPLAY_SLOT: usize = 2;
    const DEVICE_OR_FEEDBACK_SLOT: usize = 3;
    const GPU_FRAME_SLOT: usize = 4;
    const GPU_CONTROL_SLOT: usize = 5;
    const GPU_MODE_FLAG: u64 = 1 << 63;
    let use_gpu = scanout::render_info().is_ok();

    let mut display_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    display_caps[DISPLAY_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(DISPLAY_ENDPOINT),
        rights: Rights::RECEIVE,
    };
    display_caps[DEVICE_OR_FEEDBACK_SLOT] = CapabilityEntry {
        kind: CapabilityKind::DisplayScanout(0),
        rights: Rights::READ.union(Rights::WRITE).union(Rights::WAIT),
    };
    let display = manager.spawn_internal(
        "system/bin/displayd.rune",
        SpawnOptions {
            parent: ProcessId::KERNEL,
            priority: PriorityClass::Driver,
            boot_arguments: [
                DISPLAY_SLOT as u64,
                DEVICE_OR_FEEDBACK_SLOT as u64,
                syscall::ABI_VERSION,
            ],
            expected: None,
        },
        display_caps,
        None,
    )?;
    manager.endpoints[DISPLAY_ENDPOINT as usize].receiver = display;

    let mut compositor_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    compositor_caps[DISPLAY_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(DISPLAY_ENDPOINT),
        rights: Rights::SEND,
    };
    compositor_caps[DEVICE_OR_FEEDBACK_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(FEEDBACK_ENDPOINT),
        rights: Rights::SEND.union(Rights::RECEIVE).union(Rights::TRANSFER),
    };
    if use_gpu {
        compositor_caps[GPU_FRAME_SLOT] = CapabilityEntry {
            kind: CapabilityKind::Endpoint(GPU_FRAME_ENDPOINT),
            rights: Rights::RECEIVE,
        };
        compositor_caps[GPU_CONTROL_SLOT] = CapabilityEntry {
            kind: CapabilityKind::Endpoint(GPU_CONTROL_ENDPOINT),
            rights: Rights::SEND,
        };
    }
    let compositor = manager.spawn_internal(
        "system/bin/compositord.rune",
        SpawnOptions {
            parent: ProcessId::KERNEL,
            priority: PriorityClass::System,
            boot_arguments: [
                DISPLAY_SLOT as u64,
                DEVICE_OR_FEEDBACK_SLOT as u64,
                syscall::ABI_VERSION | if use_gpu { GPU_MODE_FLAG } else { 0 },
            ],
            expected: None,
        },
        compositor_caps,
        None,
    )?;
    manager.endpoints[FEEDBACK_ENDPOINT as usize].receiver = compositor;
    let render = if use_gpu {
        let mut render_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
        render_caps[DISPLAY_SLOT] = CapabilityEntry {
            kind: CapabilityKind::Endpoint(GPU_FRAME_ENDPOINT),
            rights: Rights::SEND,
        };
        render_caps[DEVICE_OR_FEEDBACK_SLOT] = CapabilityEntry {
            kind: CapabilityKind::GpuRender(0),
            rights: Rights::READ.union(Rights::WRITE),
        };
        render_caps[GPU_FRAME_SLOT] = CapabilityEntry {
            kind: CapabilityKind::Endpoint(GPU_CONTROL_ENDPOINT),
            rights: Rights::RECEIVE,
        };
        let render = manager.spawn_internal(
            "system/bin/renderd.rune",
            SpawnOptions {
                parent: ProcessId::KERNEL,
                priority: PriorityClass::Driver,
                boot_arguments: [
                    DISPLAY_SLOT as u64,
                    DEVICE_OR_FEEDBACK_SLOT as u64,
                    syscall::ABI_VERSION,
                ],
                expected: None,
            },
            render_caps,
            None,
        )?;
        manager.endpoints[GPU_FRAME_ENDPOINT as usize].receiver = compositor;
        manager.endpoints[GPU_CONTROL_ENDPOINT as usize].receiver = render;
        Some(render)
    } else {
        None
    };
    Ok(GraphicsServices {
        display,
        compositor,
        render,
    })
}

fn graphics_services_blocked(manager: &ProcessManager, services: GraphicsServices) -> bool {
    service_blocked_on(manager, services.display, 0)
        && service_blocked_on(manager, services.compositor, 3)
        && services
            .render
            .is_none_or(|render| service_blocked_on(manager, render, 5))
        && manager.display_present_sequence == manager.display_completed_sequence
        && manager.display_completed_sequence != 0
}

fn service_blocked_on(manager: &ProcessManager, pid: ProcessId, endpoint: u8) -> bool {
    manager
        .processes
        .iter()
        .flatten()
        .any(|process| process.pid == pid && !process.exited)
        && manager.threads.iter().flatten().any(|thread| {
            thread.pid == pid
                && !thread.exited
                && matches!(
                    thread.pending,
                    PendingOperation::Receive(receive) if receive.endpoint == endpoint
                )
        })
}

fn stop_service(manager: &mut ProcessManager, pid: ProcessId, status_value: i32) {
    let exited = manager
        .processes
        .iter()
        .flatten()
        .find(|process| process.pid == pid)
        .map(|process| process.exited);
    if let Some(false) = exited {
        manager.finish_process(pid, normal_exit(status_value as i64 as u64));
    }
    manager.reap_process(pid);
}

fn run_ipc_phase(
    manager: &mut ProcessManager,
    receiver_priority: PriorityClass,
    sender_priority: PriorityClass,
    expected_blocked_receives: u64,
) -> Result<(), ProcessError> {
    let endpoint_id = 0u8;
    manager.begin_phase();
    let mut receiver_caps = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
    receiver_caps[ENDPOINT_SLOT] = CapabilityEntry {
        kind: CapabilityKind::Endpoint(endpoint_id),
        rights: Rights::RECEIVE,
    };
    let receiver = manager.spawn(
        "system/bin/ipc-receiver.rune",
        receiver_priority,
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
        "system/bin/ipc-sender.rune",
        sender_priority,
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
    if manager.blocked_receives != expected_blocked_receives
        || manager.transferred_capabilities != 1
    {
        manager.cleanup();
        return Err(ProcessError::UnexpectedExit);
    }
    manager.cleanup();
    Ok(())
}

fn bytes_of<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn align_up_usize(value: usize, alignment: usize) -> Option<usize> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return None;
    }
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
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

#[cfg(target_arch = "aarch64")]
fn is_stack_translation_fault(exception: u16, code: u16) -> bool {
    // EC=0x24: Data Abort from lower EL. DFSC 0b0001xx — translation fault
    // уровней 0..3; permission/alignment faults стек не расширяют.
    exception == 0x24 && matches!(code & 0x3f, 0x04..=0x07)
}

#[cfg(target_arch = "x86_64")]
fn is_stack_translation_fault(exception: u16, code: u16) -> bool {
    // #PF, P=0. Protection faults остаются обычными изолированными ошибками.
    exception == 14 && code & 1 == 0
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

fn pipe_id(index: usize, generation: u8) -> u16 {
    (u16::from(generation) << 8) | index as u16
}

fn pipe_index(id: u16) -> usize {
    usize::from(id & 0xff)
}

fn pipe_generation(id: u16) -> u8 {
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

fn timeline_status(error: TimelineError) -> i64 {
    match error {
        TimelineError::LimitReached => status::LIMIT_REACHED,
        TimelineError::InvalidId => status::BAD_HANDLE,
        TimelineError::NonMonotonic => status::INVALID_ARGUMENT,
    }
}

fn display_status(error: DisplayBrokerError) -> i64 {
    match error {
        DisplayBrokerError::Unavailable | DisplayBrokerError::UnsupportedMode => {
            status::NOT_SUPPORTED
        }
        DisplayBrokerError::Busy => status::BUSY,
        DisplayBrokerError::InvalidSurface => status::INVALID_ARGUMENT,
        DisplayBrokerError::DeviceLost => status::IO_ERROR,
        DisplayBrokerError::OutOfMemory => status::OUT_OF_MEMORY,
    }
}

fn gpu_status(error: DisplayBrokerError) -> i64 {
    display_status(error)
}

fn next_refresh_deadline(now: u64, requested: u64, interval: u64) -> u64 {
    let base = requested.max(now);
    base.checked_div(interval)
        .and_then(|period| period.checked_add(1))
        .and_then(|period| period.checked_mul(interval))
        .unwrap_or(u64::MAX)
}

/// Ошибки pipe редки и почти всегда означают дефект lifecycle/handle table.
/// Логируем только error path, поэтому обычный stdio не засоряет serial.
fn log_pipe_error(operation: &str, status_value: i64) {
    serial::put_str("[pipe] ");
    serial::put_str(operation);
    serial::put_str(" status=");
    serial::put_u32(status_value as u32);
    serial::put_str("\n");
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

//! Процессы микроядра: RUNE/ELF bootstrap loader, local capability table и synchronous
//! bootstrap runner. Scheduler позднее заменит synchronous enter/return, но
//! trap/fault/lifecycle ABI останется тем же.

mod elf;
mod graphics_objects;
#[path = "manager_v2.rs"]
mod manager;
mod rune;

use core::{ptr, str};

use rustos_abi::{
    bootinfo::BootInitramfs,
    syscall::{self, status},
    ExitReason, Handle, ProcessId, Rights,
};

use crate::{
    arch::{self, TrapFrame, TrapKind},
    fs,
    memory::{self, AddressSpace},
    serial,
};

pub(super) const MAX_CAPABILITIES: usize = 32;
pub(super) const VFS_ROOT_SLOT: usize = 1;

/// Результат любого executable loader. Формат файла не просачивается дальше
/// границы process subsystem.
#[derive(Clone, Copy, Debug)]
pub(super) struct LoadedImage {
    pub entry: u64,
    pub stack_pointer: u64,
    pub thread_pointer: u64,
    pub tls_template: Option<TlsTemplate>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TlsTemplate {
    pub bytes: &'static [u8],
    pub memory_size: u64,
    pub alignment: u64,
}

fn load_executable(
    space: &mut AddressSpace,
    image: &'static [u8],
) -> Result<LoadedImage, ProcessError> {
    if image.starts_with(&rustos_rune_format::MAGIC) {
        rune::load(space, image).map_err(|_| ProcessError::InvalidImage)
    } else {
        // Временный migration path: bootstrapping старого образа остаётся
        // возможным, но новые файлы initramfs собираются только как RUNE.
        elf::load(space, image).map_err(|_| ProcessError::InvalidImage)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CapabilityKind {
    Empty,
    VfsRoot,
    Endpoint(u8),
    /// Динамический process-owned endpoint: high byte — generation, low byte — index.
    DynamicEndpoint(u16),
    Process(ProcessId),
    Thread(rustos_abi::ThreadId),
    SharedMemory(u16),
    /// Разделяемая графическая память с неизменяемым pixel descriptor.
    GraphicsBuffer(u16),
    /// Монотонный explicit-sync timeline.
    SyncTimeline(rustos_microkernel::TimelineId),
    /// Эксклюзивный display controller; выдаётся только ring-3 displayd.
    DisplayScanout(u8),
    /// Эксклюзивная render authority; MMIO/virtqueue остаются в kernel.
    GpuRender(u8),
    /// Изолированный VirGL context доверенного ring-3 renderd.
    GpuContext(u8),
    /// Однонаправленный byte stream; READ/WRITE различаются rights одного object.
    Pipe(u16),
    /// Raw block device выдаётся только storage service, не приложениям.
    BlockDevice(u8),
}

#[derive(Clone, Copy)]
pub(super) struct CapabilityEntry {
    pub kind: CapabilityKind,
    pub rights: Rights,
}

pub(super) const EMPTY_CAPABILITY: CapabilityEntry = CapabilityEntry {
    kind: CapabilityKind::Empty,
    rights: Rights::NONE,
};

struct ProcessContext {
    pid: ProcessId,
    address_space: AddressSpace,
    capabilities: [CapabilityEntry; MAX_CAPABILITIES],
    initramfs: BootInitramfs,
    exit_reason: ExitReason,
}

impl ProcessContext {
    fn new(pid: ProcessId, address_space: AddressSpace, initramfs: BootInitramfs) -> Self {
        let mut capabilities = [EMPTY_CAPABILITY; MAX_CAPABILITIES];
        capabilities[VFS_ROOT_SLOT] = CapabilityEntry {
            kind: CapabilityKind::VfsRoot,
            rights: Rights::READ,
        };
        Self {
            pid,
            address_space,
            capabilities,
            initramfs,
            exit_reason: ExitReason {
                status: 0,
                exception: 0,
                flags: 0,
                fault_address: 0,
            },
        }
    }

    fn resolve(&self, handle: Handle, kind: CapabilityKind, rights: Rights) -> Result<(), i64> {
        let index = handle.0 as usize;
        let Some(entry) = self.capabilities.get(index) else {
            return Err(status::BAD_HANDLE);
        };
        if entry.kind == CapabilityKind::Empty {
            return Err(status::BAD_HANDLE);
        }
        if entry.kind != kind || !entry.rights.contains(rights) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ProcessError {
    MissingImage,
    AddressSpace,
    InvalidImage,
    UnexpectedExit,
    FrameLeak,
}

impl ProcessError {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::MissingImage => "missing-image",
            Self::AddressSpace => "address-space",
            Self::InvalidImage => "invalid-image",
            Self::UnexpectedExit => "unexpected-exit",
            Self::FrameLeak => "frame-leak",
        }
    }

    pub const fn diagnostic_code(&self) -> u8 {
        match self {
            Self::MissingImage => 0x54,
            Self::AddressSpace => 0x55,
            Self::InvalidImage => 0x56,
            Self::UnexpectedExit => 0x57,
            Self::FrameLeak => 0x58,
        }
    }
}

/// Результат программы, запущенной из интерактивной GUI-сессии.
#[derive(Clone, Copy, Debug)]
pub struct InteractiveExit {
    pub output_length: usize,
    pub status: i32,
    pub exception: u16,
}

static mut CURRENT_PROCESS: *mut ProcessContext = ptr::null_mut();

/// Запускает два настоящих ring-3 RUNE из initramfs: нормальный VFS client и
/// намеренно падающий isolation test.
pub fn run_bootstrap_milestone(initramfs: BootInitramfs) -> Result<(), ProcessError> {
    let normal = run_one(ProcessId::new(1, 1), initramfs, "system/bin/init.rune")?;
    if normal.status != 0 || normal.exception != 0 {
        log_unexpected_exit("init.rune", normal);
        return Err(ProcessError::UnexpectedExit);
    }
    serial::put_str("[process] init.rune exited cleanly; VFS capability verified\n");

    let fault = run_one(
        ProcessId::new(1, 2),
        initramfs,
        "system/bin/fault-test.rune",
    )?;
    if fault.exception != arch::ILLEGAL_INSTRUCTION_EXCEPTION {
        log_unexpected_exit("fault-test.rune", fault);
        return Err(ProcessError::UnexpectedExit);
    }
    serial::put_str("[isolation] user #UD contained; kernel and GUI continue\n");
    serial::put_str("[memory] user address spaces reclaimed\n");
    Ok(())
}

/// Запускает preemptive и capability-IPC вертикальные срезы на timer backend.
pub fn run_preemptive_milestone(info: &rustos_abi::BootInfo) -> Result<(), ProcessError> {
    manager::run_milestone(info)
}

/// Оставляет ring-3 `vfsd` запущенным после диагностических milestones.
pub fn start_interactive_services() -> Result<(), ProcessError> {
    manager::start_interactive_services()
}

/// Выполняет bounded supervisor tick постоянных display services.
pub fn pump_interactive_services() -> Result<(), ProcessError> {
    manager::pump_interactive_services()
}

/// Проверяет bounded GPU composition нескольких sampled surfaces.
#[cfg(feature = "virgl-test")]
pub fn run_gpu_compositor_probe(width: u32, height: u32) -> Result<(), ProcessError> {
    manager::run_gpu_compositor_probe(width, height)
}

/// Запускает bounded аппаратную Aurora 3D-демонстрацию.
#[cfg(feature = "virgl-test")]
pub fn run_interactive_gpu_demo(frame_count: u32) -> Result<(), ProcessError> {
    manager::run_interactive_gpu_demo(frame_count)
}

/// Запрашивает один GPU-rendered frame для обычного desktop-окна.
pub fn render_interactive_gpu_demo_frame(
    width: u32,
    height: u32,
    scene_frame: u32,
    output: &mut [u32],
) -> Result<(), ProcessError> {
    manager::render_interactive_gpu_demo_frame(width, height, scene_frame, output)
}

/// Выполняет одну команду из GUI terminal и захватывает объединённый
/// stdout/stderr. Большой вывод дренируется порциями, поэтому процесс не
/// зависнет на заполненном pipe.
pub fn run_interactive_command(
    command: &str,
    output: &mut [u8],
) -> Result<InteractiveExit, ProcessError> {
    manager::run_interactive_command(command, output)
}

fn run_one(
    pid: ProcessId,
    initramfs: BootInitramfs,
    path: &str,
) -> Result<ExitReason, ProcessError> {
    let free_before = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    let image = fs::initramfs_file(initramfs, path).map_err(|_| ProcessError::MissingImage)?;
    let kernel_root = arch::current_address_space_root();
    let mut address_space =
        AddressSpace::new(kernel_root).map_err(|_| ProcessError::AddressSpace)?;
    let loaded = load_executable(&mut address_space, image)?;
    let mut process = ProcessContext::new(pid, address_space, initramfs);
    serial::put_str("[process] enter ring3 pid=0x");
    serial::put_hex(pid.0);
    serial::put_str(" image=/boot/");
    serial::put_str(path);
    serial::put_str("\n");

    unsafe { CURRENT_PROCESS = &mut process };
    arch::set_user_thread_pointer(loaded.thread_pointer);
    let _raw_result = unsafe {
        arch::enter_user(
            loaded.entry,
            loaded.stack_pointer,
            [VFS_ROOT_SLOT as u64, syscall::ABI_VERSION, 0],
            process.address_space.root(),
            false,
        )
    };
    arch::set_user_thread_pointer(0);
    unsafe { CURRENT_PROCESS = ptr::null_mut() };
    // enter_user backend уже вернул kernel address-space root. Drop освобождает все
    // data/stack/page-table frames address space.
    let exit_reason = process.exit_reason;
    let free_while_alive = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    drop(process);
    let free_after = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    if free_after != free_before {
        serial::put_str("[memory] FATAL address-space frame leak before=0x");
        serial::put_hex(free_before);
        serial::put_str(" after=0x");
        serial::put_hex(free_after);
        serial::put_str("\n");
        return Err(ProcessError::FrameLeak);
    }
    serial::put_str("[memory] reclaimed address-space frames=0x");
    serial::put_hex(free_before.saturating_sub(free_while_alive));
    serial::put_str("\n");
    Ok(exit_reason)
}

/// Вызывается assembly trap stub. 0 = `iretq` обратно, 1 = завершить user run.
#[no_mangle]
pub extern "C" fn rustos_handle_trap(frame: &mut TrapFrame) -> u64 {
    if let Some(disposition) = manager::handle_active_trap(frame) {
        return disposition;
    }
    let kind = frame.kind();
    if kind == TrapKind::Spurious {
        return 0;
    }
    if kind == TrapKind::Timer {
        arch::rearm_scheduler_timer(crate::arch::counter_frequency());
        return 0;
    }
    if let TrapKind::Device { interrupt } = kind {
        let _ = crate::display::scanout::handle_interrupt(interrupt);
        arch::end_of_interrupt();
        return 0;
    }
    if !frame.is_from_user() {
        serial::put_str("[trap] FATAL kernel exception ip=0x");
        serial::put_hex(frame.instruction_pointer());
        serial::put_str(" syndrome=0x");
        serial::put_hex(frame.exception_syndrome());
        serial::put_str(" address=0x");
        serial::put_hex(frame.fault_address());
        serial::put_str("\n");
        crate::boot::exit_kernel(0x70);
    }
    let process = unsafe { CURRENT_PROCESS.as_mut() };
    let Some(process) = process else {
        crate::boot::exit_kernel(0x7f);
    };

    if kind == TrapKind::Syscall {
        match dispatch_syscall(process, frame) {
            SyscallDisposition::Return(value) => {
                frame.set_syscall_result(value);
                0
            }
            SyscallDisposition::Exit(status) => {
                process.exit_reason = ExitReason {
                    status,
                    exception: 0,
                    flags: 0,
                    fault_address: 0,
                };
                arch::set_user_run_result(status as u32 as u64);
                1
            }
        }
    } else {
        let TrapKind::Exception {
            number,
            code,
            instruction_pointer,
            fault_address,
        } = kind
        else {
            crate::boot::exit_kernel(0x7f);
        };
        process.exit_reason = ExitReason {
            status: status::FAULT as i32,
            exception: number,
            flags: code,
            fault_address,
        };
        serial::put_str("[fault] contained pid=0x");
        serial::put_hex(process.pid.0);
        serial::put_str(" vector=");
        serial::put_u32(number as u32);
        serial::put_str(" rip=0x");
        serial::put_hex(instruction_pointer);
        serial::put_str("\n");
        arch::set_user_run_result((1u64 << 63) | u64::from(number));
        1
    }
}

fn log_unexpected_exit(name: &str, reason: ExitReason) {
    serial::put_str("[process] unexpected exit image=");
    serial::put_str(name);
    serial::put_str(" status=");
    serial::put_u32(reason.status as u32);
    serial::put_str(" exception=");
    serial::put_u32(reason.exception as u32);
    serial::put_str(" fault=0x");
    serial::put_hex(reason.fault_address);
    serial::put_str("\n");
}

enum SyscallDisposition {
    Return(i64),
    Exit(i32),
}

fn dispatch_syscall(process: &mut ProcessContext, frame: &TrapFrame) -> SyscallDisposition {
    let [arg0, arg1, arg2] = frame.syscall_arguments();
    match frame.syscall_number() {
        syscall::number::THREAD_YIELD => SyscallDisposition::Return(status::OK),
        syscall::number::PROCESS_EXIT => SyscallDisposition::Exit(arg0 as i64 as i32),
        syscall::number::VFS_STAT => {
            let result = vfs_stat(process, Handle(arg0 as u32), arg1, arg2);
            SyscallDisposition::Return(result)
        }
        _ => SyscallDisposition::Return(status::NOT_SUPPORTED),
    }
}

fn vfs_stat(process: &ProcessContext, handle: Handle, path: u64, length: u64) -> i64 {
    if let Err(status) = process.resolve(handle, CapabilityKind::VfsRoot, Rights::READ) {
        return status;
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
    let mut path_buffer = [0u8; 96];
    if process
        .address_space
        .copy_from_user(path, &mut path_buffer[..length])
        .is_err()
    {
        return status::INVALID_ARGUMENT;
    }
    let Ok(path) = str::from_utf8(&path_buffer[..length]) else {
        return status::INVALID_ARGUMENT;
    };
    match fs::initramfs_file(process.initramfs, path) {
        Ok(file) => {
            serial::put_str("[vfs-cap] pid=0x");
            serial::put_hex(process.pid.0);
            serial::put_str(" STAT path=");
            serial::put_str(path);
            serial::put_str(" size=");
            serial::put_u32(file.len() as u32);
            serial::put_str("\n");
            i64::try_from(file.len()).unwrap_or(i64::MAX)
        }
        Err(_) => status::NOT_FOUND,
    }
}

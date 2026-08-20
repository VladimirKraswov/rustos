//! Процессы микроядра: ELF loader, local capability table и synchronous
//! bootstrap runner. Scheduler позднее заменит synchronous enter/return, но
//! trap/fault/lifecycle ABI останется тем же.

mod elf;

use core::{ptr, str};

use rustos_abi::{
    bootinfo::BootInitramfs,
    syscall::{self, status},
    ExitReason, Handle, ProcessId, Rights,
};

use crate::{
    arch::{self, traps::TrapFrame},
    fs,
    memory::{self, AddressSpace},
    serial,
};

const MAX_CAPABILITIES: usize = 32;
const VFS_ROOT_SLOT: usize = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapabilityKind {
    Empty,
    VfsRoot,
}

#[derive(Clone, Copy)]
struct CapabilityEntry {
    kind: CapabilityKind,
    rights: Rights,
}

const EMPTY_CAPABILITY: CapabilityEntry = CapabilityEntry {
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
    MissingElf,
    AddressSpace,
    InvalidElf,
    UnexpectedExit,
    FrameLeak,
}

static mut CURRENT_PROCESS: *mut ProcessContext = ptr::null_mut();

/// Запускает два настоящих ring-3 ELF из initramfs: нормальный VFS client и
/// намеренно падающий isolation test.
pub fn run_bootstrap_milestone(initramfs: BootInitramfs) -> Result<(), ProcessError> {
    let normal = run_one(ProcessId::new(1, 1), initramfs, "system/bin/init.elf")?;
    if normal.status != 0 || normal.exception != 0 {
        log_unexpected_exit("init.elf", normal);
        return Err(ProcessError::UnexpectedExit);
    }
    serial::put_str("[process] init.elf exited cleanly; VFS capability verified\n");

    let fault = run_one(ProcessId::new(1, 2), initramfs, "system/bin/fault-test.elf")?;
    if fault.exception != 6 {
        log_unexpected_exit("fault-test.elf", fault);
        return Err(ProcessError::UnexpectedExit);
    }
    serial::put_str("[isolation] user #UD contained; kernel and GUI continue\n");
    serial::put_str("[memory] user address spaces reclaimed\n");
    Ok(())
}

fn run_one(
    pid: ProcessId,
    initramfs: BootInitramfs,
    path: &str,
) -> Result<ExitReason, ProcessError> {
    let free_before = memory::stats()
        .map_err(|_| ProcessError::AddressSpace)?
        .free_frames;
    let image = fs::initramfs_file(initramfs, path).map_err(|_| ProcessError::MissingElf)?;
    let kernel_root = arch::read_cr3();
    let mut address_space =
        AddressSpace::new(kernel_root).map_err(|_| ProcessError::AddressSpace)?;
    let loaded = elf::load(&mut address_space, image).map_err(|_| ProcessError::InvalidElf)?;
    let mut process = ProcessContext::new(pid, address_space, initramfs);
    serial::put_str("[process] enter ring3 pid=0x");
    serial::put_hex(pid.0);
    serial::put_str(" image=/boot/");
    serial::put_str(path);
    serial::put_str("\n");

    unsafe { CURRENT_PROCESS = &mut process };
    let _raw_result = unsafe {
        arch::traps::enter_user(
            loaded.entry,
            loaded.stack_pointer,
            VFS_ROOT_SLOT as u64,
            syscall::ABI_VERSION,
            process.address_space.root(),
        )
    };
    unsafe { CURRENT_PROCESS = ptr::null_mut() };
    // enter_user assembly уже вернул kernel CR3. Drop process освобождает все
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
    if !frame.is_from_user() {
        serial::put_str("[trap] FATAL kernel exception vector=");
        serial::put_u32(frame.vector as u32);
        serial::put_str(" rip=0x");
        serial::put_hex(frame.rip);
        serial::put_str("\n");
        crate::boot::exit_kernel(0x70 | frame.vector as u8);
    }
    let process = unsafe { CURRENT_PROCESS.as_mut() };
    let Some(process) = process else {
        crate::boot::exit_kernel(0x7f);
    };

    if frame.vector == syscall::INTERRUPT_VECTOR as u64 {
        match dispatch_syscall(process, frame) {
            SyscallDisposition::Return(value) => {
                frame.rax = value as u64;
                0
            }
            SyscallDisposition::Exit(status) => {
                process.exit_reason = ExitReason {
                    status,
                    exception: 0,
                    flags: 0,
                    fault_address: 0,
                };
                arch::traps::set_user_result(status as u32 as u64);
                1
            }
        }
    } else {
        let fault_address = if frame.vector == 14 {
            arch::read_cr2()
        } else {
            frame.rip
        };
        process.exit_reason = ExitReason {
            status: status::FAULT as i32,
            exception: frame.vector as u16,
            flags: frame.error_code as u16,
            fault_address,
        };
        serial::put_str("[fault] contained pid=0x");
        serial::put_hex(process.pid.0);
        serial::put_str(" vector=");
        serial::put_u32(frame.vector as u32);
        serial::put_str(" rip=0x");
        serial::put_hex(frame.rip);
        serial::put_str("\n");
        arch::traps::set_user_result((1u64 << 63) | frame.vector);
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
    match frame.rax {
        syscall::number::THREAD_YIELD => SyscallDisposition::Return(status::OK),
        syscall::number::PROCESS_EXIT => SyscallDisposition::Exit(frame.rdi as i64 as i32),
        syscall::number::VFS_STAT => {
            let result = vfs_stat(process, Handle(frame.rdi as u32), frame.rsi, frame.rdx);
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

//! Постоянный ring-3 supervisor приложений.
//!
//! Ядро предоставляет только process/IPC/capability mechanisms. Вся policy —
//! какой RUNE запускать, с каким priority, когда повторять после fault и какой
//! lifecycle result вернуть launcher'у — находится в этом изолированном
//! процессе и может развиваться без изменения kernel ABI.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    ipc::{flags as ipc_flags, Message, IPC_INLINE_BYTES},
    process::{
        ProcessSpawnRequest, ProcessSpawnResult, SpawnCapability, StartupCapability, StartupRole,
        PROCESS_ABI_VERSION,
    },
    supervisor::{
        launch_flags, LaunchReply, LaunchRequest, LAUNCH_OPCODE, LAUNCH_REPLY_OPCODE,
        SUPERVISOR_ABI_VERSION,
    },
    syscall, ExitReason, Handle, ProcessId, Rights,
};
use rustos_runtime::{
    handle_close, ipc_receive, ipc_send, process_exit, process_spawn, process_start_info,
    process_wait, startup_capability,
};

const RUNNER_PATH: &[u8] = b"/boot/system/bin/rune-runner.rune";
const RUNNER_NAME: &[u8] = b"rune-runner\0";
const ENVIRONMENT: &[u8] = b"PWD=/\0HOME=/home\0TMPDIR=/tmp\0";

#[unsafe(no_mangle)]
pub extern "C" fn _start(start_address: u64, abi_version: u64, _reserved: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(150);
    }
    let info = unsafe { process_start_info(start_address) }.unwrap_or_else(|| process_exit(151));
    let capabilities =
        SupervisorCapabilities::from_startup(info).unwrap_or_else(|| process_exit(152));

    loop {
        let mut incoming = Message::EMPTY;
        if ipc_receive(capabilities.control.handle, &mut incoming) != syscall::status::OK {
            process_exit(153);
        }
        let reply = if incoming.header.opcode != LAUNCH_OPCODE
            || incoming.header.payload_len as usize != IPC_INLINE_BYTES
            || incoming.header.handle_count != 0
        {
            failed_reply(syscall::status::INVALID_ARGUMENT as i32)
        } else {
            match LaunchRequest::decode_inline(&incoming.payload) {
                Ok(request) => supervise(&capabilities, &request),
                Err(_) => failed_reply(syscall::status::INVALID_ARGUMENT as i32),
            }
        };
        let mut outgoing = Message::EMPTY;
        outgoing.header.opcode = LAUNCH_REPLY_OPCODE;
        outgoing.header.flags = ipc_flags::REPLY;
        outgoing.header.request_id = incoming.header.request_id;
        outgoing.header.payload_len = core::mem::size_of::<LaunchReply>() as u32;
        outgoing.payload = reply.encode_inline();
        if ipc_send(capabilities.reply.handle, &outgoing) != syscall::status::OK {
            process_exit(154);
        }
    }
}

#[derive(Clone, Copy)]
struct SupervisorCapabilities {
    namespace: StartupCapability,
    vfs: StartupCapability,
    vfs_reply: StartupCapability,
    stdout: StartupCapability,
    stderr: StartupCapability,
    control: StartupCapability,
    reply: StartupCapability,
}

impl SupervisorCapabilities {
    fn from_startup(info: &rustos_abi::process::ProcessStartInfo) -> Option<Self> {
        let get = |role| unsafe { startup_capability(info, role) };
        let capabilities = Self {
            namespace: get(StartupRole::EXECUTABLE_NAMESPACE)?,
            vfs: get(StartupRole::VFS)?,
            vfs_reply: get(StartupRole::VFS_REPLY)?,
            stdout: get(StartupRole::STDOUT)?,
            stderr: get(StartupRole::STDERR)?,
            control: get(StartupRole::LAUNCH_CONTROL)?,
            reply: get(StartupRole::LAUNCH_REPLY)?,
        };
        capabilities
            .namespace
            .rights
            .contains(Rights::READ.union(Rights::EXECUTE).union(Rights::TRANSFER))
            .then_some(())?;
        capabilities
            .vfs
            .rights
            .contains(Rights::SEND.union(Rights::TRANSFER))
            .then_some(())?;
        capabilities
            .vfs_reply
            .rights
            .contains(Rights::SEND.union(Rights::RECEIVE).union(Rights::TRANSFER))
            .then_some(())?;
        capabilities
            .stdout
            .rights
            .contains(Rights::WRITE.union(Rights::TRANSFER))
            .then_some(())?;
        capabilities
            .stderr
            .rights
            .contains(Rights::WRITE.union(Rights::TRANSFER))
            .then_some(())?;
        capabilities
            .control
            .rights
            .contains(Rights::RECEIVE)
            .then_some(())?;
        capabilities
            .reply
            .rights
            .contains(Rights::SEND)
            .then_some(())?;
        Some(capabilities)
    }
}

fn supervise(capabilities: &SupervisorCapabilities, request: &LaunchRequest) -> LaunchReply {
    let mut attempts = 0u8;
    loop {
        attempts += 1;
        let result = run_once(capabilities, request);
        let failed = result.supervisor_status != syscall::status::OK as i32
            || result.reason.status != 0
            || result.reason.exception != 0;
        let restart = request.flags & launch_flags::RESTART_ON_FAILURE != 0
            && failed
            && attempts <= request.restart_limit;
        if !restart {
            return LaunchReply { attempts, ..result };
        }
    }
}

fn run_once(capabilities: &SupervisorCapabilities, request: &LaunchRequest) -> LaunchReply {
    let command_length = usize::from(request.command_length);
    let mut arguments = [0u8; RUNNER_NAME.len() + rustos_abi::supervisor::COMMAND_BYTES];
    arguments[..RUNNER_NAME.len()].copy_from_slice(RUNNER_NAME);
    arguments[RUNNER_NAME.len()..RUNNER_NAME.len() + command_length]
        .copy_from_slice(&request.command[..command_length]);
    let argument_length = RUNNER_NAME.len() + command_length;
    let transfers = [
        transfer(
            capabilities.namespace,
            1,
            StartupRole::EXECUTABLE_NAMESPACE,
            Rights::READ.union(Rights::EXECUTE).union(Rights::TRANSFER),
        ),
        transfer(
            capabilities.vfs,
            2,
            StartupRole::VFS,
            Rights::SEND.union(Rights::TRANSFER),
        ),
        transfer(
            capabilities.vfs_reply,
            3,
            StartupRole::VFS_REPLY,
            Rights::SEND.union(Rights::RECEIVE).union(Rights::TRANSFER),
        ),
        transfer(
            capabilities.stdout,
            4,
            StartupRole::STDOUT,
            Rights::WRITE.union(Rights::TRANSFER),
        ),
        transfer(
            capabilities.stderr,
            5,
            StartupRole::STDERR,
            Rights::WRITE.union(Rights::TRANSFER),
        ),
    ];
    let spawn = ProcessSpawnRequest {
        version: PROCESS_ABI_VERSION,
        flags: 0,
        path_address: RUNNER_PATH.as_ptr() as u64,
        path_length: RUNNER_PATH.len() as u32,
        priority: request.priority,
        reserved0: [0; 3],
        arguments_address: arguments.as_ptr() as u64,
        arguments_length: argument_length as u32,
        argument_count: u32::from(request.argument_count) + 1,
        environment_address: ENVIRONMENT.as_ptr() as u64,
        environment_length: ENVIRONMENT.len() as u32,
        environment_count: 3,
        capabilities_address: transfers.as_ptr() as u64,
        capability_count: transfers.len() as u32,
        namespace: capabilities.namespace.handle,
    };
    let mut child = ProcessSpawnResult {
        process: Handle::INVALID,
        reserved: 0,
        pid: ProcessId::KERNEL,
    };
    let status = process_spawn(&spawn, &mut child);
    if status != syscall::status::OK {
        return failed_reply(status as i32);
    }
    let mut reason = ExitReason {
        status: syscall::status::BUSY as i32,
        exception: 0,
        flags: 0,
        fault_address: 0,
    };
    let wait_status = process_wait(child.process, &mut reason);
    let _ = handle_close(child.process);
    LaunchReply {
        version: SUPERVISOR_ABI_VERSION,
        attempts: 1,
        reserved0: 0,
        supervisor_status: wait_status as i32,
        pid: child.pid,
        reason,
    }
}

const fn transfer(
    capability: StartupCapability,
    target_slot: u16,
    role: StartupRole,
    rights: Rights,
) -> SpawnCapability {
    SpawnCapability {
        source: capability.handle,
        target_slot,
        role,
        rights,
    }
}

const fn failed_reply(status: i32) -> LaunchReply {
    LaunchReply {
        version: SUPERVISOR_ABI_VERSION,
        attempts: 0,
        reserved0: 0,
        supervisor_status: status,
        pid: ProcessId::KERNEL,
        reason: ExitReason {
            status,
            exception: 0,
            flags: 0,
            fault_address: 0,
        },
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(155)
}

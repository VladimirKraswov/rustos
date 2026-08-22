//! Сквозной ring-3 тест ABI. Один обычный пользовательский процесс проверяет
//! VM, TLS, потоки, shared memory, dynamic endpoints, spawn/wait/kill и часы.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    ipc::Message,
    memory::MEMORY_ABI_VERSION,
    process::{
        ProcessSpawnRequest, ProcessSpawnResult, SpawnCapability, StartupRole, ThreadCreateRequest,
        ThreadCreateResult, PROCESS_ABI_VERSION,
    },
    ExitReason, PriorityClass,
};
use rustos_runtime::{
    endpoint_create, handle_close, handle_duplicate, ipc_receive, ipc_send, monotonic_time_ns,
    process_exit, process_kill, process_spawn, process_wait, read_thread_pointer_u64,
    shared_memory_create, shared_memory_map, syscall, thread_create, thread_exit, thread_join,
    thread_set_tls, vm_map, vm_protect, vm_unmap, Handle, Rights, SharedMemoryCreate,
    SharedMemoryMap, VmFlags, VmMapRequest,
};

const PAGE_SIZE: u64 = 4096;
const VFS_SLOT: Handle = Handle(1);

#[no_mangle]
pub extern "C" fn _start(vfs_handle: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION || vfs_handle != VFS_SLOT.0 as u64 {
        process_exit(210);
    }
    let first_time = monotonic_time_ns();
    if first_time <= 0 {
        process_exit(211);
    }
    test_dynamic_endpoint();

    let scratch_request = VmMapRequest {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        length: PAGE_SIZE * 4,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };
    let scratch = vm_map(&scratch_request);
    if scratch <= 0 {
        process_exit(212);
    }
    let scratch_word = scratch as *mut u64;
    unsafe { scratch_word.write_volatile(0x1122_3344_5566_7788) };
    if vm_protect(scratch as u64, PAGE_SIZE * 4, VmFlags::READ) != syscall::status::OK
        || unsafe { scratch_word.read_volatile() } != 0x1122_3344_5566_7788
        || vm_protect(
            scratch as u64,
            PAGE_SIZE * 4,
            VmFlags::READ.union(VmFlags::WRITE).union(VmFlags::EXECUTE),
        ) != syscall::status::INVALID_ARGUMENT
        || vm_protect(
            scratch as u64,
            PAGE_SIZE * 4,
            VmFlags::READ.union(VmFlags::WRITE),
        ) != syscall::status::OK
        || vm_unmap(scratch as u64, PAGE_SIZE * 4) != syscall::status::OK
    {
        process_exit(213);
    }

    let shared_create = SharedMemoryCreate {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        length: PAGE_SIZE * 2,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };
    let shared_handle_value = shared_memory_create(&shared_create);
    if shared_handle_value <= 0 {
        process_exit(214);
    }
    let shared_handle = Handle(shared_handle_value as u32);
    let shared_map = SharedMemoryMap {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        offset: 0,
        length: PAGE_SIZE,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };
    let shared_address = shared_memory_map(shared_handle, &shared_map);
    if shared_address <= 0 {
        process_exit(215);
    }
    unsafe { (shared_address as *mut u64).write_volatile(0x5255_5354_4f53_0001) };

    let capabilities = [
        SpawnCapability {
            source: VFS_SLOT,
            target_slot: 1,
            role: StartupRole::EXECUTABLE_NAMESPACE,
            rights: Rights::READ,
        },
        SpawnCapability {
            source: shared_handle,
            target_slot: 3,
            role: StartupRole::NONE,
            rights: Rights::READ.union(Rights::WRITE).union(Rights::MAP),
        },
    ];
    let arguments = b"abi-child\0shared-test\0";
    let environment = b"RUSTOS_TEST=1\0";
    let request = ProcessSpawnRequest {
        version: PROCESS_ABI_VERSION,
        flags: 0,
        path_address: b"/boot/system/bin/abi-child.rune".as_ptr() as u64,
        path_length: b"/boot/system/bin/abi-child.rune".len() as u32,
        priority: PriorityClass::Interactive as u8,
        reserved0: [0; 3],
        arguments_address: arguments.as_ptr() as u64,
        arguments_length: arguments.len() as u32,
        argument_count: 2,
        environment_address: environment.as_ptr() as u64,
        environment_length: environment.len() as u32,
        environment_count: 1,
        capabilities_address: capabilities.as_ptr() as u64,
        capability_count: capabilities.len() as u32,
        namespace: VFS_SLOT,
    };
    let mut child = ProcessSpawnResult {
        process: Handle::INVALID,
        reserved: 0,
        pid: rustos_abi::ProcessId::KERNEL,
    };
    let spawn_status = process_spawn(&request, &mut child);
    if spawn_status != syscall::status::OK {
        process_exit(230 + (-spawn_status as i32));
    }
    if !child.process.is_valid() {
        process_exit(216);
    }
    let mut child_reason = empty_reason();
    if process_wait(child.process, &mut child_reason) != syscall::status::OK
        || child_reason.status != 0
        || child_reason.exception != 0
        || unsafe { (shared_address as *const u64).read_volatile() } != 0x5255_5354_4f53_0002
    {
        process_exit(217);
    }

    test_thread(shared_address as u64);

    // Второй child позволяет проверить внешний kill и повторное использование
    // process slots с новым generation.
    let spin_arguments = b"abi-child\0spin\0";
    let mut killed = ProcessSpawnResult {
        process: Handle::INVALID,
        reserved: 0,
        pid: rustos_abi::ProcessId::KERNEL,
    };
    let mut kill_request = request;
    kill_request.arguments_address = spin_arguments.as_ptr() as u64;
    kill_request.arguments_length = spin_arguments.len() as u32;
    if process_spawn(&kill_request, &mut killed) != syscall::status::OK
        || process_kill(killed.process, 77) != syscall::status::OK
    {
        process_exit(218);
    }
    let mut killed_reason = empty_reason();
    if process_wait(killed.process, &mut killed_reason) != syscall::status::OK
        || killed_reason.status != 77
        || killed.pid == child.pid
    {
        process_exit(219);
    }

    if vm_unmap(shared_address as u64, PAGE_SIZE) != syscall::status::OK
        || handle_close(shared_handle) != syscall::status::OK
        || monotonic_time_ns() < first_time
    {
        process_exit(220);
    }
    process_exit(0)
}

fn test_dynamic_endpoint() {
    let endpoint_value = endpoint_create();
    if endpoint_value <= 0 {
        process_exit(225);
    }
    let endpoint = Handle(endpoint_value as u32);
    let sender_value = handle_duplicate(endpoint, Rights::SEND.union(Rights::TRANSFER));
    if sender_value <= 0
        || handle_duplicate(endpoint, Rights::RECEIVE) != syscall::status::ACCESS_DENIED
    {
        process_exit(226);
    }
    let sender = Handle(sender_value as u32);
    let mut outgoing = Message::EMPTY;
    outgoing.header.opcode = 0x7e00;
    outgoing.header.request_id = 42;
    outgoing.header.payload_len = 8;
    outgoing.payload[..8].copy_from_slice(&0x5255_5354_4550_0001u64.to_le_bytes());
    if ipc_send(sender, &outgoing) != syscall::status::OK {
        process_exit(227);
    }
    let mut incoming = Message::EMPTY;
    if ipc_receive(endpoint, &mut incoming) != syscall::status::OK
        || incoming.header.opcode != outgoing.header.opcode
        || incoming.header.request_id != outgoing.header.request_id
        || incoming.header.sender_pid == 0
        || incoming.payload[..8] != outgoing.payload[..8]
        || handle_close(sender) != syscall::status::OK
        || handle_close(endpoint) != syscall::status::OK
    {
        process_exit(228);
    }
}

fn test_thread(shared_address: u64) {
    let stack_request = VmMapRequest {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        length: PAGE_SIZE * 4,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };
    let tls_request = VmMapRequest {
        length: PAGE_SIZE,
        ..stack_request
    };
    let stack = vm_map(&stack_request);
    let tls = vm_map(&tls_request);
    if stack <= 0 || tls <= 0 {
        process_exit(221);
    }
    unsafe { (tls as *mut u64).write_volatile(0x544c_5300_0000_0001) };
    if thread_set_tls(tls as u64) != syscall::status::OK
        || unsafe { read_thread_pointer_u64() } != 0x544c_5300_0000_0001
    {
        process_exit(222);
    }
    let request = ThreadCreateRequest {
        version: PROCESS_ABI_VERSION,
        flags: 0,
        entry: thread_worker as *const () as u64,
        stack_pointer: stack as u64 + PAGE_SIZE * 4 - 8,
        argument: shared_address,
        thread_pointer: tls as u64,
        reclaim_address: 0,
        reclaim_length: 0,
        priority: PriorityClass::Interactive as u8,
        reserved: [0; 7],
    };
    let mut result = ThreadCreateResult {
        thread: Handle::INVALID,
        reserved: 0,
        tid: rustos_abi::ThreadId::INVALID,
    };
    if thread_create(&request, &mut result) != syscall::status::OK {
        process_exit(223);
    }
    let mut reason = empty_reason();
    if thread_join(result.thread, &mut reason) != syscall::status::OK
        || reason.status != 33
        || unsafe { (shared_address as *const u64).read_volatile() } != 0x5255_5354_4f53_0003
        || vm_unmap(stack as u64, PAGE_SIZE * 4) != syscall::status::OK
        || vm_unmap(tls as u64, PAGE_SIZE) != syscall::status::OK
    {
        process_exit(224);
    }
}

extern "C" fn thread_worker(shared_address: u64) -> ! {
    if unsafe { read_thread_pointer_u64() } != 0x544c_5300_0000_0001 {
        thread_exit(31);
    }
    unsafe { (shared_address as *mut u64).write_volatile(0x5255_5354_4f53_0003) };
    thread_exit(33)
}

const fn empty_reason() -> ExitReason {
    ExitReason {
        status: 0,
        exception: 0,
        flags: 0,
        fault_address: 0,
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(229)
}

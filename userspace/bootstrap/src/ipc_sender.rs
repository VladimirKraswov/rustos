//! IPC client: передаёт VFS capability с ослабленными до READ правами.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_runtime::{ipc_send, process_exit, syscall, Handle, Message, Rights};

#[no_mangle]
pub extern "C" fn _start(endpoint: u64, vfs_handle: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(140);
    }
    let mut message = Message::EMPTY;
    message.header.opcode = 1;
    message.header.request_id = 0x5255_5354;
    message.header.payload_len = 3;
    message.header.handle_count = 1;
    message.payload[..3].copy_from_slice(b"vfs");
    message.handles[0].handle = Handle(vfs_handle as u32);
    // Нельзя усилить READ capability правом WRITE во время передачи.
    message.handles[0].rights = Rights::READ.union(Rights::WRITE);
    if ipc_send(Handle(endpoint as u32), &message) != syscall::status::ACCESS_DENIED {
        process_exit(142);
    }
    message.handles[0].rights = Rights::READ;
    if ipc_send(Handle(endpoint as u32), &message) != syscall::status::OK {
        process_exit(141);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(149)
}

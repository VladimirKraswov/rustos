//! IPC server: блокируется на endpoint, получает производный VFS capability
//! и использует его. Driver/service code не копируется в процесс.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_runtime::{
    ipc_receive, process_exit, syscall, vfs_stat, Handle, Message, VfsCapability,
};

#[no_mangle]
pub extern "C" fn _start(endpoint: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(130);
    }
    let mut message = Message::EMPTY;
    if ipc_receive(Handle(endpoint as u32), &mut message) != syscall::status::OK {
        process_exit(131);
    }
    if message.header.sender_pid == 0
        || message.header.payload_len != 3
        || &message.payload[..3] != b"vfs"
        || message.header.handle_count != 1
    {
        process_exit(132);
    }
    let received = message.handles[0].handle;
    if vfs_stat(VfsCapability(received), "/boot/README.txt") <= 0 {
        process_exit(133);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(139)
}

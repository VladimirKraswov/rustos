//! Второй клиент запускается после полного restart `vfsd` и доказывает, что
//! файл находится на диске, а не в памяти первого server process.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_abi::vfs::{open_flags, seek_from};
use rustos_runtime::{process_exit, syscall, Handle};
use rustos_vfs::VfsClient;

#[no_mangle]
pub extern "C" fn _start(server: u64, reply: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(170);
    }
    let mut client = match VfsClient::connect(Handle(server as u32), Handle(reply as u32)) {
        Ok(client) => client,
        Err(_) => process_exit(171),
    };
    let file = match client.open("/tmp/vfsd-test/vfsd-stream-renamed.bin", open_flags::READ) {
        Ok(file) => file,
        Err(_) => process_exit(172),
    };
    if client.seek(file, 0, seek_from::END) != Ok(70_000)
        || client.seek(file, -1, seek_from::END) != Ok(69_999)
    {
        process_exit(173);
    }
    let mut byte = [0u8; 1];
    if client.read(file, &mut byte) != Ok(1)
        || byte[0] != 0x5a
        || client.close(file).is_err()
        || client
            .unlink("/tmp/vfsd-test/vfsd-stream-renamed.bin")
            .is_err()
        || client.unlink("/tmp/vfsd-test").is_err()
        || client.sync().is_err()
        || client.shutdown_service().is_err()
    {
        process_exit(174);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(179)
}

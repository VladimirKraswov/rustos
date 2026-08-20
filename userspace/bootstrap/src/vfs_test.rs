//! Ring-3 интеграционный тест `vfs.dll -> IPC -> vfsd -> persistent disk`.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_abi::vfs::{open_flags, seek_from};
use rustos_runtime::{process_exit, syscall, Handle};
use rustos_vfs::VfsClient;

static WRITE_DATA: [u8; 70_000] = [0x5a; 70_000];
static mut READ_DATA: [u8; 70_000] = [0; 70_000];

#[no_mangle]
pub extern "C" fn _start(server: u64, reply: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(160);
    }
    let mut client = match VfsClient::connect(Handle(server as u32), Handle(reply as u32)) {
        Ok(client) => client,
        Err(_) => process_exit(161),
    };
    let _ = client.unlink("/tmp/vfsd-test/vfsd-stream-renamed.bin");
    let _ = client.unlink("/tmp/vfsd-test/vfsd-stream.bin");
    let _ = client.unlink("/tmp/vfsd-test");
    if client.make_dir("/tmp/vfsd-test").is_err() {
        process_exit(168);
    }
    let file = match client.open(
        "/tmp/vfsd-test/vfsd-stream.bin",
        open_flags::READ | open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE,
    ) {
        Ok(file) => file,
        Err(_) => process_exit(162),
    };
    if client.write(file, &WRITE_DATA) != Ok(WRITE_DATA.len())
        || client.seek(file, 0, seek_from::START) != Ok(0)
    {
        process_exit(163);
    }
    let read = unsafe { &mut *core::ptr::addr_of_mut!(READ_DATA) };
    if client.read(file, read) != Ok(read.len()) || read != &WRITE_DATA {
        process_exit(164);
    }
    if client.close(file).is_err()
        || client
            .rename(
                "/tmp/vfsd-test/vfsd-stream.bin",
                "/tmp/vfsd-test/vfsd-stream-renamed.bin",
            )
            .is_err()
    {
        process_exit(165);
    }
    let directory = match client.open("/tmp/vfsd-test", open_flags::READ | open_flags::DIRECTORY) {
        Ok(directory) => directory,
        Err(_) => process_exit(166),
    };
    let mut found = false;
    while let Ok(Some(entry)) = client.read_dir(directory) {
        let name = &entry.name[..entry.name_length as usize];
        if name == b"vfsd-stream-renamed.bin" {
            found = true;
        }
    }
    if !found
        || client.close(directory).is_err()
        || client.sync().is_err()
        || client.shutdown_service().is_err()
    {
        process_exit(167);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(169)
}

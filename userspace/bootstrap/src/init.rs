//! Первый нормальный ring-3 процесс: проверяет attenuation capability и VFS.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_runtime::{process_exit, syscall, vfs_stat, Handle, VfsCapability};

#[no_mangle]
pub extern "C" fn _start(vfs_handle: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(10);
    }

    // Handle из другого process table не должен неожиданно сработать.
    if vfs_stat(VfsCapability(Handle(0xFFFF_FFFE)), "/boot/README.txt")
        != syscall::status::BAD_HANDLE
    {
        process_exit(11);
    }

    let size = vfs_stat(VfsCapability(Handle(vfs_handle as u32)), "/boot/README.txt");
    if size <= 0 {
        process_exit(12);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(101)
}

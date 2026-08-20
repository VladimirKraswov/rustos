//! Намеренно падающий процесс. `UD2` обязан завершить только этот process;
//! kernel после этого продолжает boot и запускает GUI.

#![no_std]
#![no_main]

use core::{arch::asm, panic::PanicInfo};
use rustos_runtime::process_exit;

#[no_mangle]
pub extern "C" fn _start(_vfs_handle: u64, _abi_version: u64) -> ! {
    unsafe { asm!("ud2", options(noreturn)) }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(101)
}

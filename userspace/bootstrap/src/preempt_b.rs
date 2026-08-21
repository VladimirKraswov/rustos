//! Второй CPU-bound процесс того же класса. Оба ELF обязаны получить timer
//! quanta до завершения без добровольного `thread_yield`.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_runtime::{extended_state_preemption_probe, process_exit};

#[no_mangle]
pub extern "C" fn _start(_unused: u64, abi_version: u64) -> ! {
    if abi_version != rustos_runtime::syscall::ABI_VERSION {
        process_exit(120);
    }
    if !extended_state_preemption_probe(0xbbbbbbbb_33333333, 0xbbbbbbbb_44444444, 200_000_000) {
        process_exit(122);
    }
    process_exit(22)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(121)
}

//! Второй CPU-bound процесс того же класса. Оба ELF обязаны получить timer
//! quanta до завершения без добровольного `thread_yield`.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_runtime::{monotonic_counter, process_exit};

#[no_mangle]
pub extern "C" fn _start(_unused: u64, abi_version: u64) -> ! {
    if abi_version != rustos_runtime::syscall::ABI_VERSION {
        process_exit(120);
    }
    burn_tsc_cycles(200_000_000);
    process_exit(22)
}

fn burn_tsc_cycles(cycles: u64) {
    let start = monotonic_counter();
    while monotonic_counter().wrapping_sub(start) < cycles {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(121)
}

//! CPU-bound процесс A: не вызывает yield и тем самым проверяет именно
//! аппаратное вытеснение архитектурным timer'ом.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_runtime::{extended_state_preemption_probe, process_exit};

#[no_mangle]
pub extern "C" fn _start(_unused: u64, abi_version: u64) -> ! {
    if abi_version != rustos_runtime::syscall::ABI_VERSION {
        process_exit(110);
    }
    if !extended_state_preemption_probe(0xaaaaaaaa_11111111, 0xaaaaaaaa_22222222, 200_000_000) {
        process_exit(112);
    }
    process_exit(21)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(111)
}

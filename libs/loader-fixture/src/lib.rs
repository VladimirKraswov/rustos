//! Минимальная настоящая DLL для исполняемого теста dynamic loader.

#![no_std]
#![feature(thread_local)]

use core::panic::PanicInfo;

#[thread_local]
static TLS_ANSWER: u64 = 41;

#[no_mangle]
pub extern "C" fn fixture_answer() -> u64 {
    TLS_ANSWER
}

/// Не использует GOT/TLS и поэтому подходит для проверки одной физически
/// общей RX-страницы в другом address space.
#[no_mangle]
pub extern "C" fn fixture_shared_answer() -> u64 {
    41
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

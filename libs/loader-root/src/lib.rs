//! Root DLL теста. Вызов проходит через импорт из `fixture-1.dll`.

#![no_std]

use core::panic::PanicInfo;

unsafe extern "C" {
    fn fixture_answer() -> u64;
}

#[no_mangle]
pub extern "C" fn linked_answer() -> u64 {
    unsafe { fixture_answer() + 1 }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

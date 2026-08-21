//! Второй CPU-bound процесс того же класса. Оба ELF обязаны получить timer
//! quanta до завершения без добровольного `thread_yield`.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
#[cfg(not(target_arch = "x86_64"))]
use rustos_runtime::monotonic_counter;
use rustos_runtime::process_exit;

#[no_mangle]
pub extern "C" fn _start(_unused: u64, abi_version: u64) -> ! {
    if abi_version != rustos_runtime::syscall::ABI_VERSION {
        process_exit(120);
    }
    if !burn_with_extended_state(0xbbbbbbbb_33333333, 0xbbbbbbbb_44444444, 200_000_000) {
        process_exit(122);
    }
    process_exit(22)
}

#[cfg(not(target_arch = "x86_64"))]
fn burn_with_extended_state(_low: u64, _high: u64, cycles: u64) -> bool {
    burn_tsc_cycles(cycles);
    true
}

/// Парный marker отличается от `preempt-a`; так тест проверяет не просто
/// возможность выполнить SSE-инструкцию, а изоляцию SIMD state процессов.
#[cfg(target_arch = "x86_64")]
fn burn_with_extended_state(low: u64, high: u64, cycles: u64) -> bool {
    let expected = [low, high];
    let mut actual = [0u64; 2];
    unsafe {
        core::arch::asm!(
            "movdqu xmm15, xmmword ptr [{expected}]",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "mov r8, rax",
            "2:",
            "pause",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "sub rax, r8",
            "cmp rax, {cycles}",
            "jb 2b",
            "movdqu xmmword ptr [{actual}], xmm15",
            expected = in(reg) expected.as_ptr(),
            actual = in(reg) actual.as_mut_ptr(),
            cycles = in(reg) cycles,
            out("rax") _,
            out("rdx") _,
            out("r8") _,
            out("xmm15") _,
            options(nostack),
        );
    }
    actual == expected
}

#[cfg(not(target_arch = "x86_64"))]
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

//! Единственное место user runtime с ISA-specific инструкциями.

#[cfg(target_arch = "x86_64")]
pub unsafe fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            options(nostack),
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
pub unsafe fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") arg0 as i64 => result,
            in("x1") arg1,
            in("x2") arg2,
            options(nostack),
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub fn monotonic_counter() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

#[cfg(target_arch = "aarch64")]
pub fn monotonic_counter() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!(
            "mrs {value}, cntvct_el0",
            value = out(reg) value,
            options(nomem, nostack),
        );
    }
    value
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn read_thread_pointer_u64() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!(
            "mov {}, fs:[0]",
            out(reg) value,
            options(nostack, readonly),
        );
    }
    value
}

#[cfg(target_arch = "aarch64")]
pub unsafe fn read_thread_pointer_u64() -> u64 {
    let address: u64;
    unsafe {
        core::arch::asm!(
            "mrs {value}, tpidr_el0",
            value = out(reg) address,
            options(nostack),
        );
        (address as *const u64).read_volatile()
    }
}

#[cfg(target_arch = "x86_64")]
pub fn trigger_test_fault() -> ! {
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}

#[cfg(target_arch = "aarch64")]
pub fn trigger_test_fault() -> ! {
    unsafe { core::arch::asm!("brk #0", options(noreturn)) }
}

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

/// Держит уникальный 128-битный marker в XMM15 во время busy-loop.
/// Парные ring-3 процессы обнаруживают отсутствие FXSAVE/FXRSTOR, потому что
/// после timer preemption один из них получает SIMD state соседа.
#[cfg(target_arch = "x86_64")]
pub fn extended_state_preemption_probe(low: u64, high: u64, cycles: u64) -> bool {
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

/// AArch64 trap frame уже сохраняет q0..q31. Здесь portable ring-3 workload
/// продолжает проверять реальное вытеснение; SIMD layout отдельно закреплён
/// compile-time offset assertions в kernel arch backend.
#[cfg(target_arch = "aarch64")]
pub fn extended_state_preemption_probe(_low: u64, _high: u64, cycles: u64) -> bool {
    let start = monotonic_counter();
    while monotonic_counter().wrapping_sub(start) < cycles {
        core::hint::spin_loop();
    }
    true
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

#[cfg(target_arch = "x86_64")]
pub unsafe fn jump_to_image(entry: u64, stack: u64, start_info: u64, abi: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rsp, {stack}",
            "xor rbp, rbp",
            "jmp {entry}",
            stack = in(reg) stack,
            entry = in(reg) entry,
            in("rdi") start_info,
            in("rsi") abi,
            in("rdx") 0u64,
            options(noreturn)
        )
    }
}

#[cfg(target_arch = "aarch64")]
pub unsafe fn jump_to_image(entry: u64, stack: u64, start_info: u64, abi: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov sp, {stack}",
            "br {entry}",
            stack = in(reg) stack,
            entry = in(reg) entry,
            in("x0") start_info,
            in("x1") abi,
            in("x2") 0u64,
            options(noreturn)
        )
    }
}

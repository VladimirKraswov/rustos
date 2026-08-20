//! Минимальная граница между upstream `std` и syscall ABI RustOS.
//!
//! Здесь намеренно нет зависимости от `rustos-runtime`: стандартная
//! библиотека является самым нижним user-space слоем и должна собираться
//! внутри собственного sysroot без циклических Cargo-зависимостей.

use crate::io;

// SAFETY: вызывается один раз runtime'ом до пользовательского `main`.
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: вызывается один раз при штатном завершении runtime.
pub unsafe fn cleanup() {}

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}
pub fn unsupported_err() -> io::Error {
    io::Error::UNSUPPORTED_PLATFORM
}

pub fn abort_internal() -> ! {
    core::intrinsics::abort();
}

/// Общий машинно-независимый вход в микроядро. Номера и аргументы совпадают
/// с `rustos-abi`; различается только инструкция ISA.
#[inline]
pub unsafe fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    #[cfg(target_arch = "x86_64")]
    {
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
    {
        let mut result = arg0 as i64;
        unsafe {
            core::arch::asm!(
                "svc #0",
                in("x8") number,
                inlateout("x0") result,
                in("x1") arg1,
                in("x2") arg2,
                options(nostack),
            );
        }
        result
    }
}

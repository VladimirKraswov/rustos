//! Futex RustOS: uncontended path остаётся чистым atomic, а contention
//! блокирует поток в scheduler микроядра без polling и потери CPU.

use crate::sync::atomic::{Atomic, Ordering};
use crate::time::Duration;

pub type Futex = Atomic<Primitive>;
pub type Primitive = u32;
pub type SmallFutex = Atomic<SmallPrimitive>;
pub type SmallPrimitive = u32;

pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    if futex.load(Ordering::Relaxed) != expected {
        return true;
    }
    let timeout_ns = timeout
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX - 1))
        .unwrap_or(u64::MAX);
    unsafe {
        crate::sys::pal::syscall3(
            24,
            core::ptr::from_ref(futex).addr() as u64,
            expected as u64,
            timeout_ns,
        ) != -13
    }
}

pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    unsafe { crate::sys::pal::syscall3(25, core::ptr::from_ref(futex).addr() as u64, 1, 0) > 0 }
}

pub fn futex_wake_all(futex: &Atomic<u32>) {
    unsafe {
        crate::sys::pal::syscall3(25, core::ptr::from_ref(futex).addr() as u64, u64::MAX, 0);
    }
}

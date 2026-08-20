//! Начальная futex-семантика через cooperative yield.
//!
//! Atomics обеспечивают корректность, а yield не даёт waiter'у monopolize CPU.
//! Kernel wait/wake syscall позднее заменит polling без изменения Mutex API.

use crate::sync::atomic::{Atomic, Ordering};
use crate::sys::pal::syscall3;
use crate::time::Duration;

pub type Futex = Atomic<Primitive>;
pub type Primitive = u32;
pub type SmallFutex = Atomic<SmallPrimitive>;
pub type SmallPrimitive = u32;

pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    let deadline = timeout.and_then(|duration| {
        monotonic_ns().checked_add(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
    });
    while futex.load(Ordering::Relaxed) == expected {
        if deadline.is_some_and(|value| monotonic_ns() >= value) {
            return false;
        }
        unsafe { syscall3(0, 0, 0, 0) };
    }
    true
}

pub fn futex_wake(_futex: &Atomic<u32>) -> bool {
    // Polling waiter уже увидит atomic transition.
    false
}

pub fn futex_wake_all(_futex: &Atomic<u32>) {}

fn monotonic_ns() -> u64 {
    unsafe { syscall3(18, 0, 0, 0).max(0) as u64 }
}

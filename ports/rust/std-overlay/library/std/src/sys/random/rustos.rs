//! Временный non-cryptographic источник для HashMap seeds.
//!
//! Для ключей, токенов и криптографии приложения обязаны дождаться отдельного
//! entropy-service. Здесь задача только не оставлять HashMap с постоянным seed.

use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sys::pal::syscall3;

static STATE: AtomicU64 = AtomicU64::new(0x7275_7374_6f73_0001);

pub fn fill_bytes(bytes: &mut [u8]) {
    let mut state = STATE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    state ^= unsafe { syscall3(18, 0, 0, 0) } as u64;
    for byte in bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    STATE.store(state, Ordering::Relaxed);
}

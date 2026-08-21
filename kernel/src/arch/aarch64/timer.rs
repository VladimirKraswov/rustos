//! Generic Timer (AArch64 architected) — one-shot physical non-secure timer.
//!
//! Использует PPI 30 (physical non-secure timer) через GICv3.
//! CNTP_TVAL_EL0 — one-shot: при истечении генерирует interrupt и сбрасывается.

/// Частота architected counter (CNTFRQ_EL0).
pub fn counter_frequency() -> u64 {
    let freq: u64;
    // SAFETY: CNTFRQ_EL0 — architected read-only register.
    unsafe {
        core::arch::asm!(
            "mrs {freq}, cntfrq_el0",
            freq = out(reg) freq,
            options(nomem, nostack),
        );
    }
    freq
}

/// Инициализация timer: CNTP_CTL_EL0.ENABLE=0 (disabled).
pub fn initialize() {
    // SAFETY: CNTP system registers — доступны с EL1.
    unsafe {
        core::arch::asm!(
            "msr cntp_ctl_el0, xzr",
            "msr cntp_tval_el0, xzr",
            "isb",
            options(nostack),
        );
    }
}

/// Запуск scheduler timer: one-shot на 1 мс.
pub fn start(counter_hz: u64) {
    let period = counter_hz.div_ceil(1000).max(1);
    rearm(period);
}

/// Ре-арм timer: следующий interrupt через `period` тиков.
pub fn rearm(period: u64) {
    // SAFETY: CNTP_TVAL_EL0 — one-shot timer value.
    unsafe {
        core::arch::asm!(
            "msr cntp_tval_el0, {val}",
            "mov x9, #1",
            "msr cntp_ctl_el0, x9",
            "isb",
            val = in(reg) period,
            out("x9") _,
            options(nostack),
        );
    }
}

/// Остановка timer (CNTP_CTL_EL0.ENABLE=0).
pub fn stop() {
    // SAFETY: CNTP_CTL_EL0 — system register.
    unsafe {
        core::arch::asm!("msr cntp_ctl_el0, xzr", "isb", options(nostack),);
    }
}

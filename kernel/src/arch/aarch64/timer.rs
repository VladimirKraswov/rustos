//! Generic Timer (AArch64 architected) — one-shot virtual timer.
//!
//! Использует PPI 27 (`CNTV_*`) через GICv3. Virtual timer доступен EL1 как
//! на bare-metal без EL2, так и внутри гипервизора. В отличие от `CNTP_*`, он
//! не требует, чтобы host разрешил гостю прямое программирование physical
//! timer; это важно для HVF/UTM на Apple Silicon.

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

/// Инициализация timer: CNTV_CTL_EL0.ENABLE=0 (disabled).
pub fn initialize() {
    // SAFETY: CNTV system registers являются архитектурным EL1 interface.
    unsafe {
        core::arch::asm!(
            "msr cntv_ctl_el0, xzr",
            "msr cntv_tval_el0, xzr",
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
    // SAFETY: CNTV_TVAL_EL0 задаёт signed relative deadline virtual timer.
    unsafe {
        core::arch::asm!(
            "msr cntv_tval_el0, {val}",
            "mov x9, #1",
            "msr cntv_ctl_el0, x9",
            "isb",
            val = in(reg) period,
            out("x9") _,
            options(nostack),
        );
    }
}

/// Остановка timer (CNTV_CTL_EL0.ENABLE=0).
pub fn stop() {
    // SAFETY: CNTV_CTL_EL0 — архитектурный EL1 system register.
    unsafe {
        core::arch::asm!("msr cntv_ctl_el0, xzr", "isb", options(nostack),);
    }
}

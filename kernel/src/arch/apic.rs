//! Local APIC через x2APIC MSR.
//!
//! MSR-интерфейс не требует отображать legacy MMIO page `0xfee00000` и
//! одинаково работает в QEMU TCG/KVM. Первый timer использует TSC-deadline:
//! нет неточной калибровки decrement counter и каждый tick явно rearm'ится.

use core::{
    arch::{asm, x86_64::__cpuid},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

pub const TIMER_VECTOR: u8 = 0x40;
pub const SPURIOUS_VECTOR: u8 = 0xff;
const IA32_APIC_BASE: u32 = 0x1b;
const IA32_TSC_DEADLINE: u32 = 0x6e0;
const X2APIC_ID: u32 = 0x802;
const X2APIC_TPR: u32 = 0x808;
const X2APIC_EOI: u32 = 0x80b;
const X2APIC_SVR: u32 = 0x80f;
const X2APIC_ICR: u32 = 0x830;
const X2APIC_LVT_TIMER: u32 = 0x832;
const X2APIC_INITIAL_COUNT: u32 = 0x838;
const X2APIC_CURRENT_COUNT: u32 = 0x839;
const X2APIC_DIVIDE_CONFIG: u32 = 0x83e;
const TIMER_MASKED: u64 = 1 << 16;
const TIMER_PERIODIC: u64 = 1 << 17;
const TIMER_TSC_DEADLINE: u64 = 2 << 17;
const APIC_GLOBAL_ENABLE: u64 = 1 << 11;
const X2APIC_ENABLE: u64 = 1 << 10;
const APIC_SOFTWARE_ENABLE: u64 = 1 << 8;
const TICKS_PER_SECOND: u64 = 100;
const DIVIDE_BY_16: u64 = 0b0011;

static USES_TSC_DEADLINE: AtomicBool = AtomicBool::new(false);
static PERIODIC_INITIAL_COUNT: AtomicU32 = AtomicU32::new(100_000);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApicError {
    MissingApic,
    MissingX2Apic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApicInfo {
    pub id: u32,
    pub tsc_hz: u64,
    pub uses_tsc_deadline: bool,
}

/// Включает x2APIC текущего CPU, принимает все классы приоритетов и ставит
/// spurious vector. Вызывать нужно отдельно на BSP и каждом AP.
pub fn initialize_local() -> Result<ApicInfo, ApicError> {
    let features = __cpuid(1);
    if features.edx & (1 << 9) == 0 {
        return Err(ApicError::MissingApic);
    }
    if features.ecx & (1 << 21) == 0 {
        return Err(ApicError::MissingX2Apic);
    }
    let uses_tsc_deadline = features.ecx & (1 << 24) != 0;
    let base = read_msr(IA32_APIC_BASE);
    write_msr(IA32_APIC_BASE, base | APIC_GLOBAL_ENABLE | X2APIC_ENABLE);
    write_msr(X2APIC_TPR, 0);
    write_msr(
        X2APIC_SVR,
        APIC_SOFTWARE_ENABLE | u64::from(SPURIOUS_VECTOR),
    );
    let tsc_hz = tsc_frequency_hz();
    USES_TSC_DEADLINE.store(uses_tsc_deadline, Ordering::Release);
    if uses_tsc_deadline {
        write_msr(
            X2APIC_LVT_TIMER,
            TIMER_MASKED | TIMER_TSC_DEADLINE | u64::from(TIMER_VECTOR),
        );
    } else {
        calibrate_periodic_timer(tsc_hz);
    }
    Ok(ApicInfo {
        id: local_id(),
        tsc_hz,
        uses_tsc_deadline,
    })
}

/// Разрешает timer и назначает первый deadline примерно через 10 ms.
pub fn start_timer(tsc_hz: u64) {
    if USES_TSC_DEADLINE.load(Ordering::Acquire) {
        write_msr(
            X2APIC_LVT_TIMER,
            TIMER_TSC_DEADLINE | u64::from(TIMER_VECTOR),
        );
        rearm_timer(tsc_hz);
    } else {
        write_msr(X2APIC_DIVIDE_CONFIG, DIVIDE_BY_16);
        write_msr(X2APIC_LVT_TIMER, TIMER_PERIODIC | u64::from(TIMER_VECTOR));
        write_msr(
            X2APIC_INITIAL_COUNT,
            u64::from(PERIODIC_INITIAL_COUNT.load(Ordering::Acquire)),
        );
    }
}

/// TSC-deadline однократный, поэтому handler назначает следующий tick.
pub fn rearm_timer(tsc_hz: u64) {
    if !USES_TSC_DEADLINE.load(Ordering::Acquire) {
        return;
    }
    let delta = (tsc_hz / TICKS_PER_SECOND).max(1);
    write_msr(IA32_TSC_DEADLINE, read_tsc().saturating_add(delta));
}

pub fn stop_timer() {
    if USES_TSC_DEADLINE.load(Ordering::Acquire) {
        write_msr(IA32_TSC_DEADLINE, 0);
        write_msr(
            X2APIC_LVT_TIMER,
            TIMER_MASKED | TIMER_TSC_DEADLINE | u64::from(TIMER_VECTOR),
        );
    } else {
        write_msr(X2APIC_INITIAL_COUNT, 0);
        write_msr(
            X2APIC_LVT_TIMER,
            TIMER_MASKED | TIMER_PERIODIC | u64::from(TIMER_VECTOR),
        );
    }
}

/// Подтверждает обычное APIC-прерывание. Spurious vector EOI не требует.
pub fn end_of_interrupt() {
    write_msr(X2APIC_EOI, 0);
}

pub fn local_id() -> u32 {
    read_msr(X2APIC_ID) as u32
}

/// Отправляет raw ICR другому local APIC. Destination занимает старшие биты
/// x2APIC ICR; нижние биты задают INIT/SIPI/vector.
pub fn send_ipi(destination: u32, command: u32) {
    write_msr(
        X2APIC_ICR,
        (u64::from(destination) << 32) | u64::from(command),
    );
}

pub fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    unsafe { asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack)) };
    (u64::from(high) << 32) | u64::from(low)
}

/// Busy-wait нужен только для регламентированных INIT/SIPI интервалов ранней
/// загрузки, когда scheduler и clock service ещё не работают.
pub fn delay_microseconds(tsc_hz: u64, microseconds: u64) {
    let ticks = tsc_hz
        .saturating_div(1_000_000)
        .saturating_mul(microseconds);
    let start = read_tsc();
    while read_tsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

fn tsc_frequency_hz() -> u64 {
    let maximum = __cpuid(0).eax;
    if maximum >= 0x15 {
        let leaf = __cpuid(0x15);
        if leaf.eax != 0 && leaf.ebx != 0 && leaf.ecx != 0 {
            return u64::from(leaf.ecx)
                .saturating_mul(u64::from(leaf.ebx))
                .saturating_div(u64::from(leaf.eax));
        }
    }
    if maximum >= 0x16 {
        let leaf = __cpuid(0x16);
        if leaf.eax != 0 {
            return u64::from(leaf.eax) * 1_000_000;
        }
    }
    // QEMU и старые CPU иногда не сообщают crystal/base frequency. 1 GHz
    // даёт безопасный короткий quantum; monotonic correctness от числа не
    // зависит, меняется только длительность теста.
    1_000_000_000
}

fn calibrate_periodic_timer(tsc_hz: u64) {
    write_msr(X2APIC_DIVIDE_CONFIG, DIVIDE_BY_16);
    write_msr(X2APIC_LVT_TIMER, TIMER_MASKED | u64::from(TIMER_VECTOR));
    write_msr(X2APIC_INITIAL_COUNT, u64::from(u32::MAX));
    delay_microseconds(tsc_hz, 10_000);
    let current = read_msr(X2APIC_CURRENT_COUNT) as u32;
    write_msr(X2APIC_INITIAL_COUNT, 0);
    let elapsed_in_10ms = u32::MAX.saturating_sub(current);
    // Калибровочное окно тоже 10 ms, то есть ровно один quantum 100 Hz.
    PERIODIC_INITIAL_COUNT.store(elapsed_in_10ms.max(100), Ordering::Release);
}

fn read_msr(register: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") register,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

fn write_msr(register: u32, value: u64) {
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") register,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack),
        );
    }
}

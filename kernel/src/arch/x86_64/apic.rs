//! Local APIC с двумя взаимозаменяемыми backend'ами.
//!
//! На современном CPU используется x2APIC/MSR. Если гипервизор не реализует
//! x2APIC (например, старый QEMU TCG), ядро остаётся на архитектурном
//! xAPIC/MMIO. Загрузчик заранее отображает страницу `0xfee00000`, поэтому
//! fallback не требует менять page tables после старта ядра. Timer предпочитает
//! TSC-deadline, а при его отсутствии калибрует periodic decrement counter.

use core::{
    arch::{asm, x86_64::__cpuid},
    ptr,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering},
};

pub const TIMER_VECTOR: u8 = 0x40;
pub const SPURIOUS_VECTOR: u8 = 0xff;
const IA32_APIC_BASE: u32 = 0x1b;
const IA32_TSC_DEADLINE: u32 = 0x6e0;
const LEGACY_APIC_PHYS: usize = 0xfee0_0000;
const X2APIC_ID: u32 = 0x802;
const X2APIC_TPR: u32 = 0x808;
const X2APIC_EOI: u32 = 0x80b;
const X2APIC_SVR: u32 = 0x80f;
const X2APIC_ICR: u32 = 0x830;
const X2APIC_LVT_TIMER: u32 = 0x832;
const X2APIC_INITIAL_COUNT: u32 = 0x838;
const X2APIC_CURRENT_COUNT: u32 = 0x839;
const X2APIC_DIVIDE_CONFIG: u32 = 0x83e;
const XAPIC_ID: usize = 0x020;
const XAPIC_TPR: usize = 0x080;
const XAPIC_EOI: usize = 0x0b0;
const XAPIC_SVR: usize = 0x0f0;
const XAPIC_ICR_LOW: usize = 0x300;
const XAPIC_ICR_HIGH: usize = 0x310;
const XAPIC_LVT_TIMER: usize = 0x320;
const XAPIC_INITIAL_COUNT: usize = 0x380;
const XAPIC_CURRENT_COUNT: usize = 0x390;
const XAPIC_DIVIDE_CONFIG: usize = 0x3e0;
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
const MODE_XAPIC: u8 = 1;
const MODE_X2APIC: u8 = 2;
static APIC_MODE: AtomicU8 = AtomicU8::new(MODE_XAPIC);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApicError {
    MissingApic,
    UnsupportedMmioBase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApicInfo {
    pub id: u32,
    pub tsc_hz: u64,
    pub uses_x2apic: bool,
    pub uses_tsc_deadline: bool,
}

/// Включает лучший доступный APIC backend текущего CPU, принимает все классы
/// приоритетов и ставит spurious vector. Вызывать отдельно на BSP и каждом AP.
pub fn initialize_local() -> Result<ApicInfo, ApicError> {
    let features = __cpuid(1);
    if features.edx & (1 << 9) == 0 {
        return Err(ApicError::MissingApic);
    }
    let uses_x2apic = features.ecx & (1 << 21) != 0;
    let uses_tsc_deadline = features.ecx & (1 << 24) != 0;
    let base = read_msr(IA32_APIC_BASE);
    if !uses_x2apic && base & 0xffff_f000 != LEGACY_APIC_PHYS as u64 {
        return Err(ApicError::UnsupportedMmioBase);
    }
    if uses_x2apic {
        write_msr(IA32_APIC_BASE, base | APIC_GLOBAL_ENABLE | X2APIC_ENABLE);
        APIC_MODE.store(MODE_X2APIC, Ordering::Release);
    } else {
        write_msr(IA32_APIC_BASE, (base | APIC_GLOBAL_ENABLE) & !X2APIC_ENABLE);
        APIC_MODE.store(MODE_XAPIC, Ordering::Release);
    }
    write_register(X2APIC_TPR, XAPIC_TPR, 0);
    write_register(
        X2APIC_SVR,
        XAPIC_SVR,
        APIC_SOFTWARE_ENABLE | u64::from(SPURIOUS_VECTOR),
    );
    let tsc_hz = tsc_frequency_hz();
    USES_TSC_DEADLINE.store(uses_tsc_deadline, Ordering::Release);
    if uses_tsc_deadline {
        write_register(
            X2APIC_LVT_TIMER,
            XAPIC_LVT_TIMER,
            TIMER_MASKED | TIMER_TSC_DEADLINE | u64::from(TIMER_VECTOR),
        );
    } else {
        calibrate_periodic_timer(tsc_hz);
    }
    Ok(ApicInfo {
        id: local_id(),
        tsc_hz,
        uses_x2apic,
        uses_tsc_deadline,
    })
}

/// Разрешает timer и назначает первый deadline примерно через 10 ms.
pub fn start_timer(tsc_hz: u64) {
    if USES_TSC_DEADLINE.load(Ordering::Acquire) {
        write_register(
            X2APIC_LVT_TIMER,
            XAPIC_LVT_TIMER,
            TIMER_TSC_DEADLINE | u64::from(TIMER_VECTOR),
        );
        rearm_timer(tsc_hz);
    } else {
        write_register(X2APIC_DIVIDE_CONFIG, XAPIC_DIVIDE_CONFIG, DIVIDE_BY_16);
        write_register(
            X2APIC_LVT_TIMER,
            XAPIC_LVT_TIMER,
            TIMER_PERIODIC | u64::from(TIMER_VECTOR),
        );
        write_register(
            X2APIC_INITIAL_COUNT,
            XAPIC_INITIAL_COUNT,
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
        write_register(
            X2APIC_LVT_TIMER,
            XAPIC_LVT_TIMER,
            TIMER_MASKED | TIMER_TSC_DEADLINE | u64::from(TIMER_VECTOR),
        );
    } else {
        write_register(X2APIC_INITIAL_COUNT, XAPIC_INITIAL_COUNT, 0);
        write_register(
            X2APIC_LVT_TIMER,
            XAPIC_LVT_TIMER,
            TIMER_MASKED | TIMER_PERIODIC | u64::from(TIMER_VECTOR),
        );
    }
}

/// Подтверждает обычное APIC-прерывание. Spurious vector EOI не требует.
pub fn end_of_interrupt() {
    write_register(X2APIC_EOI, XAPIC_EOI, 0);
}

pub fn local_id() -> u32 {
    if uses_x2apic() {
        read_msr(X2APIC_ID) as u32
    } else {
        read_xapic(XAPIC_ID) >> 24
    }
}

/// Отправляет raw ICR другому local APIC. В x2APIC destination занимает
/// старшие 32 бита ICR; в xAPIC — старший байт отдельного ICR-high регистра.
pub fn send_ipi(destination: u32, command: u32) {
    if uses_x2apic() {
        write_msr(
            X2APIC_ICR,
            (u64::from(destination) << 32) | u64::from(command),
        );
    } else {
        write_xapic(XAPIC_ICR_HIGH, destination << 24);
        write_xapic(XAPIC_ICR_LOW, command);
    }
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
    write_register(X2APIC_DIVIDE_CONFIG, XAPIC_DIVIDE_CONFIG, DIVIDE_BY_16);
    write_register(
        X2APIC_LVT_TIMER,
        XAPIC_LVT_TIMER,
        TIMER_MASKED | u64::from(TIMER_VECTOR),
    );
    write_register(
        X2APIC_INITIAL_COUNT,
        XAPIC_INITIAL_COUNT,
        u64::from(u32::MAX),
    );
    delay_microseconds(tsc_hz, 10_000);
    let current = read_register(X2APIC_CURRENT_COUNT, XAPIC_CURRENT_COUNT) as u32;
    write_register(X2APIC_INITIAL_COUNT, XAPIC_INITIAL_COUNT, 0);
    let elapsed_in_10ms = u32::MAX.saturating_sub(current);
    // Калибровочное окно тоже 10 ms, то есть ровно один quantum 100 Hz.
    PERIODIC_INITIAL_COUNT.store(elapsed_in_10ms.max(100), Ordering::Release);
}

fn uses_x2apic() -> bool {
    APIC_MODE.load(Ordering::Acquire) == MODE_X2APIC
}

fn read_register(msr: u32, xapic_offset: usize) -> u64 {
    if uses_x2apic() {
        read_msr(msr)
    } else {
        u64::from(read_xapic(xapic_offset))
    }
}

fn write_register(msr: u32, xapic_offset: usize, value: u64) {
    if uses_x2apic() {
        write_msr(msr, value);
    } else {
        write_xapic(xapic_offset, value as u32);
    }
}

fn read_xapic(offset: usize) -> u32 {
    // SAFETY: platform bootstrap supervisor-map'ит LAPIC MMIO page identity;
    // регистры выровнены по 16 байт и читаются только после APIC discovery.
    unsafe { ptr::read_volatile((LEGACY_APIC_PHYS + offset) as *const u32) }
}

fn write_xapic(offset: usize, value: u32) {
    // SAFETY: те же условия, что у read_xapic; volatile запрещает компилятору
    // удалять и переупорядочивать архитектурно значимые MMIO-транзакции.
    unsafe { ptr::write_volatile((LEGACY_APIC_PHYS + offset) as *mut u32, value) }
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

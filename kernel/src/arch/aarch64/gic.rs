//! Минимальный, но настоящий GICv3 backend для эталонной QEMU `virt`.
//!
//! Здесь используются три независимых части GICv3:
//!
//! - Distributor описывает SPI и для scheduler PPI не программируется;
//! - Redistributor CPU0 содержит SGI/PPI frame, где PPI 27 переводится в
//!   Group 1, получает priority и включается;
//! - CPU interface доступен через `ICC_*_EL1` system registers и выполняет
//!   обязательную пару acknowledge/EOI для каждого IRQ.
//!
//! Адрес Redistributor у QEMU `virt` — `0x080a_0000`, а не
//! `GICD + 0x0100_0000`: последний адрес равен PL011 и старая реализация
//! буквально посылала служебные значения GIC в serial-консоль.

use core::sync::atomic::{AtomicU32, Ordering};

/// QEMU `virt` memory map. Позже platform discovery заменит эти константы
/// значениями из Device Tree, не меняя CPU-interface код.
const GICR_BASE_CPU0: u64 = 0x080a_0000;
const GICD_BASE: u64 = 0x0800_0000;
const GICD_CTLR: u64 = 0x0000;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ICFGR: u64 = 0x0c00;
// Формула регистра — 0x6000 + 8 * INTID; первый допустимый SPI 32
// действительно находится по 0x6100.
const GICD_IROUTER: u64 = 0x6000;
const GICR_WAKER: u64 = 0x0014;
const GICR_SGI_BASE: u64 = 0x1_0000;
const GICR_IGROUPR0: u64 = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI_BASE + 0x0100;
const GICR_IPRIORITYR0: u64 = GICR_SGI_BASE + 0x0400;

const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const SPURIOUS_INTID_MIN: u32 = 1020;
const RWP: u32 = 1 << 31;

/// PPI 27 — EL1 virtual timer (`CNTV_*`).
///
/// Virtual timer не требует пробрасывать physical timer programming registers
/// через гипервизор и поэтому одинаково работает в QEMU TCG, HVF/UTM и на
/// ARMv8 bare-metal без активного EL2.
pub const TIMER_IRQ: u32 = 27;

static LAST_INTID: AtomicU32 = AtomicU32::new(1023);

/// Будит Redistributor CPU0 и включает Group-1 virtual timer PPI.
pub fn initialize() -> Result<(), ()> {
    // SAFETY: диапазон GICR присутствует в QEMU `virt` и отображён
    // загрузчиком как device-nGnRE.
    unsafe {
        // Distributor обязан разрешить non-secure Group 1 даже для PPI:
        // Redistributor выбирает группу конкретного INTID, но глобальный
        // gate остаётся в GICD_CTLR. ARE_NS нужен GICv3 affinity routing.
        let distributor_control = mmio_read32(GICD_BASE + GICD_CTLR);
        mmio_write32(
            GICD_BASE + GICD_CTLR,
            distributor_control | (1 << 1) | (1 << 5),
        );
        wait_for_clear(GICD_BASE + GICD_CTLR, RWP)?;

        let waker = GICR_BASE_CPU0 + GICR_WAKER;
        let sleeping = mmio_read32(waker);
        mmio_write32(waker, sleeping & !WAKER_PROCESSOR_SLEEP);
        wait_for_clear(waker, WAKER_CHILDREN_ASLEEP)?;

        let group = GICR_BASE_CPU0 + GICR_IGROUPR0;
        mmio_write32(group, mmio_read32(group) | (1 << TIMER_IRQ));
        mmio_write8(
            GICR_BASE_CPU0 + GICR_IPRIORITYR0 + u64::from(TIMER_IRQ),
            0x80,
        );
        mmio_write32(GICR_BASE_CPU0 + GICR_ISENABLER0, 1 << TIMER_IRQ);

        initialize_cpu_interface();
    }
    Ok(())
}

/// Направляет один edge-triggered SPI на boot CPU и включает его как Group 1.
/// QEMU `virt` описывает virtio-mmio IRQ именно как rising-edge SPI. Реальный
/// board adapter позднее возьмёт trigger/affinity из Device Tree тем же API.
pub fn enable_spi(intid: u32) -> Result<(), ()> {
    if !(32..SPURIOUS_INTID_MIN).contains(&intid) {
        return Err(());
    }
    let register = u64::from(intid / 32) * 4;
    let bit = 1u32 << (intid % 32);
    let config_register = u64::from(intid / 16) * 4;
    let config_shift = (intid % 16) * 2;
    // SAFETY: GIC Distributor уже включён initialize(); offsets определены
    // GICv3 и intid проверен до вычисления register/shift.
    unsafe {
        let group = GICD_BASE + GICD_IGROUPR + register;
        mmio_write32(group, mmio_read32(group) | bit);
        mmio_write8(GICD_BASE + GICD_IPRIORITYR + u64::from(intid), 0x70);

        let config = GICD_BASE + GICD_ICFGR + config_register;
        let configured = (mmio_read32(config) & !(0b11 << config_shift)) | (0b10 << config_shift);
        mmio_write32(config, configured);

        // Affinity 0 маршрутизирует SPI на boot CPU0; IRM=0.
        mmio_write64(GICD_BASE + GICD_IROUTER + u64::from(intid) * 8, 0);
        mmio_write32(GICD_BASE + GICD_ISENABLER + register, bit);
        wait_for_clear(GICD_BASE + GICD_CTLR, RWP)?;
    }
    Ok(())
}

unsafe fn wait_for_clear(address: u64, mask: u32) -> Result<(), ()> {
    let mut spins = 0u32;
    while unsafe { mmio_read32(address) } & mask != 0 {
        spins = spins.saturating_add(1);
        if spins == 10_000_000 {
            return Err(());
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Возвращает INTID активного Group-1 interrupt. Вызывается vector stub до
/// переносимого trap handler, чтобы один IRQ всегда имел ровно один EOI.
#[no_mangle]
pub extern "C" fn rustos_gic_acknowledge() -> u32 {
    let intid: u64;
    // SAFETY: CPU interface включён `initialize`; IAR read является
    // архитектурным acknowledge и не обращается к обычной памяти.
    unsafe {
        core::arch::asm!(
            "mrs {intid}, ICC_IAR1_EL1",
            intid = out(reg) intid,
            options(nomem, nostack),
        );
    }
    let intid = intid as u32;
    LAST_INTID.store(intid, Ordering::Relaxed);
    intid
}

/// Завершает interrupt, который вернул последний IAR1 read. Spurious INTID
/// 1020..1023 не является active interrupt и EOI для него запрещён.
#[no_mangle]
pub extern "C" fn rustos_gic_eoi() {
    let intid = LAST_INTID.swap(1023, Ordering::Relaxed);
    if intid >= SPURIOUS_INTID_MIN {
        return;
    }
    // SAFETY: `intid` получен из ICC_IAR1_EL1 на этом CPU и ещё не завершён.
    unsafe {
        core::arch::asm!(
            "msr ICC_EOIR1_EL1, {intid}",
            "isb",
            intid = in(reg) u64::from(intid),
            options(nomem, nostack),
        );
    }
}

unsafe fn initialize_cpu_interface() {
    let mut sre: u64;
    unsafe {
        core::arch::asm!(
            "mrs {sre}, ICC_SRE_EL1",
            sre = out(reg) sre,
            options(nomem, nostack),
        );
        sre |= 1;
        core::arch::asm!(
            "msr ICC_SRE_EL1, {sre}",
            "isb",
            sre = in(reg) sre,
            options(nomem, nostack),
        );

        // PMR=0xff пропускает все приоритеты; BPR=0 не объединяет их в
        // дополнительные группы; IGRPEN1 включает non-secure Group 1.
        let priority_mask = 0xffu64;
        let enable = 1u64;
        core::arch::asm!(
            "msr ICC_PMR_EL1, {priority_mask}",
            "msr ICC_BPR1_EL1, xzr",
            "msr ICC_IGRPEN1_EL1, {enable}",
            "isb",
            priority_mask = in(reg) priority_mask,
            enable = in(reg) enable,
            options(nomem, nostack),
        );
    }
}

#[inline]
unsafe fn mmio_read32(address: u64) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}

#[inline]
unsafe fn mmio_write32(address: u64, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) };
}

#[inline]
unsafe fn mmio_write64(address: u64, value: u64) {
    unsafe { (address as *mut u64).write_volatile(value) };
}

#[inline]
unsafe fn mmio_write8(address: u64, value: u8) {
    unsafe { (address as *mut u8).write_volatile(value) };
}

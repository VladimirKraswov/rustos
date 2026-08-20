//! Архитектурозависимый код (x86-64): ASM-вставки, порты, halt, mem-builtins.
//!
//! Все `unsafe`-блоки документируются: ядро — единственный код с ring 0
//! привилегиями, и каждый доступ к порту/регистру осознан.
//! Инициализация не нужна (порты x86 работают «из коробки» в long mode).

// Часть функций (например, `outw` под ACPI PM) пока не используется —
// модуль расширяется по мере появления драйверов, поэтому allow на уровень модуля.
#![allow(dead_code)]

pub mod apic;
pub mod mem;
pub mod segmentation;
pub mod smp;
pub mod traps;

/// Остановка текущего CPU до следующего прерывания.
///
/// # Safety
///
/// HLT требует ring 0: функция вызывается только из ядра.
pub fn halt() {
    unsafe {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

/// Текущий PML4 physical address (CR3 без PCID bits).
pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack)) };
    value & 0x000f_ffff_ffff_f000
}

/// Переключает address space текущего CPU.
///
/// # Safety
///
/// `root` должен быть physical address валидного PML4, содержащего mappings
/// исполняемого kernel-кода и текущего стека.
pub unsafe fn write_cr3(root: u64) {
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack)) };
}

/// Адрес последнего page fault.
pub fn read_cr2() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr2", out(reg) value, options(nomem, nostack)) };
    value
}

/// Включает NX pages (EFER.NXE) и защиту read-only supervisor pages (CR0.WP).
pub fn enable_memory_protection() {
    const EFER: u32 = 0xC000_0080;
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") EFER,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        );
        let value = (u64::from(high) << 32) | u64::from(low) | (1 << 11);
        core::arch::asm!(
            "wrmsr",
            in("ecx") EFER,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack),
        );
        let mut cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 |= 1 << 16;
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nostack));
    }
}

/// Завершение VM через QEMU isa-debug-exit: запись 32-битного значения в
/// IO-порт 0xF4 заставляет QEMU выйти с этим значением как кодом возврата.
/// В системе без устройства запись игнорируется (безопасный no-op).
pub fn debug_exit(code: u8) {
    // SAFETY: 0xF4 — отдельный тестовый порт QEMU; запись u32 допустима.
    unsafe {
        // Intel-синтаксис (default в rustc asm!): явные регистры пишутся
        // в шаблон напрямую, без плейсхолдеров. Операнды явных регистров
        // нужны, чтобы аллокатор узнал об их занятости.
        core::arch::asm!(
            "out dx, eax",
            in("eax") (code as u32),
            in("dx") 0xF4u16,
            options(nomem, nostack)
        );
    }
}

/// Чтение 8-битного значения из IO-порта (для serial и ранней диагностики).
///
/// # Safety
///
/// Порт выбирает вызывающий; в ring 0 чтение любого порта выполнимо,
/// но эффект на оборудование зависит от порта — вызывающий отвечает за это.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: порт выбирает вызывающий (контракт функции);
    // в ring 0 чтение любого порта выполнимо.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") val,
            in("dx") port,
            options(nomem, nostack)
        );
    }
    val
}

/// Запись 8-битного значения в IO-порт.
///
/// # Safety
///
/// См. [`inb`]: вызывающий отвечает за корректность порта.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    // SAFETY: см. [`inb`]: вызывающий отвечает за корректность порта.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("al") val,
            in("dx") port,
            options(nomem, nostack)
        );
    }
}

/// Запись 16-битного значения в I/O-порт (ACPI PM control и драйверы).
///
/// # Safety
///
/// Вызывающий обязан выбрать устройство и допустимое для него значение.
#[inline]
pub unsafe fn outw(port: u16, val: u16) {
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("ax") val,
            in("dx") port,
            options(nomem, nostack)
        );
    }
}

//! Ранняя serial-консоль, выбранная загрузчиком из описания платформы.
//!
//! ## Почему serial
//!
//! Serial — единственный канал, доступный с самых ранних стадий загрузки
//! (до framebuffer'а, до scheduler'а, в любом режиме) и видимый из QEMU
//! (`-serial`) и из CI. Все ранние panic-сообщения идут сюда.
//!
//! ## Параметры
//!
//! 115200 бит/с, 8N1, FIFO включены. Скорость не критична для диагностики:
//! вывод идёт малыми порциями, а spin-wait на THR-пустоту — приемлемая
//! цена за простоту (без прерываний и без DMA).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rustos_abi::{
    bootinfo::{BOOT_CONSOLE_16550_MMIO, BOOT_CONSOLE_16550_PORT, BOOT_CONSOLE_PL011},
    BootInfo,
};

static CONSOLE_KIND: AtomicU32 = AtomicU32::new(0);
static CONSOLE_BASE: AtomicU64 = AtomicU64::new(0);

/// Смещение Line Status Register (LSR): статус THR/DR, ошибки.
const LSR: u8 = 5;
/// Флаг «транслятор-регистр (THR) пуст».
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Настройка UART: 115200 8N1, прерывания отключены, FIFO включены.
///
/// # Safety
///
/// Доступ к портам COM1 — см. `crate::arch`.
pub fn init(info: &BootInfo) {
    CONSOLE_BASE.store(info.console.base, Ordering::Relaxed);
    CONSOLE_KIND.store(info.console.kind, Ordering::Release);
    match info.console.kind {
        BOOT_CONSOLE_16550_PORT => initialize_16550_port(info.console.base),
        BOOT_CONSOLE_16550_MMIO => initialize_16550_mmio(info.console.base),
        // PL011 обычно уже настроен UEFI/firmware. Перепрограммировать baud
        // без достоверного clock_hz опаснее, чем продолжить текущий режим.
        _ => {}
    }
}

/// Запись без инициализации UART (диагностический маркер в `_start`).
///
/// В QEMU любой байт в THR принимается независимо от делителя; на реальном
/// железе предполагает заводской divisor 12 (1843200/12 = 115200) — если
/// порт уже настроен firmware, иначе скорость будет другой.
pub fn early_put_str(s: &str) {
    for &b in s.as_bytes() {
        put_byte(b);
    }
}

/// Отправить один байт (spin-wait до готовности THR).
///
/// # Safety
///
/// Доступ к портам — см. `crate::arch`.
fn put_byte(b: u8) {
    let base = CONSOLE_BASE.load(Ordering::Relaxed);
    match CONSOLE_KIND.load(Ordering::Acquire) {
        BOOT_CONSOLE_16550_PORT => write_16550_port(base, b),
        BOOT_CONSOLE_16550_MMIO => write_16550_mmio(base, b),
        BOOT_CONSOLE_PL011 => write_pl011(base, b),
        _ => {}
    }
}

#[cfg(target_arch = "x86_64")]
fn initialize_16550_port(base: u64) {
    let Ok(base) = u16::try_from(base) else {
        return;
    };
    unsafe {
        crate::arch::outb(base + 1, 0x00);
        crate::arch::outb(base + 3, 0x80);
        crate::arch::outb(base, 1);
        crate::arch::outb(base + 1, 0);
        crate::arch::outb(base + 3, 0x03);
        crate::arch::outb(base + 2, 0x07);
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn initialize_16550_port(_base: u64) {}

#[cfg(target_arch = "x86_64")]
fn write_16550_port(base: u64, byte: u8) {
    let Ok(base) = u16::try_from(base) else {
        return;
    };
    unsafe {
        while crate::arch::inb(base + u16::from(LSR)) & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        crate::arch::outb(base, byte);
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn write_16550_port(_base: u64, _byte: u8) {}

fn initialize_16550_mmio(base: u64) {
    if base == 0 {
        return;
    }
    unsafe {
        write_mmio_u8(base, 1, 0x00);
        write_mmio_u8(base, 3, 0x80);
        write_mmio_u8(base, 0, 1);
        write_mmio_u8(base, 1, 0);
        write_mmio_u8(base, 3, 0x03);
        write_mmio_u8(base, 2, 0x07);
    }
}

fn write_16550_mmio(base: u64, byte: u8) {
    if base == 0 {
        return;
    }
    unsafe {
        while read_mmio_u8(base, u64::from(LSR)) & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        write_mmio_u8(base, 0, byte);
    }
}

fn write_pl011(base: u64, byte: u8) {
    if base == 0 {
        return;
    }
    // PL011 UARTFR.TXFF (bit 5): FIFO полон.
    unsafe {
        while (base.wrapping_add(0x18) as *const u32).read_volatile() & (1 << 5) != 0 {
            core::hint::spin_loop();
        }
        (base as *mut u32).write_volatile(u32::from(byte));
    }
}

unsafe fn read_mmio_u8(base: u64, offset: u64) -> u8 {
    unsafe { (base.wrapping_add(offset) as *const u8).read_volatile() }
}

unsafe fn write_mmio_u8(base: u64, offset: u64, value: u8) {
    unsafe { (base.wrapping_add(offset) as *mut u8).write_volatile(value) };
}

/// Вывести строку. `\n` преобразуется в `\r\n` (конвенция терминалов).
pub fn put_str(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            put_byte(b'\r');
        }
        put_byte(b);
    }
}

/// Вывести значение как hex (без префикса `0x`).
pub fn put_hex(v: u64) {
    let mut tmp = [b'0'; 16];
    let mut i = 15;
    let mut x = v;
    while x > 0 && i > 0 {
        tmp[i] = b"0123456789abcdef"[(x & 0xF) as usize];
        x >>= 4;
        i -= 1;
    }
    put_str(core::str::from_utf8(&tmp[i..]).unwrap_or("0"));
}

/// Вывести значение как десятичный (без heap: рекурсивно по цифрам).
pub fn put_u32(v: u32) {
    if v >= 10 {
        put_u32(v / 10);
        put_u32(v % 10);
    } else {
        put_byte(b'0' + v as u8);
    }
}

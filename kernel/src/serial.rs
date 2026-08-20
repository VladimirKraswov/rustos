//! Serial-консоль (16550 UART, COM1, 0x3F8) — обязательный диагностический
//! канал RustOS.
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

use crate::arch::{inb, outb};

/// Базовый адрес COM1.
const COM1_BASE: u16 = 0x3F8;

/// Регистр доступа к делителю (DLAB).
const LSR: u8 = 5;
/// Флаг «транслятор-регистр (THR) пуст».
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Настройка UART: 115200 8N1, прерывания отключены, FIFO включены.
///
/// # Safety
///
/// Доступ к портам COM1 — см. `crate::arch`.
pub fn init() {
    unsafe {
        // Отключить прерывания (IER = 0): на этом этапе нет IDT.
        outb(COM1_BASE + 1, 0x00);
        // DLAB=1: открыть делитель. Baud = 1843200 / 1 = 115200.
        outb(COM1_BASE + 3, 0x80);
        outb(COM1_BASE, 1); // делитель по низшим битам
        outb(COM1_BASE + 1, 0); // делитель по старшим битам
                                // DLAB=0, 8 бит данных, без stop-бита и parity (0b0011).
        outb(COM1_BASE + 3, 0x03);
        // Включить FIFO и сбросить его.
        outb(COM1_BASE + 2, 0x07);
    }
}

/// Запись без инициализации UART (диагностический маркер в `_start`).
///
/// В QEMU любой байт в THR принимается независимо от делителя; на реальном
/// железе предполагает divisor по умолчанию 16550 (12 = 115200).
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
    unsafe {
        loop {
            if inb(COM1_BASE + u16::from(LSR)) & LSR_THR_EMPTY != 0 {
                outb(COM1_BASE, b);
                return;
            }
        }
    }
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

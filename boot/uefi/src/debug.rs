//! Сырой UART для диагностики загрузчика.
//!
//! Единственный канал вывода после `ExitBootServices` (ConOut к этому
//! моменту уже свободен/недоступен).
//!
//! - x86-64: COM1 16550, port I/O 0x3F8.
//! - AArch64: PL011, MMIO 0x09000000 (QEMU virt).

/// Настройка UART. x86: 16550 115200 8N1. AArch64: no-op (firmware).
pub fn init() {
    #[cfg(target_arch = "x86_64")]
    {
        const COM1_BASE: u16 = 0x3F8;
        crate::arch::outb(COM1_BASE + 1, 0x00); // IER: без прерываний
        crate::arch::outb(COM1_BASE + 3, 0x80); // DLAB=1
        crate::arch::outb(COM1_BASE, 1); // делитель: низний байт
        crate::arch::outb(COM1_BASE + 1, 0); // делитель: верхний байт
        crate::arch::outb(COM1_BASE + 3, 0x03); // DLAB=0, 8N1
        crate::arch::outb(COM1_BASE + 2, 0x07); // FIFO on + сброс
    }
    #[cfg(target_arch = "aarch64")]
    {
        // PL011 уже настроен AAVMF; перепрограммировать baud не нужно.
    }
}

/// Вывести строку; `\n` → `\r\n` (конвенция терминалов).
pub fn put_str(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            put_byte(b'\r');
        }
        put_byte(b);
    }
}

fn put_byte(b: u8) {
    #[cfg(target_arch = "x86_64")]
    {
        const COM1_BASE: u16 = 0x3F8;
        const LSR: u16 = 5;
        const LSR_THR_EMPTY: u8 = 1 << 5;
        loop {
            if crate::arch::inb(COM1_BASE + LSR) & LSR_THR_EMPTY != 0 {
                crate::arch::outb(COM1_BASE, b);
                return;
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        const PL011_BASE: u64 = 0x0900_0000;
        const TXFF_OFFSET: u64 = 0x18;
        const TXFF_BIT: u32 = 1 << 5;
        // SAFETY: PL011 — MMIO QEMU virt, доступен в AAVMF identity map.
        unsafe {
            loop {
                let txff_ptr = (PL011_BASE.wrapping_add(TXFF_OFFSET)) as *const u32;
                if txff_ptr.read_volatile() & TXFF_BIT == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            let dr_ptr = PL011_BASE as *mut u32;
            dr_ptr.write_volatile(u32::from(b));
        }
    }
}

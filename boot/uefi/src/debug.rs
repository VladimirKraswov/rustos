//! Сырой UART (COM1, 0x3F8) для диагностики загрузчика.
//!
//! Единственный канал вывода после `ExitBootServices` (ConOut к этому
//! моменту уже свободен/недоступен). QEMU принимает любые записи в THR
//! независимо от делителя, поэтому вывод работает даже до полной инициализации
//! 16550; `init()` — стандартная конфигурация (как `kernel::serial`).

const COM1_BASE: u16 = 0x3F8;
const LSR: u16 = 5;
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Прочитать байт из I/O-порта.
#[inline]
fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: чтение из I/O-порта не затрагивает память; порты COM1 —
    // корректность гарантирует вызывающий.
    unsafe { core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)) }
    v
}

/// Записать байт в I/O-порт.
#[inline]
fn outb(port: u16, v: u8) {
    // SAFETY: запись в I/O-порт не затрагивает память.
    unsafe { core::arch::asm!("out dx, al", in("al") v, in("dx") port, options(nomem, nostack)) }
}

/// Настройка 16550: 115200 8N1, прерывания отключены, FIFO включены.
pub fn init() {
    outb(COM1_BASE + 1, 0x00); // IER: без прерываний
    outb(COM1_BASE + 3, 0x80); // DLAB=1
    outb(COM1_BASE, 1); // делитель: низний байт (1843200/1 = 115200)
    outb(COM1_BASE + 1, 0); // делитель: верхний байт
    outb(COM1_BASE + 3, 0x03); // DLAB=0, 8N1
    outb(COM1_BASE + 2, 0x07); // FIFO on + сброс
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
    loop {
        if inb(COM1_BASE + LSR) & LSR_THR_EMPTY != 0 {
            outb(COM1_BASE, b);
            return;
        }
    }
}

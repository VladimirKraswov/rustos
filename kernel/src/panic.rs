//! Panic handler ядра.
//!
//! На этапе 0 (нет прерываний, нет scheduler'а) единственное разумное
//! действие при panic — вывести сообщение в serial и остановить CPU:
//! CI увидит отсутствие «exit code 0» и текст panic'а в serial-логе.
//! С этапа 3 handler расширится: вывод реестров, backtrace, triple-fault
//! для гарантированного «шума» в monitor'е QEMU.

use core::fmt::{self, Write};

use crate::serial;

/// Буфер panic-сообщения фиксированного размера (в ядре нет heap).
struct MsgBuf {
    buf: [u8; 256],
    len: usize,
    overflowed: bool,
}

impl Write for MsgBuf {
    /// Записывает строку в буфер, отбрасывая хвост при переполнении.
    ///
    /// `Write::write_str` не может «отказать» (нет I/O-ошибок в памяти),
    /// поэтому `Ok(())` — единственный исход.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            } else {
                self.overflowed = true;
            }
        }
        Ok(())
    }
}

/// Точка входа при panic.
#[panic_handler]
fn panic_handler(payload: &core::panic::PanicInfo) -> ! {
    serial::put_str("\n*** RUSTOS KERNEL PANIC ***\n");

    // `PanicInfo::message()` (1.81+) — форматированное сообщение panic!
    // без устаревшего `payload()`. Пишем в фиксированный буфер.
    let mut msg = MsgBuf {
        buf: [0u8; 256],
        len: 0,
        overflowed: false,
    };
    // `write!` не может ошибиться: `MsgBuf` всегда принимает байты.
    let _ = write!(msg, "{}", payload.message());
    let s = core::str::from_utf8(&msg.buf[..msg.len]).unwrap_or("<non-utf8>");
    serial::put_str("  message: ");
    serial::put_str(s);
    if msg.overflowed {
        serial::put_str(" ... (truncated)");
    }
    serial::put_str("\n");

    if let Some(loc) = payload.location() {
        serial::put_str("  location: ");
        serial::put_str(loc.file());
        serial::put_str(":");
        serial::put_u32(loc.line());
        serial::put_str("\n");
    }

    serial::put_str("  NOTE: kernel has stopped; check QEMU serial log.\n");
    loop {
        crate::arch::halt();
    }
}

//! Проверка полного пути `RUNE _start -> std::rt -> fn main`.
//!
//! В этом файле намеренно нет `#![no_main]`, ручных syscalls и особой точки
//! входа: именно так должна выглядеть новая пользовательская программа.

#![feature(restricted_std)]

// Ссылка сообщает rustc, что программа использует RustOS CRT. SDK добавляет
// linker flag `-u_start`, поэтому startup object извлекается из rlib.
use rustos_crt as _;

fn main() {
    let arguments: std::vec::Vec<_> = std::env::args().collect();
    assert_eq!(arguments, ["std-main", "--self-test"]);
    assert_eq!(std::env::var("RUSTOS_MODE").as_deref(), Ok("developer"));

    // Изменения environment являются локальной политикой процесса и не
    // требуют kernel syscall или глобального mutable состояния ОС.
    unsafe { std::env::set_var("RUSTOS_CHILD", "ready") };
    assert_eq!(std::env::var("RUSTOS_CHILD").as_deref(), Ok("ready"));
    unsafe { std::env::remove_var("RUSTOS_CHILD") };
    assert!(std::env::var("RUSTOS_CHILD").is_err());
}

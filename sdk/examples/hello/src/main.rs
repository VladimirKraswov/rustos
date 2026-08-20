//! Минимальная обычная программа RustOS.
//!
//! В приложении нет `_start`, syscall assembly или знания о runner: CRT,
//! upstream `std` и system capabilities предоставляются SDK.

#![cfg_attr(target_os = "rustos", feature(restricted_std))]

#[cfg(target_os = "rustos")]
use rustos_crt as _;

#[cfg(target_os = "rustos")]
fn main() {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| String::from("developer"));
    println!("Hello, {name}, from a VaraniaFS RUNE application!");
}

#[cfg(not(target_os = "rustos"))]
fn main() {
    eprintln!("Этот пример предназначен для target x86_64-unknown-rustos");
}

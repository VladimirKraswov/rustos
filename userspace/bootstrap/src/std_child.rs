//! Дочерняя обычная std-программа для проверки Command + pipes + stdio.

#![feature(restricted_std)]

use rustos_crt as _;
use std::{io::Write, process::ExitCode};

const STRESS_BYTES: usize = 12 * 1024;

fn main() -> ExitCode {
    let argument = std::env::args().nth(1).unwrap_or_default();
    println!("child-out:{argument}");
    eprintln!("child-err:ready");
    if argument == "stress" {
        // Объём намеренно втрое больше kernel pipe. Последовательное чтение
        // stdout/stderr в родителе гарантированно зависло бы на этом тесте.
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        stdout.lock().write_all(&[b'O'; STRESS_BYTES]).unwrap();
        stderr.lock().write_all(&[b'E'; STRESS_BYTES]).unwrap();
    }
    ExitCode::from(17)
}

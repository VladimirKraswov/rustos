//! Первый настоящий `std` процесс RustOS.
//!
//! Точка входа пока явная: окончательный `lang_start` появится вместе с
//! process startup ABI для обычного `fn main`. Сам код ниже использует именно
//! upstream `std`, собранную через `-Zbuild-std`, а не локальный facade.

#![no_main]
#![feature(restricted_std)]

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    hint,
    io::{Read, Seek, SeekFrom, Write},
    string::String,
    sync::Mutex,
    time::Instant,
    vec::Vec,
};

const ABI_VERSION: u64 = 4;

unsafe extern "C" {
    fn __rustos_std_vfs_init(server: u32, reply: u32) -> i32;
    fn __rustos_std_vfs_shutdown() -> i32;
}

#[no_mangle]
pub extern "C" fn _start(vfs_server: u64, vfs_reply: u64, abi_version: u64) -> ! {
    if abi_version != ABI_VERSION {
        process_exit(1);
    }
    if unsafe { __rustos_std_vfs_init(vfs_server as u32, vfs_reply as u32) } != 0 {
        process_exit(2);
    }
    let started = Instant::now();

    // Vec/String проверяют GlobalAlloc -> vm_map и realloc/dealloc path.
    let mut numbers = Vec::with_capacity(64);
    for value in 0u64..64 {
        numbers.push(value * value);
    }
    if numbers.iter().sum::<u64>() != 85_344 {
        finish(3);
    }
    let greeting = String::from("RUNE + Rust std");
    if greeting.len() != 15 || !greeting.starts_with("RUNE") {
        finish(4);
    }

    // Обе коллекции важны: BTreeMap создаёт много небольших allocations,
    // HashMap дополнительно вызывает platform random seed.
    let mut ordered = BTreeMap::new();
    let mut hashed = HashMap::new();
    for (index, value) in numbers.iter().copied().enumerate() {
        ordered.insert(index, value);
        hashed.insert(index, value);
    }
    if ordered.get(&17) != Some(&289) || hashed.get(&31) != Some(&961) {
        finish(5);
    }

    // Mutex fast path уже проходит RustOS futex backend. Contended path будет
    // отдельным тестом после включения std::thread::spawn.
    let shared = Mutex::new(40u64);
    *shared.lock().unwrap() += 2;
    if *shared.lock().unwrap() != 42 {
        finish(6);
    }

    // CLOCK_MONOTONIC не обязан измениться за столь короткий интервал, но не
    // может пойти назад.
    let before = started.elapsed();
    hint::spin_loop();
    let after = started.elapsed();
    if after < before {
        finish(7);
    }

    if !verify_std_fs() {
        finish(8);
    }

    finish(0)
}

/// Проверяет публичный `std::fs`, а не внутренние IPC helpers. Поэтому этот
/// тест одновременно защищает привычную для переносимых Linux/Rust программ
/// API-семантику и границу `std -> IPC -> vfsd -> VaraniaFS`.
fn verify_std_fs() -> bool {
    const DIRECTORY: &str = "/std-port-smoke";
    const FIRST: &str = "/std-port-smoke/source.txt";
    const SECOND: &str = "/std-port-smoke/result.txt";
    const CONTENT: &str = "std::fs over capability IPC";

    let _ = fs::remove_file(FIRST);
    let _ = fs::remove_file(SECOND);
    let _ = fs::remove_dir(DIRECTORY);
    if fs::create_dir(DIRECTORY).is_err() {
        return false;
    }

    let result = (|| -> std::io::Result<bool> {
        let mut file = File::create(FIRST)?;
        file.write_all(CONTENT.as_bytes())?;
        file.flush()?;
        file.seek(SeekFrom::Start(0))?;
        drop(file);

        let mut file = OpenOptions::new().read(true).append(true).open(FIRST)?;
        file.write_all(b"!")?;
        file.seek(SeekFrom::Start(0))?;
        let mut actual = String::new();
        file.read_to_string(&mut actual)?;
        drop(file);
        if actual != "std::fs over capability IPC!" {
            return Ok(false);
        }

        let metadata = fs::metadata(FIRST)?;
        if !metadata.is_file() || metadata.len() != (CONTENT.len() + 1) as u64 {
            return Ok(false);
        }
        fs::rename(FIRST, SECOND)?;
        let mut found = false;
        for entry in fs::read_dir(DIRECTORY)? {
            let entry = entry?;
            if entry.file_name() == "result.txt" && entry.file_type()?.is_file() {
                found = true;
            }
        }
        Ok(found)
    })();

    let cleanup = fs::remove_file(SECOND).and_then(|_| fs::remove_dir(DIRECTORY));
    matches!(result, Ok(true)) && cleanup.is_ok()
}

fn finish(status: i32) -> ! {
    if unsafe { __rustos_std_vfs_shutdown() } != 0 {
        process_exit(if status == 0 { 9 } else { status });
    }
    process_exit(status)
}

fn process_exit(status: i32) -> ! {
    unsafe {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!(
            "int 0x80",
            in("rax") 1u64,
            in("rdi") status as i64 as u64,
            in("rsi") 0u64,
            in("rdx") 0u64,
            options(noreturn),
        );
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!(
            "svc #0",
            in("x8") 1u64,
            in("x0") status as i64 as u64,
            in("x1") 0u64,
            in("x2") 0u64,
            options(noreturn),
        );
    }
}

//! Первый настоящий `std` процесс RustOS.
//!
//! Это обычная программа с `fn main`: RustOS CRT и std PAL сами принимают
//! ProcessStartInfo, argv/env и типизированные VFS capabilities.

#![feature(restricted_std)]

use rustos_crt as _;
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
    fs::{self, OpenOptions},
    hint,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    process::{Command, ExitCode},
    string::String,
    sync::{Arc, Barrier, Mutex},
    thread,
    time::Instant,
    vec::Vec,
};

unsafe extern "C" {
    fn __rustos_std_vfs_shutdown() -> i32;
}

thread_local! {
    /// Ненулевой initial image проверяет, что новый поток копирует TLS template,
    /// а не просто получает zero-filled область с подходящим FS base.
    static WORKER_TLS: Cell<u32> = const { Cell::new(7) };
}

fn main() -> ExitCode {
    let started = Instant::now();

    // Vec/String проверяют GlobalAlloc -> vm_map и realloc/dealloc path.
    let mut numbers = Vec::with_capacity(64);
    for value in 0u64..64 {
        numbers.push(value * value);
    }
    if numbers.iter().sum::<u64>() != 85_344 {
        return finish(3);
    }
    let greeting = String::from("RUNE + Rust std");
    if greeting.len() != 15 || !greeting.starts_with("RUNE") {
        return finish(4);
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
        return finish(5);
    }

    // Mutex fast path уже проходит RustOS futex backend. Contended path будет
    // отдельным тестом после включения std::thread::spawn.
    let shared = Mutex::new(40u64);
    *shared.lock().unwrap() += 2;
    if *shared.lock().unwrap() != 42 {
        return finish(6);
    }

    // CLOCK_MONOTONIC не обязан измениться за столь короткий интервал, но не
    // может пойти назад.
    let before = started.elapsed();
    hint::spin_loop();
    let after = started.elapsed();
    if after < before {
        return finish(7);
    }

    if !verify_std_fs() {
        return finish(8);
    }
    if !verify_threads() {
        return finish(10);
    }
    if !verify_process_and_pipes() {
        return finish(11);
    }
    if !verify_native_sdk_tool() {
        return finish(12);
    }
    if !verify_vfs_executable() {
        return finish(13);
    }
    if !verify_sdk_example() {
        return finish(14);
    }

    finish(0)
}

fn verify_process_and_pipes() -> bool {
    // Сначала стресс: так его диагностика не зависит от очистки предыдущего
    // процесса и отдельно защищает первый lifecycle проход.
    let Ok(stress) = Command::new("std-child").arg("stress").output() else {
        return false;
    };
    let stdout_prefix = b"child-out:stress\n";
    let stderr_prefix = b"child-err:ready\n";
    let stress_ok = stress.status.code() == Some(17)
        && stress.stdout.len() == stdout_prefix.len() + 12 * 1024
        && stress.stderr.len() == stderr_prefix.len() + 12 * 1024
        && stress.stdout.starts_with(stdout_prefix)
        && stress.stderr.starts_with(stderr_prefix)
        && stress.stdout[stdout_prefix.len()..]
            .iter()
            .all(|byte| *byte == b'O')
        && stress.stderr[stderr_prefix.len()..]
            .iter()
            .all(|byte| *byte == b'E');
    if !stress_ok {
        return false;
    }

    let output = Command::new("/boot/system/bin/std-child.rune")
        .arg("from-parent")
        .output();
    let Ok(output) = output else { return false };
    output.status.code() == Some(17)
        && output.stdout == b"child-out:from-parent\n"
        && output.stderr == b"child-err:ready\n"
}

/// Запускает уже нативную системную утилиту SDK. Она читает RUNE DLL через
/// унаследованную VFS capability, проверяет hash/TOC и пишет результат в pipe.
/// Это тот же build-tool, который позднее будет вызывать native Cargo.
fn verify_native_sdk_tool() -> bool {
    let Ok(output) = Command::new("rune")
        .arg("verify")
        .arg("/system/lib/fixture-1.rune")
        .output()
    else {
        return false;
    };
    output.status.success()
        && output.stderr.is_empty()
        && String::from_utf8(output.stdout).is_ok_and(|stdout| stdout.starts_with("RUNE OK:"))
}

/// Публичный `Command` сам выбирает маленький runner из initramfs и загружает
/// target непосредственно из VaraniaFS. Приложению не нужно знать о
/// bootstrap-механизме; argv, stdio и VFS capabilities сохраняются.
fn verify_vfs_executable() -> bool {
    let Ok(output) = Command::new("/apps/sdk/std-child.rune")
        .arg("via-vfs")
        .output()
    else {
        return false;
    };
    output.status.code() == Some(17)
        && output.stdout == b"child-out:via-vfs\n"
        && output.stderr == b"child-err:ready\n"
}

fn verify_sdk_example() -> bool {
    let Ok(output) = Command::new("/apps/examples/hello.rune")
        .arg("student")
        .output()
    else {
        return false;
    };
    output.status.success()
        && output.stderr.is_empty()
        && output.stdout == b"Hello, student, from a VaraniaFS RUNE application!\n"
}

fn verify_threads() -> bool {
    let value = Arc::new(Mutex::new(0u64));
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for worker in 0..2u32 {
        let value = Arc::clone(&value);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            let tls_ok = WORKER_TLS.with(|cell| {
                let initial = cell.get();
                cell.set(20 + worker);
                initial == 7 && cell.get() == 20 + worker
            });
            barrier.wait();
            for _ in 0..64 {
                *value.lock().unwrap() += 1;
                thread::yield_now();
            }
            tls_ok
        }));
    }
    barrier.wait();
    let tls_ok = workers
        .into_iter()
        .all(|worker| worker.join().unwrap_or(false));
    let final_value = *value.lock().unwrap();
    tls_ok && final_value == 128
}

/// Проверяет публичный `std::fs`, а не внутренние IPC helpers. Поэтому этот
/// тест одновременно защищает привычную для переносимых Linux/Rust программ
/// API-семантику и границу `std -> IPC -> vfsd -> VaraniaFS`.
fn verify_std_fs() -> bool {
    const DIRECTORY: &str = "/std-port-smoke";
    const FIRST: &str = "/std-port-smoke/source.txt";
    const SECOND: &str = "/std-port-smoke/result.txt";
    const NESTED: &str = "/std-port-smoke/nested";
    const CONTENT: &str = "std::fs over capability IPC";

    let _ = fs::remove_file(FIRST);
    let _ = fs::remove_file(SECOND);
    let _ = fs::remove_dir_all(NESTED);
    let _ = fs::remove_dir(DIRECTORY);
    if fs::create_dir(DIRECTORY).is_err() {
        return false;
    }

    let result = (|| -> std::io::Result<bool> {
        fs::create_dir(NESTED)?;
        std::env::set_current_dir(DIRECTORY)?;
        if std::env::current_dir()? != Path::new(DIRECTORY)
            || std::env::current_exe()?.file_name() != Some(std::ffi::OsStr::new("std-smoke"))
        {
            return Ok(false);
        }

        // Относительный путь проходит через process-local CWD, а не через
        // состояние kernel или vfsd. Это именно та семантика, которая нужна
        // Cargo при обходе workspace.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open("source.txt")?;
        file.write_all(CONTENT.as_bytes())?;
        file.flush()?;
        file.set_len(8193)?;
        file.seek(SeekFrom::Start(8192))?;
        let mut sparse_tail = [1u8; 1];
        file.read_exact(&mut sparse_tail)?;
        if sparse_tail != [0] {
            return Ok(false);
        }
        file.set_len(CONTENT.len() as u64)?;
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
        if fs::canonicalize("nested/../result.txt")? != Path::new(SECOND) {
            return Ok(false);
        }
        std::env::set_current_dir("/")?;
        fs::remove_dir_all(NESTED)?;
        Ok(found)
    })();

    let _ = std::env::set_current_dir("/");
    let _ = fs::remove_dir_all(NESTED);
    let cleanup = fs::remove_file(SECOND).and_then(|_| fs::remove_dir(DIRECTORY));
    matches!(result, Ok(true)) && cleanup.is_ok()
}

fn finish(status: u8) -> ExitCode {
    if unsafe { __rustos_std_vfs_shutdown() } != 0 {
        return ExitCode::from(if status == 0 { 9 } else { status });
    }
    ExitCode::from(status)
}

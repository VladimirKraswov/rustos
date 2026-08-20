//! Одноразовый сериализующий клиент QEMU Human Monitor Protocol.
//!
//! Системные варианты `nc` на macOS и Linux по-разному закрывают Unix socket.
//! Этот маленький host tool держит одно соединение, ждёт prompt после каждой
//! команды и поэтому не переполняет виртуальный PS/2 controller.

use std::{
    env,
    io::{self, BufRead, Read, Write},
    os::unix::net::UnixStream,
    process::ExitCode,
    thread,
    time::Duration,
};

const PROMPT: &[u8] = b"(qemu)";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustos-hmp: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Подключается к monitor-сокету и проксирует stdin в HMP, ожидая prompt
/// после каждой команды (последовательность гарантирует, что monitor не
/// потеряет следующий ввод).
fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let socket = args
        .next()
        .ok_or("usage: rustos-hmp <monitor.sock> [delay-ms]")?;
    let delay_ms: u64 = args
        .next()
        .unwrap_or_else(|| "0".into())
        .parse()
        .map_err(|_| "delay-ms must be an integer")?;
    if args.next().is_some() {
        return Err("usage: rustos-hmp <monitor.sock> [delay-ms]".into());
    }

    let mut stream = UnixStream::connect(&socket).map_err(|e| format!("connect {socket}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set timeout: {e}"))?;
    wait_for_prompt(&mut stream).map_err(|e| format!("initial prompt: {e}"))?;

    let stdin = io::stdin();
    for line in stdin.lock().split(b'\n') {
        let mut line = line.map_err(|e| format!("stdin: {e}"))?;
        if line.is_empty() {
            continue;
        }
        let is_quit = line == b"quit";
        line.push(b'\n');
        stream
            .write_all(&line)
            .map_err(|e| format!("write command: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        if !is_quit {
            wait_for_prompt(&mut stream).map_err(|e| format!("command prompt: {e}"))?;
        }
        if delay_ms != 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    Ok(())
}

/// Читает байты до появления `PROMPT` (скользящее окно), EOF до prompt — ошибка.
fn wait_for_prompt(stream: &mut UnixStream) -> io::Result<()> {
    let mut window = [0u8; PROMPT.len()];
    let mut filled = 0usize;
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "monitor closed before prompt",
            ));
        }
        if filled < window.len() {
            window[filled] = byte[0];
            filled += 1;
        } else {
            window.copy_within(1.., 0);
            window[window.len() - 1] = byte[0];
        }
        if filled == window.len() && window == PROMPT {
            return Ok(());
        }
    }
}

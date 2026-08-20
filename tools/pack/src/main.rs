//! `rustos-pack` — упаковка каталога в initramfs-образ RustOS (формат RIFS v1).
//!
//! ## Формат (docs/BOOT.md)
//!
//! ```text
//! [0..32)    Заголовок (little-endian):
//!               u32 magic      = 0x52494653 ("RIFS")
//!               u32 version    = 1
//!               u32 count      — число файлов
//!               u32 reserved   = 0
//!               u64 image_size — итоговый размер образа (кратно 4096)
//!               u64 reserved2  = 0 (заполняет заголовок до 32 байт)
//! [32..32+n*64) Таблица файлов (по 64 байта):
//!               u8  name[48]  — относительный путь (NUL-терминирован, ≤ 47 символов)
//!               u64 size      — размер данных файла
//!               u64 offset    — смещение данных от начала образа
//! [данные]   Каждый файл, выровнен по 4096, дополнен нулями.
//! ```
//!
//! Порядок файлов в таблице — лексикографический (воспроизводимость сборки).
//!
//! ## Использование
//!
//! ```text
//! rustos-pack <каталог> <выходной.имг>
//! rustos-pack --verify <образ.имг>
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const MAGIC: u32 = 0x52_49_46_53; // "RIFS"
const VERSION: u32 = 1;
const ALIGN: usize = 4096;
const NAME_LEN: usize = 48;

struct FileEntry {
    name: String,
    data: Vec<u8>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--verify") => {
            if args.len() != 2 {
                eprintln!("usage: rustos-pack --verify <image>");
                return ExitCode::FAILURE;
            }
            verify(&args[1])
        }
        _ => {
            if args.len() != 2 {
                eprintln!("usage: rustos-pack <dir> <out.img>");
                eprintln!("       rustos-pack --verify <image>");
                return ExitCode::FAILURE;
            }
            pack(&args[0], &args[1])
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rustos-pack: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Рекурсивно собирает обычные файлы каталога (путь относительно `dir`).
fn collect(dir: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let ft = fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        if ft.is_dir() {
            let rd = fs::read_dir(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            for item in rd {
                let item = item.map_err(|e| format!("read_dir {}: {e}", p.display()))?;
                stack.push(item.path());
            }
        } else if ft.is_file() {
            let rel = p
                .strip_prefix(dir)
                .map_err(|_| format!("{} не внутри {}", p.display(), dir.display()))?;
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let data = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            if name.len() + 1 > NAME_LEN {
                return Err(format!("имя слишком длинное (> {NAME_LEN}): {name}"));
            }
            out.insert(name, data);
        }
        // Ссылки и сокеты — не поддерживаем (initramfs содержит только ELF и данные).
    }
    Ok(out)
}

fn pack(dir: &str, out: &str) -> Result<(), String> {
    let dir = Path::new(dir);
    if !dir.is_dir() {
        return Err(format!("каталог не найден: {}", dir.display()));
    }
    let files = collect(dir)?;

    let mut table = Vec::new(); // (name, size, offset)
    let mut data: Vec<u8> = Vec::new();
    let header_size = 32;
    let table_size = files.len() * 64;
    let mut offset = (header_size + table_size) as u64;
    let mut staged: Vec<FileEntry> = Vec::new();
    for (name, d) in files {
        staged.push(FileEntry { name, data: d });
    }
    staged.sort_by(|a, b| a.name.cmp(&b.name));
    for f in &staged {
        // Выравниваем смещение данных по 4096 (заголовок+таблица уже учтены в `offset`).
        // `offset` — image-offset начала данных; `data` — буфер области данных
        // (image-offset `data_start + data.len()`). Добавляем `pad` нулей.
        let pad = (ALIGN - (offset as usize % ALIGN)) % ALIGN;
        data.resize(data.len() + pad, 0);
        table.push((f.name.clone(), f.data.len() as u64, offset + pad as u64));
        offset += (pad + f.data.len()) as u64;
        data.extend_from_slice(&f.data);
    }
    let total = (offset as usize).div_ceil(ALIGN) * ALIGN;
    data.resize(total.saturating_sub(header_size + table_size), 0);
    // Пересчитываем: offset выше уже включает pad; total — от начала образа.
    let data_start = header_size + table_size;
    let expected = total - data_start;
    if data.len() != expected {
        data.resize(expected, 0);
    }

    let mut img: Vec<u8> = Vec::with_capacity(total);
    // Заголовок (32 байта: 4×u32 + u64 image_size + u64 reserved2).
    img.extend_from_slice(&MAGIC.to_le_bytes());
    img.extend_from_slice(&VERSION.to_le_bytes());
    img.extend_from_slice(&(table.len() as u32).to_le_bytes());
    img.extend_from_slice(&0u32.to_le_bytes());
    img.extend_from_slice(&(total as u64).to_le_bytes());
    img.extend_from_slice(&0u64.to_le_bytes()); // reserved2 (заполняет до 32 байт)
                                                // Таблица.
    for (name, size, off) in &table {
        let mut name_buf = [0u8; NAME_LEN];
        let bytes = name.as_bytes();
        name_buf[..bytes.len()].copy_from_slice(bytes);
        img.extend_from_slice(&name_buf);
        img.extend_from_slice(&size.to_le_bytes());
        img.extend_from_slice(&off.to_le_bytes());
    }
    img.extend_from_slice(&data);
    debug_assert_eq!(img.len(), total);
    fs::write(out, &img).map_err(|e| format!("{out}: {e}"))?;
    println!(
        "rustos-pack: {} файлов, {} байт -> {out}",
        table.len(),
        img.len()
    );
    Ok(())
}

/// Проверка целостности образа (для CI и ручной отладки).
fn verify(img: &str) -> Result<(), String> {
    let bytes = fs::read(img).map_err(|e| format!("{img}: {e}"))?;
    if bytes.len() < 32 {
        return Err("образ меньше заголовка".into());
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(format!("неверная магия: {magic:#x}"));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != VERSION {
        return Err(format!("неподдерживаемая версия: {version}"));
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let declared_total = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    if declared_total != bytes.len() {
        return Err(format!(
            "image_size {declared_total} != фактический {}",
            bytes.len()
        ));
    }
    if bytes.len() % ALIGN != 0 {
        return Err("размер образа не кратен 4096".into());
    }
    let table_end = 32 + count * 64;
    if table_end > bytes.len() {
        return Err("таблица выходит за пределы образа".into());
    }
    for i in 0..count {
        let base = 32 + i * 64;
        let name_end = bytes[base..base + NAME_LEN]
            .iter()
            .position(|&b| b == 0)
            .ok_or("имя в таблице без NUL")?;
        let name = String::from_utf8_lossy(&bytes[base..base + name_end]).into_owned();
        let size = u64::from_le_bytes(bytes[base + 48..base + 56].try_into().unwrap()) as usize;
        let offset = u64::from_le_bytes(bytes[base + 56..base + 64].try_into().unwrap()) as usize;
        if offset + size > bytes.len() {
            return Err(format!("файл {name}: данные вне образа"));
        }
        if !offset.is_multiple_of(ALIGN) {
            return Err(format!("файл {name}: offset не выровнен"));
        }
    }
    println!("rustos-pack: OK ({count} файлов, {} байт)", bytes.len());
    Ok(())
}

/// Утилита: список файлов в образе.
#[allow(dead_code)]
fn list_files(_img: &str) -> Result<Vec<PathBuf>, String> {
    // Реализуется по требованию (этап 5, отладка initramfs).
    Ok(Vec::new())
}

//! `rustos-pack` — упаковка каталога в initramfs-образ RustOS (RIFS v1).
//!
//! Формат intentionally прост: фиксированный заголовок, таблица записей и
//! выровненные на страницу данные. Порядок имён лексикографический, поэтому
//! одинаковое дерево всегда даёт одинаковый образ.
//!
//! ```text
//! [0..32)       header: magic, version, count, reserved, image_size, reserved
//! [32..32+n*64) entries: name[48], size(u64), offset(u64)
//! [data]         содержимое файлов, каждое начало выровнено на 4096 байт
//! ```
//!
//! Публичная семантика RIFS описана в `docs/VFS.md`; kernel reader находится
//! в `kernel/src/fs.rs`. Запись выполняется через временный файл в том же
//! каталоге и атомарный rename, чтобы ошибка сборки не портила старый образ.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const MAGIC: u32 = 0x52_49_46_53; // "RIFS"
const VERSION: u32 = 1;
const ALIGN: usize = 4096;
const HEADER_SIZE: usize = 32;
const ENTRY_SIZE: usize = 64;
const NAME_SIZE: usize = 48;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--verify") if args.len() == 2 => verify(Path::new(&args[1])),
        Some("--verify") => Err("usage: rustos-pack --verify <image>".into()),
        _ if args.len() == 2 => pack(Path::new(&args[0]), Path::new(&args[1])),
        _ => Err(
            "usage: rustos-pack <directory> <output.img>\n       rustos-pack --verify <image>"
                .into(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustos-pack: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Собирает только обычные файлы. Symlink не разыменовывается: иначе staging
/// мог бы незаметно втянуть файл за пределами доверенного каталога.
fn collect(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    if !root.is_dir() {
        return Err(format!("каталог не найден: {}", root.display()));
    }

    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if metadata.is_dir() {
            let entries = fs::read_dir(&path)
                .map_err(|error| format!("read_dir {}: {error}", path.display()))?;
            for entry in entries {
                pending.push(
                    entry
                        .map_err(|error| format!("read_dir {}: {error}", path.display()))?
                        .path(),
                );
            }
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("{} не находится внутри {}", path.display(), root.display()))?;
        let mut name = String::new();
        for component in relative.components() {
            let component = component
                .as_os_str()
                .to_str()
                .ok_or_else(|| format!("путь не UTF-8: {}", path.display()))?;
            if !name.is_empty() {
                name.push('/');
            }
            name.push_str(component);
        }
        validate_name(name.as_bytes())?;
        let data = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if files.insert(name.clone(), data).is_some() {
            return Err(format!("повторяющийся путь: {name}"));
        }
    }
    Ok(files)
}

fn pack(root: &Path, output: &Path) -> Result<(), String> {
    let files = collect(root)?;
    let image = build_image(&files)?;
    // Writer и verifier образуют одну trust boundary: никакой файл не
    // публикуется, если собственный parser не принимает получившиеся bytes.
    verify_bytes(&image)?;
    atomic_write(output, &image)?;
    println!(
        "rustos-pack: {} файлов, {} байт -> {}",
        files.len(),
        image.len(),
        output.display()
    );
    Ok(())
}

fn build_image(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, String> {
    let count = u32::try_from(files.len()).map_err(|_| "слишком много файлов")?;
    let table_size = files
        .len()
        .checked_mul(ENTRY_SIZE)
        .ok_or("размер таблицы переполнен")?;
    let data_start = HEADER_SIZE
        .checked_add(table_size)
        .ok_or("размер таблицы переполнен")?;

    let mut table = Vec::with_capacity(files.len());
    let mut payload = Vec::new();
    let mut cursor = data_start;
    for (name, data) in files {
        validate_name(name.as_bytes())?;
        let offset = align_up(cursor, ALIGN)?;
        let padding = offset
            .checked_sub(cursor)
            .ok_or("смещение данных переполнено")?;
        payload.resize(
            payload
                .len()
                .checked_add(padding)
                .ok_or("образ слишком велик")?,
            0,
        );
        payload.extend_from_slice(data);
        cursor = offset
            .checked_add(data.len())
            .ok_or("размер образа переполнен")?;
        table.push((name, data.len(), offset));
    }

    let total = align_up(cursor, ALIGN)?;
    payload.resize(
        total
            .checked_sub(data_start)
            .ok_or("размер образа переполнен")?,
        0,
    );
    let total_u64 = u64::try_from(total).map_err(|_| "образ не помещается в u64")?;

    let mut image = Vec::with_capacity(total);
    image.extend_from_slice(&MAGIC.to_le_bytes());
    image.extend_from_slice(&VERSION.to_le_bytes());
    image.extend_from_slice(&count.to_le_bytes());
    image.extend_from_slice(&0u32.to_le_bytes());
    image.extend_from_slice(&total_u64.to_le_bytes());
    image.extend_from_slice(&0u64.to_le_bytes());
    for (name, size, offset) in table {
        let mut encoded_name = [0u8; NAME_SIZE];
        encoded_name[..name.len()].copy_from_slice(name.as_bytes());
        image.extend_from_slice(&encoded_name);
        image.extend_from_slice(
            &u64::try_from(size)
                .map_err(|_| "файл не помещается в u64")?
                .to_le_bytes(),
        );
        image.extend_from_slice(
            &u64::try_from(offset)
                .map_err(|_| "offset не помещается в u64")?
                .to_le_bytes(),
        );
    }
    image.extend_from_slice(&payload);
    if image.len() != total {
        return Err("внутренняя ошибка раскладки RIFS".into());
    }
    Ok(image)
}

fn verify(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    verify_bytes(&bytes)?;
    let count = read_u32(&bytes, 8)?;
    println!("rustos-pack: OK ({count} файлов, {} байт)", bytes.len());
    Ok(())
}

fn verify_bytes(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < HEADER_SIZE {
        return Err("образ меньше заголовка".into());
    }
    let magic = read_u32(bytes, 0)?;
    if magic != MAGIC {
        return Err(format!("неверная магия: {magic:#x}"));
    }
    let version = read_u32(bytes, 4)?;
    if version != VERSION {
        return Err(format!("неподдерживаемая версия: {version}"));
    }
    if read_u32(bytes, 12)? != 0 || read_u64(bytes, 24)? != 0 {
        return Err("reserved поля заголовка должны быть нулевыми".into());
    }
    let declared_total = usize::try_from(read_u64(bytes, 16)?)
        .map_err(|_| "image_size не помещается в адресное пространство host")?;
    if declared_total != bytes.len() {
        return Err(format!(
            "image_size {declared_total} != фактический {}",
            bytes.len()
        ));
    }
    if !bytes.len().is_multiple_of(ALIGN) {
        return Err(format!("размер образа не кратен {ALIGN}"));
    }

    let count = usize::try_from(read_u32(bytes, 8)?).map_err(|_| "count overflow")?;
    let table_size = count
        .checked_mul(ENTRY_SIZE)
        .ok_or("размер таблицы переполнен")?;
    let table_end = HEADER_SIZE
        .checked_add(table_size)
        .ok_or("размер таблицы переполнен")?;
    if table_end > bytes.len() {
        return Err("таблица выходит за пределы образа".into());
    }

    let mut previous_name: Option<&[u8]> = None;
    let mut previous_data_end = table_end;
    for index in 0..count {
        let base = HEADER_SIZE
            .checked_add(index.checked_mul(ENTRY_SIZE).ok_or("entry overflow")?)
            .ok_or("entry overflow")?;
        let encoded_name = bytes
            .get(base..base + NAME_SIZE)
            .ok_or("обрезанная запись имени")?;
        let terminator = encoded_name
            .iter()
            .position(|&byte| byte == 0)
            .ok_or("имя в таблице без NUL")?;
        let name = &encoded_name[..terminator];
        validate_name(name)?;
        if encoded_name[terminator + 1..].iter().any(|&byte| byte != 0) {
            return Err("байты после NUL в имени должны быть нулевыми".into());
        }
        if previous_name.is_some_and(|previous| previous >= name) {
            return Err("таблица имён не отсортирована или содержит дубликат".into());
        }

        let size = usize::try_from(read_u64(bytes, base + NAME_SIZE)?)
            .map_err(|_| "размер файла не помещается в host usize")?;
        let offset = usize::try_from(read_u64(bytes, base + NAME_SIZE + 8)?)
            .map_err(|_| "offset файла не помещается в host usize")?;
        let end = offset
            .checked_add(size)
            .ok_or("диапазон файла переполнен")?;
        if offset < table_end || end > bytes.len() {
            return Err("данные файла выходят за пределы data area".into());
        }
        if !offset.is_multiple_of(ALIGN) {
            return Err(format!("offset файла не выровнен на {ALIGN}"));
        }
        if offset < previous_data_end {
            return Err("диапазоны файлов пересекаются".into());
        }
        previous_name = Some(name);
        previous_data_end = end;
    }
    Ok(())
}

fn validate_name(name: &[u8]) -> Result<(), String> {
    if name.is_empty() || name.len() >= NAME_SIZE {
        return Err(format!("длина имени должна быть 1..{} байт", NAME_SIZE - 1));
    }
    let name = core::str::from_utf8(name).map_err(|_| "имя файла не UTF-8")?;
    if name.starts_with('/')
        || name.ends_with('/')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("небезопасный относительный путь: {name}"));
    }
    Ok(())
}

fn align_up(value: usize, alignment: usize) -> Result<usize, String> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded / alignment * alignment)
        .ok_or_else(|| "выравнивание переполнено".into())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes.get(offset..offset + 4).ok_or("обрезанное поле u32")?;
    Ok(u32::from_le_bytes(
        value.try_into().map_err(|_| "неверное поле u32")?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let value = bytes.get(offset..offset + 8).ok_or("обрезанное поле u64")?;
    Ok(u64::from_le_bytes(
        value.try_into().map_err(|_| "неверное поле u64")?,
    ))
}

fn atomic_write(output: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = output
        .file_name()
        .ok_or_else(|| format!("нет имени выходного файла: {}", output.display()))?
        .to_string_lossy();
    let (temporary, mut file) = create_temporary(parent, &file_name)?;

    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, output).map_err(|error| {
            format!(
                "rename {} -> {}: {error}",
                temporary.display(),
                output.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary(parent: &Path, file_name: &str) -> Result<(PathBuf, File), String> {
    for nonce in 0..100u32 {
        let path = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Err("не удалось выбрать имя временного файла".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            ("system/bin/init.rune".into(), vec![1, 2, 3]),
            ("Примеры/привет.txt".into(), "Привет".as_bytes().to_vec()),
        ])
    }

    #[test]
    fn image_is_deterministic_and_self_verifying() {
        let first = build_image(&fixture()).unwrap();
        let second = build_image(&fixture()).unwrap();
        assert_eq!(first, second);
        assert!(verify_bytes(&first).is_ok());
    }

    #[test]
    fn malformed_header_and_ranges_are_rejected() {
        let image = build_image(&fixture()).unwrap();

        let mut reserved = image.clone();
        reserved[12] = 1;
        assert!(verify_bytes(&reserved).is_err());

        let mut overlap = image.clone();
        let first_offset = read_u64(&overlap, HEADER_SIZE + NAME_SIZE + 8).unwrap();
        let second_offset_field = HEADER_SIZE + ENTRY_SIZE + NAME_SIZE + 8;
        overlap[second_offset_field..second_offset_field + 8]
            .copy_from_slice(&first_offset.to_le_bytes());
        assert!(verify_bytes(&overlap).is_err());
    }

    #[test]
    fn unsafe_or_oversized_names_are_rejected() {
        assert!(validate_name(b"../kernel").is_err());
        assert!(validate_name(b"/absolute").is_err());
        assert!(validate_name(&[b'a'; NAME_SIZE]).is_err());
        assert!(validate_name(&[0xff]).is_err());
    }
}

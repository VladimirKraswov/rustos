//! `rustos-image` — сборка загрузочного образа для QEMU.
//!
//! Результат — raw-диск с GPT и одной ESP (FAT32), на которой лежит
//! `EFI/BOOT/BOOTX64.EFI` (UEFI-загрузчик). OVMF в «fallback»-режиме
//! (без NVRAM-переменных BootOrder) ищет именно этот путь.
//!
//! ## Использование
//!
//! ```text
//! rustos-image <bootloader.efi> <out.img> [--size-mb N] [--efi-name NAME]
//! ```
//!
//! * `bootloader.efi` — собранный UEFI-загрузчик (`rustos-boot`);
//! * `out.img` — выходной raw-образ диска;
//! * `--size-mb N` — размер диска в МБ (по умолчанию 256 → FAT32);
//! * `--efi-name NAME` — имя EFI-файла в `EFI/BOOT/` (по умолчанию
//!   `BOOTX64.EFI`; для AArch64-варианта — `BOOTAA64.EFI`, имя, которое
//!   ищет EDK2/AAVMF в fallback-режиме).
//!
//! ## Воспроизводимость
//!
//! Образ детерминирован: фиксированные GUID (disk/partition) и фиксированная
//! DOS-дата 1980/1/1 (fatfs без фичи `chrono`). Один и тот же загрузчик даёт
//! байт-в-байт идентичный образ.

use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fatfs::{format_volume, FatType, FileSystem, FormatVolumeOptions, FsOptions};

const SECTOR: usize = 512;
const DEFAULT_SIZE_MB: u64 = 256;
/// Имя EFI-файла по умолчанию (x86_64 fallback-boot).
const DEFAULT_EFI_NAME: &str = "BOOTX64.EFI";
const NUM_ENTRIES: u32 = 128;
const ENTRY_SIZE: u32 = 128;
const GPT_HEADER_SIZE: usize = 92;
const ENTRY_ARRAY_BYTES: usize = NUM_ENTRIES as usize * ENTRY_SIZE as usize;
const ENTRY_ARRAY_SECTORS: u64 = ENTRY_ARRAY_BYTES.div_ceil(SECTOR) as u64;
/// Первая usable LBA (после protective MBR + GPT header + 128 записей).
const FIRST_USABLE_LBA: u64 = 34;
/// Запас с конца диска под backup GPT (записи + заголовок).
const TAIL_RESERVED_LBA: u64 = 34;

/// GUID типа «EFI System Partition» (EF00), как хранится в GPT (mixed-endian).
const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];
/// Фиксированный disk GUID (воспроизводимость).
const DISK_GUID: [u8; 16] = [
    0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36,
];
/// Фиксированный unique GUID раздела (воспроизводимость).
const PART_GUID: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rustos-image: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Разбор аргументов и режимы: сборка образа
/// (`<efi> <out> [--size-mb N] [--efi-name NAME]`)
/// или проверка (`--verify <img> [expected_efi] [--efi-name NAME]`).
fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Режим проверки: `rustos-image --verify <img> [expected_efi] [--efi-name NAME]`.
    if let Some(first) = args.first() {
        if first.as_str() == "--verify" {
            let mut positionals: Vec<&String> = Vec::new();
            let mut efi_name = DEFAULT_EFI_NAME.to_string();
            let mut i = 1;
            while i < args.len() {
                if args[i].as_str() == "--efi-name" {
                    i += 1;
                    efi_name = args
                        .get(i)
                        .ok_or("--efi-name: не указано значение")?
                        .clone();
                } else {
                    positionals.push(&args[i]);
                }
                i += 1;
            }
            if positionals.is_empty() || positionals.len() > 2 {
                return Err(
                    "usage: rustos-image --verify <img> [expected_efi] [--efi-name NAME]".into(),
                );
            }
            validate_efi_name(&efi_name)?;
            return verify_image(
                positionals.first().copied(),
                positionals.get(1).copied(),
                &efi_name,
            );
        }
    }

    // Режим сборки: `rustos-image <efi> <out.img> [--size-mb N] [--efi-name NAME]`.
    if args.len() < 2 {
        return Err(
            "usage: rustos-image <bootloader.efi> <out.img> [--size-mb N] [--efi-name NAME]\n\
                    rustos-image --verify <img> [expected_efi] [--efi-name NAME]"
                .into(),
        );
    }
    let efi_path = Path::new(&args[0]);
    let out_path = Path::new(&args[1]);
    let mut size_mb = DEFAULT_SIZE_MB;
    let mut efi_name = DEFAULT_EFI_NAME.to_string();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--size-mb" => {
                i += 1;
                size_mb = args
                    .get(i)
                    .ok_or("--size-mb: не указано значение")?
                    .parse::<u64>()
                    .map_err(|e| format!("--size-mb: не число: {e}"))?;
                if size_mb < 32 {
                    return Err("--size-mb: минимум 32 МБ".into());
                }
            }
            "--efi-name" => {
                i += 1;
                efi_name = args
                    .get(i)
                    .ok_or("--efi-name: не указано значение")?
                    .clone();
            }
            other => return Err(format!("неизвестный флаг: {other}")),
        }
        i += 1;
    }

    let efi_bytes = fs::read(efi_path).map_err(|e| format!("{}: {e}", efi_path.display()))?;
    validate_efi_name(&efi_name)?;

    let total_bytes = size_mb
        .checked_mul(1024 * 1024)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or("--size-mb: размер не помещается в адресное пространство host")?;
    let total_lbas = (total_bytes / SECTOR) as u64;

    // 1. Нулевой диск.
    let mut disk = vec![0u8; total_bytes];

    // 2. GPT: protective MBR + primary/backup заголовки + записи.
    write_gpt(&mut disk, total_lbas)?;

    // 3. Форматируем регион раздела как FAT32 и кладём EFI-файл
    //    (BOOTX64.EFI по умолчанию; BOOTAA64.EFI для AArch64).
    let part_first = FIRST_USABLE_LBA;
    let part_last = total_lbas - TAIL_RESERVED_LBA;
    let partition = lba_range(part_first, part_last, disk.len())?;
    write_esp(&mut disk[partition], &efi_bytes, &efi_name)?;

    // 4. До публикации образ обязан пройти тот же parser, что `--verify`.
    verify_disk(&mut disk, Some(&efi_bytes), &efi_name)?;
    atomic_write(out_path, &disk)?;

    println!(
        "rustos-image: OK — {} ({} МБ, {} LBA, ESP {}..{} LBA, FAT32, EFI/BOOT/{} {} Б)",
        out_path.display(),
        size_mb,
        total_lbas,
        part_first,
        part_last,
        efi_name,
        efi_bytes.len(),
    );
    Ok(())
}

/// Режим `--verify <img> [expected_efi]`: валидация GPT (signature, CRC header/entries,
/// поиск раздела ESP EF00) и read-back `EFI/BOOT/<efi_name>` с ESP (FAT).
/// Если задан `expected_efi` — байт-в-байт сравнение с эталоном.
/// Полезен для CI и дебага до QEMU-загрузки.
fn verify_image(
    img: Option<&String>,
    expected_efi: Option<&String>,
    efi_name: &str,
) -> Result<(), String> {
    let img_path =
        img.ok_or("usage: rustos-image --verify <img> [expected_efi] [--efi-name NAME]")?;
    let mut disk = fs::read(img_path).map_err(|e| format!("{}: {e}", img_path))?;
    let expected = expected_efi
        .map(|path| fs::read(path).map_err(|error| format!("{path}: {error}")))
        .transpose()?;
    let verified = verify_disk(&mut disk, expected.as_deref(), efi_name)?;
    println!(
        "verify: GPT OK — {} LBA, ESP {}..{} LBA (EF00)",
        verified.total_lbas, verified.partition_first, verified.partition_last
    );
    println!("verify: FAT OK — {efi_name} = {} Б", verified.efi_size);
    if let Some(reference) = expected_efi {
        println!("verify: {efi_name} совпадает с эталоном ({reference})");
    }
    println!("rustos-image: verify OK");
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VerifiedImage {
    total_lbas: u64,
    partition_first: u64,
    partition_last: u64,
    efi_size: usize,
}

#[derive(Clone, Copy)]
struct GptHeader {
    my_lba: u64,
    alternate_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_lba: u64,
    entries_crc: u32,
}

fn verify_disk(
    disk: &mut [u8],
    expected_efi: Option<&[u8]>,
    efi_name: &str,
) -> Result<VerifiedImage, String> {
    validate_efi_name(efi_name)?;
    if disk.len() < 4 * SECTOR || !disk.len().is_multiple_of(SECTOR) {
        return Err("GPT: размер диска мал или не кратен сектору".into());
    }
    if disk.get(510..512) != Some(&[0x55, 0xaa])
        || disk.get(450) != Some(&0xee)
        || read_u32(disk, 454)? != 1
    {
        return Err("GPT: protective MBR повреждён".into());
    }

    let total_lbas = u64::try_from(disk.len() / SECTOR).map_err(|_| "GPT: диск слишком велик")?;
    let primary = parse_gpt_header(disk, 1)?;
    let backup_lba = total_lbas.checked_sub(1).ok_or("GPT: нет backup LBA")?;
    let backup = parse_gpt_header(disk, backup_lba)?;
    let expected_backup_entries = backup_lba
        .checked_sub(ENTRY_ARRAY_SECTORS)
        .ok_or("GPT: backup entries underflow")?;
    if primary.my_lba != 1
        || primary.alternate_lba != backup_lba
        || primary.entries_lba != 2
        || backup.my_lba != backup_lba
        || backup.alternate_lba != 1
        || backup.entries_lba != expected_backup_entries
        || primary.first_usable != backup.first_usable
        || primary.last_usable != backup.last_usable
        || primary.entries_crc != backup.entries_crc
    {
        return Err("GPT: primary и backup headers не согласованы".into());
    }

    let primary_entries = partition_entries(disk, primary)?;
    let backup_entries = partition_entries(disk, backup)?;
    if primary_entries != backup_entries {
        return Err("GPT: primary и backup partition arrays различаются".into());
    }

    let mut esp = None;
    let (entries, remainder) = primary_entries.as_chunks::<{ ENTRY_SIZE as usize }>();
    if !remainder.is_empty() {
        return Err("GPT: partition array не кратен размеру записи".into());
    }
    for entry in entries {
        let kind = entry.get(..16).ok_or("GPT: обрезан GUID записи")?;
        if kind.iter().all(|&byte| byte == 0) {
            continue;
        }
        if kind != ESP_TYPE_GUID {
            return Err("GPT: образ RustOS содержит неизвестный раздел".into());
        }
        if esp.is_some() {
            return Err("GPT: образ RustOS содержит несколько ESP".into());
        }
        let first = read_u64(entry, 32)?;
        let last = read_u64(entry, 40)?;
        if first < primary.first_usable || last > primary.last_usable || first > last {
            return Err("GPT: ESP выходит за usable LBA".into());
        }
        esp = Some((first, last));
    }
    let (partition_first, partition_last) = esp.ok_or("GPT: ESP-раздел не найден")?;
    let partition = lba_range(partition_first, partition_last, disk.len())?;

    let mut efi_on_disk = Vec::new();
    {
        let volume = FileSystem::new(Cursor::new(&mut disk[partition]), FsOptions::new())
            .map_err(|error| format!("FAT: FileSystem::new: {error}"))?;
        let root = volume.root_dir();
        let efi_dir = root
            .open_dir("EFI")
            .map_err(|error| format!("FAT: каталог EFI: {error}"))?;
        let boot_dir = efi_dir
            .open_dir("BOOT")
            .map_err(|error| format!("FAT: каталог EFI/BOOT: {error}"))?;
        let mut file = boot_dir
            .open_file(efi_name)
            .map_err(|error| format!("FAT: EFI/BOOT/{efi_name}: {error}"))?;
        file.read_to_end(&mut efi_on_disk)
            .map_err(|error| format!("FAT: read {efi_name}: {error}"))?;
    }
    if expected_efi.is_some_and(|expected| expected != efi_on_disk) {
        return Err(format!("{efi_name} на образе не совпадает с эталоном"));
    }
    Ok(VerifiedImage {
        total_lbas,
        partition_first,
        partition_last,
        efi_size: efi_on_disk.len(),
    })
}

fn parse_gpt_header(disk: &[u8], lba: u64) -> Result<GptHeader, String> {
    let sector = lba_count_range(lba, 1, disk.len())?;
    let header = disk.get(sector).ok_or("GPT: header вне диска")?;
    if header.get(..8) != Some(b"EFI PART") {
        return Err(format!("GPT: signature не найдена в LBA {lba}"));
    }
    if read_u32(header, 8)? != 0x0001_0000 || read_u32(header, 20)? != 0 {
        return Err("GPT: revision/reserved заголовка не поддерживаются".into());
    }
    let header_size =
        usize::try_from(read_u32(header, 12)?).map_err(|_| "GPT: header_size overflow")?;
    if !(GPT_HEADER_SIZE..=SECTOR).contains(&header_size) {
        return Err("GPT: неверный header_size".into());
    }
    let stored_crc = read_u32(header, 16)?;
    let mut checked = header
        .get(..header_size)
        .ok_or("GPT: обрезанный заголовок")?
        .to_vec();
    checked[16..20].copy_from_slice(&0u32.to_le_bytes());
    if crc32(&checked) != stored_crc {
        return Err(format!("GPT: CRC заголовка LBA {lba} не сходится"));
    }
    if read_u32(header, 80)? != NUM_ENTRIES || read_u32(header, 84)? != ENTRY_SIZE {
        return Err("GPT: RustOS ожидает 128 записей по 128 байт".into());
    }
    let result = GptHeader {
        my_lba: read_u64(header, 24)?,
        alternate_lba: read_u64(header, 32)?,
        first_usable: read_u64(header, 40)?,
        last_usable: read_u64(header, 48)?,
        entries_lba: read_u64(header, 72)?,
        entries_crc: read_u32(header, 88)?,
    };
    if result.my_lba != lba || result.first_usable > result.last_usable {
        return Err("GPT: неверные self/usable LBA".into());
    }
    Ok(result)
}

fn partition_entries(disk: &[u8], header: GptHeader) -> Result<&[u8], String> {
    let range = lba_count_range(header.entries_lba, ENTRY_ARRAY_SECTORS, disk.len())?;
    let entries = disk
        .get(range.start..range.start + ENTRY_ARRAY_BYTES)
        .ok_or("GPT: partition array обрезан")?;
    if crc32(entries) != header.entries_crc {
        return Err("GPT: CRC partition entries не сходится".into());
    }
    Ok(entries)
}

/// Записывает GPT (protective MBR + primary/backup header + partition entries).
fn write_gpt(disk: &mut [u8], total_lbas: u64) -> Result<(), String> {
    let primary_hdr_lba: u64 = 1;
    let backup_hdr_lba: u64 = total_lbas - 1;
    let primary_entries_lba: u64 = 2;
    let backup_entries_lba: u64 = total_lbas - 1 - ENTRY_ARRAY_SECTORS;
    let first_usable: u64 = FIRST_USABLE_LBA;
    let last_usable: u64 = total_lbas - TAIL_RESERVED_LBA;
    let part_first: u64 = FIRST_USABLE_LBA;
    let part_last: u64 = total_lbas - TAIL_RESERVED_LBA;

    // Protective MBR (LBA 0).
    disk[..SECTOR].copy_from_slice(&build_protective_mbr(total_lbas));

    // Partition entries (128 × 128B): одна ESP, остальные нулевые.
    let mut entries = [0u8; (NUM_ENTRIES * ENTRY_SIZE) as usize];
    entries[..ENTRY_SIZE as usize].copy_from_slice(&build_esp_entry(part_first, part_last));
    let entries_crc = crc32(&entries);

    // Primary header (LBA 1) + entries (LBA 2..).
    let primary_hdr = build_gpt_header(
        primary_hdr_lba,
        backup_hdr_lba,
        first_usable,
        last_usable,
        primary_entries_lba,
        entries_crc,
    );
    disk[SECTOR..2 * SECTOR].copy_from_slice(&primary_hdr);
    let ent_bytes = (NUM_ENTRIES * ENTRY_SIZE) as usize;
    let ent_start = (primary_entries_lba as usize) * SECTOR;
    disk[ent_start..ent_start + ent_bytes].copy_from_slice(&entries);

    // Backup entries + header (конец диска).
    let be_start = (backup_entries_lba as usize) * SECTOR;
    disk[be_start..be_start + ent_bytes].copy_from_slice(&entries);
    let backup_hdr = build_gpt_header(
        backup_hdr_lba,
        primary_hdr_lba,
        first_usable,
        last_usable,
        backup_entries_lba,
        entries_crc,
    );
    let bh_start = (backup_hdr_lba as usize) * SECTOR;
    disk[bh_start..bh_start + SECTOR].copy_from_slice(&backup_hdr);

    Ok(())
}

/// Protective MBR: один псевдо-раздел 0xEE, покрывающий весь диск.
fn build_protective_mbr(total_lbas: u64) -> [u8; SECTOR] {
    let mut m = [0u8; SECTOR];
    // Partition entry #1 (offset 446..462).
    m[446] = 0x00; // status
    m[447] = 0x00; // CHS start: head
    m[448] = 0x02; // CHS start: sector+drive
    m[449] = 0x00; // CHS start: cyl high
    m[450] = 0xEE; // type (GPT protective)
    m[451] = 0xff; // CHS end: head
    m[452] = 0xff; // CHS end: sector+drive
    m[453] = 0xff; // CHS end: cyl high
    m[454..458].copy_from_slice(&1u32.to_le_bytes()); // LBA start = 1
    let lba_count = if total_lbas - 1 > u32::MAX as u64 {
        u32::MAX
    } else {
        (total_lbas - 1) as u32
    };
    m[458..462].copy_from_slice(&lba_count.to_le_bytes());
    m[510] = 0x55;
    m[511] = 0xaa;
    m
}

/// GPT header (92B) + CRC32, остальное сектор — нули.
fn build_gpt_header(
    my_lba: u64,
    alt_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_lba: u64,
    entries_crc: u32,
) -> [u8; SECTOR] {
    let mut h = [0u8; SECTOR];
    h[0..8].copy_from_slice(b"EFI PART");
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    h[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
                                                     // h[16..20] — header CRC (считаем в конце).
    h[20..24].copy_from_slice(&0u32.to_le_bytes()); // reserved
    h[24..32].copy_from_slice(&my_lba.to_le_bytes());
    h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
    h[40..48].copy_from_slice(&first_usable.to_le_bytes());
    h[48..56].copy_from_slice(&last_usable.to_le_bytes());
    h[56..72].copy_from_slice(&DISK_GUID);
    h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    h[80..84].copy_from_slice(&NUM_ENTRIES.to_le_bytes());
    h[84..88].copy_from_slice(&ENTRY_SIZE.to_le_bytes());
    h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let crc = crc32(&h[0..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Запись раздела: ESP (EF00) с фиксированными GUID и именем "ESP".
fn build_esp_entry(first_lba: u64, last_lba: u64) -> [u8; ENTRY_SIZE as usize] {
    let mut e = [0u8; ENTRY_SIZE as usize];
    e[0..16].copy_from_slice(&ESP_TYPE_GUID);
    e[16..32].copy_from_slice(&PART_GUID);
    e[32..40].copy_from_slice(&first_lba.to_le_bytes());
    e[40..48].copy_from_slice(&last_lba.to_le_bytes());
    e[48..56].copy_from_slice(&0u64.to_le_bytes()); // attributes
                                                    // Имя "ESP" в UTF-16LE (offset 56).
    let name: [u8; 8] = [0x45, 0x00, 0x53, 0x00, 0x50, 0x00, 0x00, 0x00];
    e[56..64].copy_from_slice(&name);
    e
}

/// Форматирует `part` как FAT32-том с `EFI/BOOT/<efi_name>` (BOOTX64.EFI
/// по умолчанию; BOOTAA64.EFI для AArch64-варианта).
fn write_esp(part: &mut [u8], efi: &[u8], efi_name: &str) -> Result<(), String> {
    let mut label = [0u8; 11];
    label[..3].copy_from_slice(b"ESP");

    // `&mut [u8]` НЕ реализует std::io::Read (только `&[u8]`), а fatfs требует
    // Read+Write+Seek (ReadWriteSeek) → оборачиваем в Cursor<&mut [u8]>.
    format_volume(
        Cursor::new(&mut part[..]),
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat32)
            .bytes_per_cluster(4096)
            .volume_label(label),
    )
    .map_err(|e| format!("format_volume: {e}"))?;

    let volume = FileSystem::new(Cursor::new(&mut part[..]), FsOptions::new())
        .map_err(|e| format!("FileSystem::new: {e}"))?;

    // Вложенная область: Dir/File заимствуют volume; unmount после выхода.
    {
        let root = volume.root_dir();
        let efi_dir = root
            .create_dir("EFI")
            .map_err(|e| format!("create_dir EFI: {e}"))?;
        let boot_dir = efi_dir
            .create_dir("BOOT")
            .map_err(|e| format!("create_dir EFI/BOOT: {e}"))?;
        let mut file = boot_dir
            .create_file(efi_name)
            .map_err(|e| format!("create_file {efi_name}: {e}"))?;
        file.write_all(efi)
            .map_err(|e| format!("write {efi_name}: {e}"))?;
        file.flush().map_err(|e| format!("flush {efi_name}: {e}"))?;
    }

    volume.unmount().map_err(|e| format!("unmount: {e}"))?;
    Ok(())
}

fn validate_efi_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 12
        || name.bytes().any(|byte| {
            !byte.is_ascii_alphanumeric() && byte != b'.' && byte != b'_' && byte != b'-'
        })
        || name.matches('.').count() > 1
    {
        return Err("--efi-name: ожидается одно безопасное ASCII 8.3 имя".into());
    }
    Ok(())
}

fn lba_range(first: u64, last: u64, disk_len: usize) -> Result<core::ops::Range<usize>, String> {
    if first > last {
        return Err("GPT: диапазон LBA инвертирован".into());
    }
    lba_count_range(
        first,
        last.checked_sub(first)
            .and_then(|count| count.checked_add(1))
            .ok_or("GPT: число LBA переполнено")?,
        disk_len,
    )
}

fn lba_count_range(
    first: u64,
    count: u64,
    disk_len: usize,
) -> Result<core::ops::Range<usize>, String> {
    let start = first
        .checked_mul(SECTOR as u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or("GPT: byte offset переполнен")?;
    let length = count
        .checked_mul(SECTOR as u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or("GPT: byte length переполнен")?;
    let end = start
        .checked_add(length)
        .ok_or("GPT: byte range переполнен")?;
    if end > disk_len {
        return Err("GPT: LBA range выходит за диск".into());
    }
    Ok(start..end)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    let end = offset.checked_add(N).ok_or("GPT: field offset overflow")?;
    bytes
        .get(offset..end)
        .ok_or("GPT: обрезанное поле")?
        .try_into()
        .map_err(|_| "GPT: внутренняя ошибка размера поля".into())
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

/// Стандартный CRC32 (IEEE 802.3, polynomial 0xEDB88320, reflected,
/// init/xorout 0xFFFFFFFF) — тот же, что используется в GPT.
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BYTES: usize = 32 * 1024 * 1024;

    fn gpt_fixture() -> Vec<u8> {
        let mut disk = vec![0; TEST_BYTES];
        write_gpt(&mut disk, (TEST_BYTES / SECTOR) as u64).unwrap();
        disk
    }

    #[test]
    fn gpt_has_standard_protective_mbr_and_matching_backup() {
        let disk = gpt_fixture();
        assert_eq!(&disk[510..512], &[0x55, 0xaa]);
        assert_eq!(disk[450], 0xee);

        let total_lbas = (disk.len() / SECTOR) as u64;
        let primary = parse_gpt_header(&disk, 1).unwrap();
        let backup = parse_gpt_header(&disk, total_lbas - 1).unwrap();
        assert_eq!(primary.alternate_lba, backup.my_lba);
        assert_eq!(backup.entries_lba, total_lbas - 1 - ENTRY_ARRAY_SECTORS);
        assert_eq!(
            partition_entries(&disk, primary).unwrap(),
            partition_entries(&disk, backup).unwrap()
        );
    }

    #[test]
    fn malformed_gpt_is_rejected_without_unchecked_slices() {
        assert!(parse_gpt_header(&[0; SECTOR], 1).is_err());
        assert!(lba_count_range(u64::MAX, 2, usize::MAX).is_err());

        let mut disk = gpt_fixture();
        disk[SECTOR + 12..SECTOR + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_gpt_header(&disk, 1).is_err());
    }

    #[test]
    fn efi_name_is_one_bounded_path_component() {
        assert!(validate_efi_name("BOOTX64.EFI").is_ok());
        assert!(validate_efi_name("BOOTAA64.EFI").is_ok());
        assert!(validate_efi_name("../BOOT.EFI").is_err());
        assert!(validate_efi_name("EFI/BOOTX64.EFI").is_err());
    }
}

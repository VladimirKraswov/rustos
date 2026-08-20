//! `rustos-image` — сборка загрузочного образа для QEMU.
//!
//! Результат — raw-диск с GPT и одной ESP (FAT32), на которой лежит
//! `EFI/BOOT/BOOTX64.EFI` (UEFI-загрузчик). OVMF в «fallback»-режиме
//! (без NVRAM-переменных BootOrder) ищет именно этот путь.
//!
//! ## Использование
//!
//! ```text
//! rustos-image <bootloader.efi> <out.img> [--size-mb N]
//! ```
//!
//! * `bootloader.efi` — собранный UEFI-загрузчик (`rustos-boot`);
//! * `out.img` — выходной raw-образ диска;
//! * `--size-mb N` — размер диска в МБ (по умолчанию 256 → FAT32).
//!
//! ## Воспроизводимость
//!
//! Образ детерминирован: фиксированные GUID (disk/partition) и фиксированная
//! DOS-дата 1980/1/1 (fatfs без фичи `chrono`). Один и тот же загрузчик даёт
//! байт-в-байт идентичный образ.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use fatfs::{format_volume, FatType, FileSystem, FormatVolumeOptions, FsOptions};

const SECTOR: usize = 512;
const DEFAULT_SIZE_MB: u64 = 256;
const NUM_ENTRIES: u32 = 128;
const ENTRY_SIZE: u32 = 128;
/// Первая utilisable LBA (после protective MBR + GPT header + 128 записей).
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

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Режим проверки: `rustos-image --verify <img> [expected_efi]`.
    if let Some(first) = args.first() {
        if first.as_str() == "--verify" {
            return verify_image(args.get(1), args.get(2));
        }
    }

    // Режим сборки: `rustos-image <bootloader.efi> <out.img> [--size-mb N]`.
    if args.len() < 2 || args.len() > 4 {
        return Err(
            "usage: rustos-image <bootloader.efi> <out.img> [--size-mb N]\n\
                   rustos-image --verify <img> [expected_efi]"
                .into(),
        );
    }
    let efi_path = Path::new(&args[0]);
    let out_path = Path::new(&args[1]);
    let mut size_mb = DEFAULT_SIZE_MB;
    if args.len() == 4 {
        if args[2] != "--size-mb" {
            return Err(format!("неизвестный флаг: {}", args[2]));
        }
        size_mb = args[3]
            .parse::<u64>()
            .map_err(|_| format!("--size-mb: не число: {}", args[3]))?;
        if size_mb < 32 {
            return Err("--size-mb: минимум 32 МБ".into());
        }
    }

    let efi_bytes = fs::read(efi_path).map_err(|e| format!("{}: {e}", efi_path.display()))?;

    let total_bytes = (size_mb * 1024 * 1024) as usize;
    let total_lbas = (total_bytes / SECTOR) as u64;

    // 1. Нулевой диск.
    let mut disk = vec![0u8; total_bytes];

    // 2. GPT: protective MBR + primary/backup заголовки + записи.
    write_gpt(&mut disk, total_lbas)?;

    // 3. Форматируем регион раздела как FAT32 и кладём BOOTX64.EFI.
    let part_first = FIRST_USABLE_LBA;
    let part_last = total_lbas - TAIL_RESERVED_LBA;
    let part_start = (part_first * SECTOR as u64) as usize;
    let part_end = ((part_last + 1) * SECTOR as u64) as usize;
    write_esp(&mut disk[part_start..part_end], &efi_bytes)?;

    // 4. Запись образа.
    fs::write(out_path, &disk).map_err(|e| format!("{}: {e}", out_path.display()))?;

    println!(
        "rustos-image: OK — {} ({} МБ, {} LBA, ESP {}..{} LBA, FAT32, {} Б EFI)",
        out_path.display(),
        size_mb,
        total_lbas,
        part_first,
        part_last,
        efi_bytes.len(),
    );
    Ok(())
}

/// Режим `--verify <img> [expected_efi]`: валидация GPT (signature, CRC header/entries,
/// поиск раздела ESP EF00) и read-back `EFI/BOOT/BOOTX64.EFI` с ESP (FAT).
/// Если задан `expected_efi` — байт-в-байт сравнение с эталоном.
/// Полезен для CI и дебага до QEMU-загрузки.
fn verify_image(img: Option<&String>, expected_efi: Option<&String>) -> Result<(), String> {
    let img_path = img.ok_or("usage: rustos-image --verify <img> [expected_efi]")?;
    let mut disk = fs::read(img_path).map_err(|e| format!("{}: {e}", img_path))?;
    let total_lbas = (disk.len() / SECTOR) as u64;

    // 1. GPT: primary header (LBA 1).
    let hdr = &disk[SECTOR..2 * SECTOR];
    if &hdr[0..8] != b"EFI PART" {
        return Err("GPT: primary header signature не найдена".into());
    }
    let header_size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    let stored_hdr_crc = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
    let mut hdr_check = hdr[..header_size].to_vec();
    hdr_check[16..20].copy_from_slice(&0u32.to_le_bytes());
    let calc_hdr_crc = crc32(&hdr_check);
    if calc_hdr_crc != stored_hdr_crc {
        return Err(format!(
            "GPT: CRC primary header не сходится (stored {stored_hdr_crc:#010x}, calc {calc_hdr_crc:#010x})"
        ));
    }
    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let num_entries = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
    let entry_size = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
    let stored_ent_crc = u32::from_le_bytes(hdr[88..92].try_into().unwrap());

    let ent_start = (entries_lba as usize) * SECTOR;
    let ent_bytes = num_entries * entry_size;
    let entries = &disk[ent_start..ent_start + ent_bytes];
    let calc_ent_crc = crc32(entries);
    if calc_ent_crc != stored_ent_crc {
        return Err(format!(
            "GPT: CRC partition entries не сходится (stored {stored_ent_crc:#010x}, calc {calc_ent_crc:#010x})"
        ));
    }

    // Ищем ESP (EF00).
    let mut esp: Option<(u64, u64)> = None;
    for i in 0..num_entries {
        let e = &entries[i * entry_size..(i + 1) * entry_size];
        if e[0..16] == ESP_TYPE_GUID {
            let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
            esp = Some((first, last));
        }
    }
    let (part_first, part_last) = esp.ok_or("GPT: ESP-раздел (EF00) не найден")?;
    println!("verify: GPT OK — {total_lbas} LBA, ESP {part_first}..{part_last} LBA (EF00)");

    // 2. FAT: read-back EFI/BOOT/BOOTX64.EFI.
    let part_start = (part_first * SECTOR as u64) as usize;
    let part_end = ((part_last + 1) * SECTOR as u64) as usize;
    let mut efi_on_disk = Vec::new();
    {
        let volume = FileSystem::new(
            Cursor::new(&mut disk[part_start..part_end]),
            FsOptions::new(),
        )
        .map_err(|e| format!("FAT: FileSystem::new: {e}"))?;
        let root = volume.root_dir();
        let efi_dir = root
            .open_dir("EFI")
            .map_err(|e| format!("FAT: каталог EFI: {e}"))?;
        let boot_dir = efi_dir
            .open_dir("BOOT")
            .map_err(|e| format!("FAT: каталог EFI/BOOT: {e}"))?;
        let mut file = boot_dir
            .open_file("BOOTX64.EFI")
            .map_err(|e| format!("FAT: EFI/BOOT/BOOTX64.EFI: {e}"))?;
        Read::read_to_end(&mut file, &mut efi_on_disk)
            .map_err(|e| format!("FAT: read BOOTX64.EFI: {e}"))?;
    }
    println!("verify: FAT OK — BOOTX64.EFI = {} Б", efi_on_disk.len());

    // 3. Опциональное байт-в-байт сравнение с эталоном.
    if let Some(ref_e) = expected_efi {
        let ref_bytes = fs::read(ref_e).map_err(|e| format!("{}: {e}", ref_e))?;
        if ref_bytes != efi_on_disk {
            return Err("BOOTX64.EFI на образе НЕ совпадает с эталоном".into());
        }
        println!("verify: BOOTX64.EFI совпадает с эталоном ({ref_e})");
    }

    println!("rustos-image: verify OK");
    Ok(())
}

/// Записывает GPT (protective MBR + primary/backup header + partition entries).
fn write_gpt(disk: &mut [u8], total_lbas: u64) -> Result<(), String> {
    let primary_hdr_lba: u64 = 1;
    let backup_hdr_lba: u64 = total_lbas - 1;
    let primary_entries_lba: u64 = 2;
    let backup_entries_lba: u64 = total_lbas - 1 - (NUM_ENTRIES as u64);
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
    m[444] = 0x55;
    m[445] = 0xaa;
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

/// Форматирует `part` как FAT32-том с `EFI/BOOT/BOOTX64.EFI`.
fn write_esp(part: &mut [u8], efi: &[u8]) -> Result<(), String> {
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
            .create_file("BOOTX64.EFI")
            .map_err(|e| format!("create_file BOOTX64.EFI: {e}"))?;
        file.write_all(efi)
            .map_err(|e| format!("write BOOTX64.EFI: {e}"))?;
        file.flush()
            .map_err(|e| format!("flush BOOTX64.EFI: {e}"))?;
    }

    volume.unmount().map_err(|e| format!("unmount: {e}"))?;
    Ok(())
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

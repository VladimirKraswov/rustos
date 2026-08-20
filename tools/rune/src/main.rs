//! Host-инструмент миграции toolchain output в RUNE.
//!
//! Rust и C компиляторы не требуется форкать: они создают привычный ELF64
//! PIE, после чего этот инструмент переносит только семантически нужные
//! regions/relocations/TLS/RELRO в компактный нативный контейнер.

use std::{env, fs, path::Path, process};

use rustos_rune_format::{
    architecture, file_flags, record_kind, region_flags, relocation_kind, sha256_with_zeroed_range,
    Container, CONTENT_HASH_OFFSET, FORMAT_VERSION, HEADER_SIZE, MAGIC, PAGE_SIZE, RELOCATION_SIZE,
    TOC_ENTRY_SIZE,
};

const ELF_HEADER_SIZE: usize = 64;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 0x3e;
const EM_AARCH64: u16 = 0xb7;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;
const PT_GNU_RELRO: u32 = 0x6474_e552;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const R_X86_64_RELATIVE: u32 = 8;
const R_AARCH64_RELATIVE: u32 = 1027;

#[derive(Clone, Copy)]
struct ElfSegment {
    kind: u32,
    flags: u32,
    offset: u64,
    virtual_address: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
}

#[derive(Clone)]
struct OutputRecord {
    kind: u16,
    architecture: u16,
    flags: u32,
    offset: u64,
    file_size: u64,
    virtual_address: u64,
    memory_size: u64,
    alignment: u64,
    name_offset: u32,
    name_length: u16,
    abi_version: u16,
    link: u32,
    payload: Vec<u8>,
}

impl OutputRecord {
    fn metadata(kind: u16, architecture: u16) -> Self {
        Self {
            kind,
            architecture,
            flags: 0,
            offset: 0,
            file_size: 0,
            virtual_address: 0,
            memory_size: 0,
            alignment: 0,
            name_offset: 0,
            name_length: 0,
            abi_version: 1,
            link: 0,
            payload: Vec::new(),
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("rune: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    match args.as_slice() {
        [_, command, input] if command == "verify" => verify(Path::new(input)),
        [_, command, input] if command == "inspect" => inspect(Path::new(input)),
        [_, input, output] => pack(Path::new(input), Path::new(output), None),
        [_, command, input, output, name] if command == "pack" => {
            pack(Path::new(input), Path::new(output), Some(name))
        }
        _ => Err(
            "usage: rustos-rune <input.elf> <output.rune> | pack <input> <output> <package> | verify|inspect <file>"
                .into(),
        ),
    }
}

fn pack(input: &Path, output: &Path, explicit_name: Option<&str>) -> Result<(), String> {
    let elf = fs::read(input).map_err(|error| format!("{}: {error}", input.display()))?;
    let name = explicit_name
        .map(str::to_owned)
        .or_else(|| {
            input
                .file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .ok_or("input has no UTF-8 file name")?;
    let bytes = convert_elf(&elf, &name)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::write(output, &bytes).map_err(|error| format!("{}: {error}", output.display()))?;
    let parsed = Container::parse(&bytes).map_err(|error| format!("self-check: {error:?}"))?;
    println!(
        "RUNE v{}: {} -> {} ({} bytes, {} records)",
        FORMAT_VERSION,
        input.display(),
        output.display(),
        bytes.len(),
        parsed.header().toc_count
    );
    Ok(())
}

fn verify(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let container = Container::parse(&bytes).map_err(|error| format!("invalid RUNE: {error:?}"))?;
    let slice_count = container
        .entries()
        .filter(|entry| entry.kind == record_kind::SLICE)
        .count();
    if slice_count == 0 {
        return Err("container has no architecture slice".into());
    }
    println!(
        "RUNE OK: {} bytes, {} records, {} slice(s)",
        bytes.len(),
        container.header().toc_count,
        slice_count
    );
    Ok(())
}

fn inspect(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let container = Container::parse(&bytes).map_err(|error| format!("invalid RUNE: {error:?}"))?;
    println!(
        "RUNE v{FORMAT_VERSION} flags=0x{:x} size={} records={}",
        container.header().flags,
        bytes.len(),
        container.header().toc_count
    );
    for (index, entry) in container.entries().enumerate() {
        println!(
            "  {index:02}: kind={} arch={} flags=0x{:x} rva=0x{:x} file=0x{:x} mem=0x{:x} name={:?}",
            entry.kind,
            entry.architecture,
            entry.flags,
            entry.virtual_address,
            entry.file_size,
            entry.memory_size,
            container.name(entry).unwrap_or("<invalid>")
        );
    }
    Ok(())
}

fn convert_elf(elf: &[u8], package_name: &str) -> Result<Vec<u8>, String> {
    if elf.len() < ELF_HEADER_SIZE
        || elf.get(..4) != Some(b"\x7fELF")
        || elf[4] != 2
        || elf[5] != 1
        || read_u16(elf, 16)? != ET_DYN
    {
        return Err("expected little-endian ELF64 ET_DYN/PIE".into());
    }
    let machine = read_u16(elf, 18)?;
    let architecture = match machine {
        EM_X86_64 => architecture::X86_64,
        EM_AARCH64 => architecture::AARCH64,
        _ => return Err(format!("unsupported ELF machine 0x{machine:x}")),
    };
    let entry = read_u64(elf, 24)?;
    let phoff = usize::try_from(read_u64(elf, 32)?).map_err(|_| "program table overflow")?;
    let phentsize = read_u16(elf, 54)? as usize;
    let phnum = read_u16(elf, 56)? as usize;
    if phentsize < 56 {
        return Err("ELF program header is too small".into());
    }
    let mut segments = Vec::new();
    for index in 0..phnum {
        let offset = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or("program table overflow")?,
            )
            .ok_or("program table overflow")?;
        let header = elf
            .get(offset..offset + 56)
            .ok_or("truncated program table")?;
        segments.push(ElfSegment {
            kind: read_u32(header, 0)?,
            flags: read_u32(header, 4)?,
            offset: read_u64(header, 8)?,
            virtual_address: read_u64(header, 16)?,
            file_size: read_u64(header, 32)?,
            memory_size: read_u64(header, 40)?,
            alignment: read_u64(header, 48)?,
        });
    }
    let load_segments: Vec<_> = segments
        .iter()
        .copied()
        .filter(|segment| segment.kind == PT_LOAD)
        .collect();
    let min_page = load_segments
        .iter()
        .map(|segment| align_down(segment.virtual_address, PAGE_SIZE))
        .min()
        .ok_or("ELF has no PT_LOAD regions")?;
    if entry < min_page {
        return Err("entry is below the first load region".into());
    }

    let mut strings = Vec::new();
    let mut records = Vec::new();
    let (package_name_offset, package_name_length) = add_string(&mut strings, package_name)?;
    let mut slice = OutputRecord::metadata(record_kind::SLICE, architecture);
    slice.flags = file_flags::APPLICATION;
    slice.virtual_address = entry - min_page;
    slice.alignment = PAGE_SIZE;
    slice.name_offset = package_name_offset;
    slice.name_length = package_name_length;
    records.push(slice);

    for (index, segment) in load_segments.iter().copied().enumerate() {
        validate_load_segment(elf, segment)?;
        let name = match (segment.flags & PF_X != 0, segment.flags & PF_W != 0) {
            (true, false) => format!("code-{index}"),
            (false, true) => format!("data-{index}"),
            _ => format!("ro-{index}"),
        };
        let (name_offset, name_length) = add_string(&mut strings, &name)?;
        let mut flags = 0;
        if segment.flags & PF_R != 0 {
            flags |= region_flags::READ;
        }
        if segment.flags & PF_W != 0 {
            flags |= region_flags::WRITE;
        }
        if segment.flags & PF_X != 0 {
            flags |= region_flags::EXECUTE | region_flags::SHAREABLE;
        }
        let start = usize::try_from(segment.offset).map_err(|_| "segment offset overflow")?;
        let end = start
            .checked_add(usize::try_from(segment.file_size).map_err(|_| "segment size overflow")?)
            .ok_or("segment size overflow")?;
        records.push(OutputRecord {
            kind: record_kind::REGION,
            architecture,
            flags,
            offset: 0,
            file_size: segment.file_size,
            virtual_address: segment.virtual_address - min_page,
            memory_size: segment.memory_size,
            alignment: segment.alignment.max(PAGE_SIZE),
            name_offset,
            name_length,
            abi_version: 1,
            link: 0,
            payload: elf[start..end].to_vec(),
        });
    }

    let relocations = extract_relocations(elf, &segments, machine, min_page)?;
    if !relocations.is_empty() {
        let (name_offset, name_length) = add_string(&mut strings, "relative-relocations")?;
        let mut record = OutputRecord::metadata(record_kind::RELOCATIONS, architecture);
        record.alignment = 8;
        record.name_offset = name_offset;
        record.name_length = name_length;
        record.file_size = relocations.len() as u64;
        record.memory_size = record.file_size;
        record.payload = relocations;
        records.push(record);
    }

    if let Some(tls) = segments
        .iter()
        .copied()
        .find(|segment| segment.kind == PT_TLS)
    {
        validate_load_segment(elf, tls)?;
        let (name_offset, name_length) = add_string(&mut strings, "tls-template")?;
        let start = tls.offset as usize;
        let end = start + tls.file_size as usize;
        records.push(OutputRecord {
            kind: record_kind::TLS,
            architecture,
            flags: region_flags::READ,
            offset: 0,
            file_size: tls.file_size,
            virtual_address: 0,
            memory_size: tls.memory_size,
            alignment: tls.alignment.max(1),
            name_offset,
            name_length,
            abi_version: 1,
            link: 0,
            payload: elf[start..end].to_vec(),
        });
    }

    if let Some(relro) = segments
        .iter()
        .copied()
        .find(|segment| segment.kind == PT_GNU_RELRO)
    {
        let (name_offset, name_length) = add_string(&mut strings, "relro")?;
        let mut record = OutputRecord::metadata(record_kind::RELRO, architecture);
        record.virtual_address = relro
            .virtual_address
            .checked_sub(min_page)
            .ok_or("RELRO lies below image")?;
        record.memory_size = relro.memory_size;
        record.alignment = PAGE_SIZE;
        record.name_offset = name_offset;
        record.name_length = name_length;
        records.push(record);
    }

    let strings_index = records.len() as u32;
    let mut string_record = OutputRecord::metadata(record_kind::STRINGS, architecture::ANY);
    string_record.alignment = 1;
    string_record.file_size = strings.len() as u64;
    string_record.memory_size = strings.len() as u64;
    string_record.payload = strings;
    records.push(string_record);

    encode(records, strings_index, package_name)
}

fn extract_relocations(
    elf: &[u8],
    segments: &[ElfSegment],
    machine: u16,
    min_page: u64,
) -> Result<Vec<u8>, String> {
    let Some(dynamic) = segments
        .iter()
        .copied()
        .find(|segment| segment.kind == PT_DYNAMIC)
    else {
        return Ok(Vec::new());
    };
    let start = usize::try_from(dynamic.offset).map_err(|_| "dynamic offset overflow")?;
    let end = start
        .checked_add(usize::try_from(dynamic.file_size).map_err(|_| "dynamic size overflow")?)
        .ok_or("dynamic size overflow")?;
    let dynamic_bytes = elf.get(start..end).ok_or("truncated PT_DYNAMIC")?;
    let mut rela_address = 0u64;
    let mut rela_size = 0u64;
    let mut rela_entry_size = RELOCATION_SIZE as u64;
    for entry in dynamic_bytes.chunks_exact(16) {
        let tag = read_i64(entry, 0)?;
        let value = read_u64(entry, 8)?;
        match tag {
            DT_NULL => break,
            DT_RELA => rela_address = value,
            DT_RELASZ => rela_size = value,
            DT_RELAENT => rela_entry_size = value,
            _ => {}
        }
    }
    if rela_size == 0 {
        return Ok(Vec::new());
    }
    if rela_entry_size != 24 || !rela_size.is_multiple_of(rela_entry_size) {
        return Err("only ELF64 RELA entries are supported".into());
    }
    let file_offset = virtual_to_file_offset(segments, rela_address)?;
    let count = rela_size / rela_entry_size;
    let mut output = Vec::with_capacity(count as usize * RELOCATION_SIZE);
    for index in 0..count {
        let offset = file_offset
            .checked_add(index * rela_entry_size)
            .ok_or("RELA offset overflow")? as usize;
        let rela = elf.get(offset..offset + 24).ok_or("truncated RELA table")?;
        let target = read_u64(rela, 0)?;
        let info = read_u64(rela, 8)?;
        let addend = read_i64(rela, 16)?;
        let expected = match machine {
            EM_X86_64 => R_X86_64_RELATIVE,
            EM_AARCH64 => R_AARCH64_RELATIVE,
            _ => unreachable!(),
        };
        if info as u32 != expected || info >> 32 != 0 {
            return Err(format!(
                "ELF relocation type {} references symbol {}; native imports must be declared explicitly",
                info as u32,
                info >> 32
            ));
        }
        let target = target
            .checked_sub(min_page)
            .ok_or("relocation target below image")?;
        let normalized_addend = i128::from(addend)
            .checked_sub(i128::from(min_page))
            .and_then(|value| i64::try_from(value).ok())
            .ok_or("relocation addend overflow")?;
        output.extend_from_slice(&target.to_le_bytes());
        output.extend_from_slice(&normalized_addend.to_le_bytes());
        output.extend_from_slice(&0u32.to_le_bytes());
        output.extend_from_slice(&relocation_kind::RELATIVE64.to_le_bytes());
        output.extend_from_slice(&0u16.to_le_bytes());
    }
    Ok(output)
}

fn encode(
    mut records: Vec<OutputRecord>,
    strings_index: u32,
    package_name: &str,
) -> Result<Vec<u8>, String> {
    let toc_offset = HEADER_SIZE as u64;
    let toc_size = records
        .len()
        .checked_mul(TOC_ENTRY_SIZE)
        .ok_or("TOC size overflow")?;
    let mut cursor = align_up((HEADER_SIZE + toc_size) as u64, PAGE_SIZE)?;
    for record in &mut records {
        if record.payload.is_empty() {
            record.offset = 0;
            continue;
        }
        let alignment = record.alignment.clamp(1, PAGE_SIZE);
        cursor = align_up(cursor, alignment)?;
        record.offset = cursor;
        cursor = cursor
            .checked_add(record.payload.len() as u64)
            .ok_or("RUNE file is too large")?;
    }
    let file_size = usize::try_from(cursor).map_err(|_| "RUNE file is too large")?;
    let mut output = vec![0u8; file_size];
    output[..8].copy_from_slice(&MAGIC);
    put_u16(&mut output, 8, FORMAT_VERSION);
    put_u16(&mut output, 10, HEADER_SIZE as u16);
    put_u32(
        &mut output,
        12,
        file_flags::APPLICATION | file_flags::REPRODUCIBLE,
    );
    put_u64(&mut output, 16, file_size as u64);
    put_u64(&mut output, 24, toc_offset);
    put_u32(&mut output, 32, records.len() as u32);
    put_u32(&mut output, 36, TOC_ENTRY_SIZE as u32);
    put_u32(&mut output, 40, strings_index);
    put_u32(&mut output, 44, u32::MAX);
    let package_digest = sha256_with_zeroed_range(package_name.as_bytes(), 0..0);
    output[48..64].copy_from_slice(&package_digest[..16]);

    for (index, record) in records.iter().enumerate() {
        let entry_offset = HEADER_SIZE + index * TOC_ENTRY_SIZE;
        put_u16(&mut output, entry_offset, record.kind);
        put_u16(&mut output, entry_offset + 2, record.architecture);
        put_u32(&mut output, entry_offset + 4, record.flags);
        put_u64(&mut output, entry_offset + 8, record.offset);
        put_u64(&mut output, entry_offset + 16, record.file_size);
        put_u64(&mut output, entry_offset + 24, record.virtual_address);
        put_u64(&mut output, entry_offset + 32, record.memory_size);
        put_u64(&mut output, entry_offset + 40, record.alignment);
        put_u32(&mut output, entry_offset + 48, record.name_offset);
        put_u16(&mut output, entry_offset + 52, record.name_length);
        put_u16(&mut output, entry_offset + 54, record.abi_version);
        put_u32(&mut output, entry_offset + 56, record.link);
        if !record.payload.is_empty() {
            let start = record.offset as usize;
            output[start..start + record.payload.len()].copy_from_slice(&record.payload);
        }
    }
    let build_digest =
        sha256_with_zeroed_range(&output, CONTENT_HASH_OFFSET..CONTENT_HASH_OFFSET + 32);
    output[64..80].copy_from_slice(&build_digest[..16]);
    let content_digest =
        sha256_with_zeroed_range(&output, CONTENT_HASH_OFFSET..CONTENT_HASH_OFFSET + 32);
    output[CONTENT_HASH_OFFSET..CONTENT_HASH_OFFSET + 32].copy_from_slice(&content_digest);
    Ok(output)
}

fn validate_load_segment(elf: &[u8], segment: ElfSegment) -> Result<(), String> {
    if segment.file_size > segment.memory_size
        || segment.flags & (PF_W | PF_X) == (PF_W | PF_X)
        || segment.alignment == 0
        || !segment.alignment.is_power_of_two()
    {
        return Err("invalid or writable+executable ELF region".into());
    }
    let end = segment
        .offset
        .checked_add(segment.file_size)
        .ok_or("segment size overflow")?;
    if end > elf.len() as u64 {
        return Err("ELF region lies outside the file".into());
    }
    Ok(())
}

fn virtual_to_file_offset(segments: &[ElfSegment], address: u64) -> Result<u64, String> {
    segments
        .iter()
        .find(|segment| {
            segment.kind == PT_LOAD
                && address >= segment.virtual_address
                && address < segment.virtual_address.saturating_add(segment.file_size)
        })
        .map(|segment| segment.offset + address - segment.virtual_address)
        .ok_or_else(|| "dynamic address is not backed by a load region".into())
}

fn add_string(table: &mut Vec<u8>, value: &str) -> Result<(u32, u16), String> {
    let offset = u32::try_from(table.len()).map_err(|_| "string table is too large")?;
    let length = u16::try_from(value.len()).map_err(|_| "RUNE name is too long")?;
    table.extend_from_slice(value.as_bytes());
    Ok((offset, length))
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Result<u64, String> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or_else(|| "alignment overflow".into())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or("truncated u16")?
            .try_into()
            .unwrap(),
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or("truncated u32")?
            .try_into()
            .unwrap(),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or("truncated u64")?
            .try_into()
            .unwrap(),
    ))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, String> {
    Ok(i64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or("truncated i64")?
            .try_into()
            .unwrap(),
    ))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

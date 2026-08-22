//! Host-инструмент миграции toolchain output в RUNE.
//!
//! Rust и C компиляторы не требуется форкать: они создают привычный ELF64
//! PIE, после чего этот инструмент переносит только семантически нужные
//! regions/relocations/TLS/RELRO в компактный нативный контейнер.

#![cfg_attr(target_os = "rustos", feature(restricted_std))]

#[cfg(target_os = "rustos")]
use rustos_crt as _;

use std::{env, fs, io::Write, path::Path, process};

use rustos_ruidl::{parse_manifest, AbiManifest, ManifestMetadata};
use rustos_rune_format::{
    architecture, artifact_kind, dependency_flags, export_flags, file_flags, icon_format,
    icon_purpose, import_flags, lifecycle, metadata_flags, metadata_key, package_id,
    parse_icon_header, parse_interface_schema_header, parse_metadata_entry, parse_resource_header,
    record_flags, record_kind, region_flags, relocation_kind, resource_encoding,
    sha256_with_zeroed_range, symbol_id, Container, CAPABILITY_REQUEST_SIZE, CONTENT_HASH_OFFSET,
    DEPENDENCY_SIZE, EXPORT_SIZE, FORMAT_VERSION, HEADER_SIZE, ICON_HEADER_SIZE, IMPORT_SIZE,
    INTERFACE_SCHEMA_HEADER_SIZE, MAGIC, MANIFEST_SIZE, METADATA_ENTRY_SIZE, PAGE_SIZE,
    RELOCATION_SIZE, RESOURCE_HEADER_SIZE, TOC_ENTRY_SIZE,
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
const DT_PLTRELSZ: i64 = 2;
const DT_JMPREL: i64 = 23;
const R_X86_64_RELATIVE: u32 = 8;
const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_GLOB_DAT: u32 = 6;
const R_X86_64_JUMP_SLOT: u32 = 7;
const R_X86_64_TPOFF64: u32 = 18;
const R_AARCH64_RELATIVE: u32 = 1027;
const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;
const R_AARCH64_TLS_TPREL64: u32 = 1030;

const SHT_DYNSYM: u32 = 11;

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

#[derive(Clone, Copy)]
struct ElfSymbol<'a> {
    name: &'a str,
    info: u8,
    section: u16,
    value: u64,
}

impl ElfSymbol<'_> {
    const fn is_defined(self) -> bool {
        self.section != 0
    }

    const fn kind(self) -> u8 {
        self.info & 0x0f
    }
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
        [_, command, input] if command == "schema" => print_schema(Path::new(input)),
        [_, input, output] => pack(Path::new(input), Path::new(output), None, None),
        [_, command, input, output, name] if command == "pack" => {
            pack(Path::new(input), Path::new(output), Some(name), None)
        }
        [_, command, input, output, manifest] if command == "pack-manifest" => {
            let manifest_path = Path::new(manifest);
            let source = fs::read_to_string(manifest_path)
                .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
            let mut manifest = parse_manifest(&source)?;
            load_manifest_assets(
                &mut manifest,
                manifest_path.parent().unwrap_or_else(|| Path::new(".")),
            )?;
            pack(Path::new(input), Path::new(output), None, Some(&manifest))
        }
        _ => Err(
            "usage: rustos-rune <input.elf> <output.rune> | pack <input> <output> <package> | pack-manifest <input> <output> <manifest.rune-abi> | verify|inspect|schema <file>"
                .into(),
        ),
    }
}

fn pack(
    input: &Path,
    output: &Path,
    explicit_name: Option<&str>,
    manifest: Option<&AbiManifest>,
) -> Result<(), String> {
    let elf = fs::read(input).map_err(|error| format!("{}: {error}", input.display()))?;
    let name = manifest
        .map(|manifest| manifest.package.clone())
        .or_else(|| explicit_name.map(str::to_owned))
        .or_else(|| {
            input
                .file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .ok_or("input has no UTF-8 file name")?;
    let bytes = convert_elf(&elf, &name, manifest)?;
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

fn load_manifest_assets(manifest: &mut AbiManifest, base: &Path) -> Result<(), String> {
    for icon in &mut manifest.icons {
        let path = base.join(&icon.path);
        icon.bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if icon.format == icon_format::RGBA8_PREMULTIPLIED {
            let expected = usize::from(icon.width)
                .checked_mul(usize::from(icon.height))
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or("icon dimensions overflow")?;
            if icon.bytes.len() != expected {
                return Err(format!(
                    "{}: RGBA8 icon has {} bytes, expected {expected}",
                    path.display(),
                    icon.bytes.len()
                ));
            }
        }
        if icon.format == icon_format::SVG_UTF8 && core::str::from_utf8(&icon.bytes).is_err() {
            return Err(format!("{}: SVG icon is not UTF-8", path.display()));
        }
    }
    for resource in &mut manifest.resources {
        let path = base.join(&resource.path);
        resource.bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    }
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
    if container.manifest().is_none() {
        return Err("container has no typed manifest".into());
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
            "  {index:02}: kind={}({}) arch={} flags=0x{:x} rva=0x{:x} file=0x{:x} mem=0x{:x} name={:?}",
            entry.kind,
            record_kind_name(entry.kind),
            entry.architecture,
            entry.flags,
            entry.virtual_address,
            entry.file_size,
            entry.memory_size,
            container.name(entry).unwrap_or("<invalid>")
        );
    }
    if let Some(manifest) = container.manifest() {
        println!(
            "  manifest: kind={} lifecycle={} runtime-abi={}..={} version={}.{}.{}",
            artifact_kind_name(manifest.artifact_kind),
            lifecycle_name(manifest.lifecycle),
            manifest.runtime_abi_minimum,
            manifest.runtime_abi_maximum,
            manifest.version_major,
            manifest.version_minor,
            manifest.version_patch
        );
    }
    for entry in container.entries() {
        let Some(payload) = container.payload(entry) else {
            continue;
        };
        match entry.kind {
            record_kind::METADATA => {
                for bytes in payload.as_chunks::<METADATA_ENTRY_SIZE>().0 {
                    let metadata = parse_metadata_entry(bytes).unwrap();
                    println!(
                        "  metadata: key={} locale={:?} value={:?}",
                        metadata_key_name(metadata.key),
                        container
                            .string(metadata.locale_offset, metadata.locale_length)
                            .unwrap(),
                        container
                            .string(metadata.value_offset, metadata.value_length)
                            .unwrap()
                    );
                }
            }
            record_kind::ICON => {
                let icon = parse_icon_header(payload).unwrap();
                println!(
                    "  icon: {}x{} scale={} format={} theme={} purpose={} bytes={}",
                    icon.width,
                    icon.height,
                    icon.scale_percent,
                    icon.format,
                    icon.theme,
                    icon.purpose,
                    icon.data_size
                );
            }
            record_kind::RESOURCE => {
                let resource = parse_resource_header(payload).unwrap();
                println!(
                    "  resource: name={:?} type={:?} bytes={}",
                    container.name(entry).unwrap(),
                    container
                        .string(resource.content_type_offset, resource.content_type_length)
                        .unwrap(),
                    resource.uncompressed_size
                );
            }
            record_kind::INTERFACE_SCHEMA => {
                let schema = parse_interface_schema_header(payload).unwrap();
                println!(
                    "  interface-schema: abi={} source-bytes={} id={:02x?}",
                    schema.abi_version, schema.source_size, schema.interface.0
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn print_schema(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let container = Container::parse(&bytes).map_err(|error| format!("invalid RUNE: {error:?}"))?;
    let mut found = None;
    for entry in container
        .entries()
        .filter(|entry| entry.kind == record_kind::INTERFACE_SCHEMA)
    {
        if found.is_some() {
            return Err("container has several interface schemas; select by InterfaceId".into());
        }
        let payload = container.payload(entry).ok_or("invalid interface schema")?;
        parse_interface_schema_header(payload).ok_or("invalid interface schema")?;
        found = Some(&payload[INTERFACE_SCHEMA_HEADER_SIZE..]);
    }
    let source = found.ok_or("container has no interface schema")?;
    std::io::stdout()
        .write_all(source)
        .map_err(|error| format!("stdout: {error}"))?;
    Ok(())
}

fn record_kind_name(kind: u16) -> &'static str {
    match kind {
        record_kind::SLICE => "slice",
        record_kind::REGION => "region",
        record_kind::RELOCATIONS => "relocations",
        record_kind::IMPORTS => "imports",
        record_kind::EXPORTS => "exports",
        record_kind::DEPENDENCIES => "dependencies",
        record_kind::TLS => "tls",
        record_kind::RELRO => "relro",
        record_kind::CAPABILITIES => "capabilities",
        record_kind::STRINGS => "strings",
        record_kind::DEBUG => "debug",
        record_kind::SIGNATURE => "signature",
        record_kind::MANIFEST => "manifest",
        record_kind::METADATA => "metadata",
        record_kind::ICON => "icon",
        record_kind::RESOURCE => "resource",
        record_kind::INTERFACE_SCHEMA => "interface-schema",
        record_kind::SDK_BINDINGS => "sdk-bindings",
        _ => "unknown",
    }
}

fn artifact_kind_name(kind: u16) -> &'static str {
    match kind {
        artifact_kind::APPLICATION => "application",
        artifact_kind::LIBRARY => "library",
        artifact_kind::SERVICE => "service",
        artifact_kind::DRIVER => "driver",
        _ => "unknown",
    }
}

fn lifecycle_name(value: u16) -> &'static str {
    match value {
        lifecycle::MULTI_INSTANCE => "multi-instance",
        lifecycle::SINGLE_INSTANCE => "single-instance",
        lifecycle::MANAGED_SERVICE => "managed-service",
        lifecycle::MANAGED_DRIVER => "managed-driver",
        lifecycle::IN_PROCESS_LIBRARY => "in-process-library",
        _ => "unknown",
    }
}

fn metadata_key_name(key: u16) -> &'static str {
    match key {
        metadata_key::DISPLAY_NAME => "display-name",
        metadata_key::SUMMARY => "summary",
        metadata_key::VENDOR => "vendor",
        metadata_key::CATEGORY => "category",
        metadata_key::HOMEPAGE => "homepage",
        metadata_key::CUSTOM => "custom",
        _ => "unknown",
    }
}

fn convert_elf(
    elf: &[u8],
    package_name: &str,
    manifest: Option<&AbiManifest>,
) -> Result<Vec<u8>, String> {
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
    let artifact_flags = manifest
        .map(|manifest| manifest.kind.file_flags())
        .unwrap_or(file_flags::APPLICATION);
    let symbols = dynamic_symbols(elf)?;
    validate_manifest_symbols(manifest, &symbols)?;

    let mut slice = OutputRecord::metadata(record_kind::SLICE, architecture);
    slice.flags = artifact_flags;
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

    let relocations = extract_relocations(elf, &segments, machine, min_page, &symbols, manifest)?;
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

    if let Some(manifest) = manifest {
        append_abi_records(
            &mut records,
            &mut strings,
            manifest,
            &symbols,
            min_page,
            architecture,
        )?;
    }

    let manifest_index = append_package_records(
        &mut records,
        &mut strings,
        package_name,
        manifest,
        artifact_flags,
    )?;

    let strings_index = records.len() as u32;
    let mut string_record = OutputRecord::metadata(record_kind::STRINGS, architecture::ANY);
    string_record.alignment = 1;
    string_record.file_size = strings.len() as u64;
    string_record.memory_size = strings.len() as u64;
    string_record.payload = strings;
    records.push(string_record);

    encode(
        records,
        strings_index,
        manifest_index,
        package_name,
        artifact_flags,
    )
}

fn append_package_records(
    records: &mut Vec<OutputRecord>,
    strings: &mut Vec<u8>,
    package_name: &str,
    manifest: Option<&AbiManifest>,
    artifact_flags: u32,
) -> Result<u32, String> {
    if let Some(manifest) = manifest.filter(|manifest| manifest.interface.is_some()) {
        let interface = manifest.interface.unwrap();
        let source_size = u32::try_from(manifest.schema_source.len())
            .map_err(|_| "interface schema is too large")?;
        let mut payload = vec![0u8; INTERFACE_SCHEMA_HEADER_SIZE];
        payload[..16].copy_from_slice(&interface.0);
        put_u16(&mut payload, 16, 1);
        put_u16(&mut payload, 18, manifest.abi_version);
        put_u32(&mut payload, 20, source_size);
        payload.extend_from_slice(&manifest.schema_source);
        let (name_offset, name_length) = add_string(strings, "interface-schema")?;
        let mut record = OutputRecord::metadata(record_kind::INTERFACE_SCHEMA, architecture::ANY);
        record.alignment = 8;
        record.file_size = payload.len() as u64;
        record.memory_size = record.file_size;
        record.name_offset = name_offset;
        record.name_length = name_length;
        record.payload = payload;
        records.push(record);
    }

    if let Some(manifest) = manifest.filter(|manifest| !manifest.capabilities.is_empty()) {
        let mut payload = Vec::with_capacity(manifest.capabilities.len() * CAPABILITY_REQUEST_SIZE);
        for capability in &manifest.capabilities {
            payload.extend_from_slice(&capability.service.0);
            payload.extend_from_slice(&capability.rights.to_le_bytes());
            payload.extend_from_slice(&capability.abi_version.to_le_bytes());
            payload.extend_from_slice(&capability.slot_hint.to_le_bytes());
            payload.extend_from_slice(&capability.flags.to_le_bytes());
        }
        let (name_offset, name_length) = add_string(strings, "capability-requests")?;
        let mut record = OutputRecord::metadata(record_kind::CAPABILITIES, architecture::ANY);
        record.alignment = 8;
        record.file_size = payload.len() as u64;
        record.memory_size = record.file_size;
        record.name_offset = name_offset;
        record.name_length = name_length;
        record.payload = payload;
        records.push(record);
    }

    let mut metadata = manifest
        .map(|manifest| manifest.metadata.clone())
        .unwrap_or_default();
    if !metadata
        .iter()
        .any(|entry| entry.key == metadata_key::DISPLAY_NAME && entry.locale.is_empty())
    {
        metadata.push(ManifestMetadata {
            key: metadata_key::DISPLAY_NAME,
            name: String::new(),
            locale: String::new(),
            value: package_name.to_owned(),
        });
    }

    let metadata_index = if metadata.is_empty() {
        u32::MAX
    } else {
        let mut payload = Vec::with_capacity(metadata.len() * METADATA_ENTRY_SIZE);
        for entry in &metadata {
            let (locale_offset, locale_length) = add_string(strings, &entry.locale)?;
            let (name_offset, name_length) = add_string(strings, &entry.name)?;
            let (value_offset, value_length) = add_string(strings, &entry.value)?;
            payload.extend_from_slice(&entry.key.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
            let flags = if entry.locale.is_empty() {
                0
            } else {
                metadata_flags::LOCALIZED
            };
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.extend_from_slice(&locale_offset.to_le_bytes());
            payload.extend_from_slice(&locale_length.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
            payload.extend_from_slice(&name_offset.to_le_bytes());
            payload.extend_from_slice(&name_length.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
            payload.extend_from_slice(&value_offset.to_le_bytes());
            payload.extend_from_slice(&value_length.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
        }
        let index = records.len() as u32;
        let mut record = OutputRecord::metadata(record_kind::METADATA, architecture::ANY);
        let (name_offset, name_length) = add_string(strings, "package-metadata")?;
        record.alignment = 4;
        record.file_size = payload.len() as u64;
        record.memory_size = record.file_size;
        record.name_offset = name_offset;
        record.name_length = name_length;
        record.payload = payload;
        records.push(record);
        index
    };

    let mut default_icon_index = u32::MAX;
    if let Some(manifest) = manifest {
        for icon in &manifest.icons {
            let index = records.len() as u32;
            if default_icon_index == u32::MAX && icon.purpose == icon_purpose::APPLICATION {
                default_icon_index = index;
            }
            let mut payload = vec![0u8; ICON_HEADER_SIZE];
            put_u16(&mut payload, 0, icon.width);
            put_u16(&mut payload, 2, icon.height);
            put_u16(&mut payload, 4, icon.scale_percent);
            put_u16(&mut payload, 6, icon.format);
            put_u16(&mut payload, 8, icon.theme);
            put_u16(&mut payload, 10, icon.purpose);
            put_u64(&mut payload, 16, icon.bytes.len() as u64);
            payload.extend_from_slice(&icon.bytes);
            let label = format!("icon-{}x{}-{}", icon.width, icon.height, icon.scale_percent);
            let (name_offset, name_length) = add_string(strings, &label)?;
            let mut record = OutputRecord::metadata(record_kind::ICON, architecture::ANY);
            record.alignment = 8;
            record.file_size = payload.len() as u64;
            record.memory_size = record.file_size;
            record.name_offset = name_offset;
            record.name_length = name_length;
            record.payload = payload;
            records.push(record);
        }
        for resource in &manifest.resources {
            let (name_offset, name_length) = add_string(strings, &resource.logical_name)?;
            let (content_type_offset, content_type_length) =
                add_string(strings, &resource.content_type)?;
            let mut payload = vec![0u8; RESOURCE_HEADER_SIZE];
            put_u32(&mut payload, 0, content_type_offset);
            put_u16(&mut payload, 4, content_type_length);
            put_u16(&mut payload, 6, resource_encoding::RAW);
            put_u64(&mut payload, 8, resource.bytes.len() as u64);
            put_u64(&mut payload, 16, resource.bytes.len() as u64);
            payload.extend_from_slice(&resource.bytes);
            let mut record = OutputRecord::metadata(record_kind::RESOURCE, architecture::ANY);
            record.alignment = 8;
            record.file_size = payload.len() as u64;
            record.memory_size = record.file_size;
            record.name_offset = name_offset;
            record.name_length = name_length;
            record.payload = payload;
            records.push(record);
        }
    }

    let (kind, runtime_minimum, runtime_maximum, lifecycle, flags, version) = manifest
        .map(|manifest| {
            (
                manifest.kind.wire_kind(),
                manifest.runtime_abi_minimum,
                manifest.runtime_abi_maximum,
                manifest
                    .lifecycle
                    .unwrap_or_else(|| manifest.kind.default_lifecycle()),
                manifest.flags,
                manifest.version,
            )
        })
        .unwrap_or((
            artifact_kind_from_flags(artifact_flags),
            1,
            1,
            lifecycle::MULTI_INSTANCE,
            0,
            (0, 1, 0),
        ));
    let mut payload = vec![0u8; MANIFEST_SIZE];
    put_u16(&mut payload, 0, 1);
    put_u16(&mut payload, 2, kind);
    put_u32(&mut payload, 4, flags);
    put_u16(&mut payload, 8, runtime_minimum);
    put_u16(&mut payload, 10, runtime_maximum);
    put_u16(&mut payload, 12, lifecycle);
    put_u32(&mut payload, 16, version.0);
    put_u32(&mut payload, 20, version.1);
    put_u32(&mut payload, 24, version.2);
    put_u32(&mut payload, 28, metadata_index);
    put_u32(&mut payload, 32, default_icon_index);
    let index = records.len() as u32;
    let (name_offset, name_length) = add_string(strings, "manifest")?;
    let mut record = OutputRecord::metadata(record_kind::MANIFEST, architecture::ANY);
    record.flags = record_flags::REQUIRED;
    record.alignment = 8;
    record.file_size = MANIFEST_SIZE as u64;
    record.memory_size = MANIFEST_SIZE as u64;
    record.name_offset = name_offset;
    record.name_length = name_length;
    record.payload = payload;
    records.push(record);
    Ok(index)
}

fn artifact_kind_from_flags(flags: u32) -> u16 {
    if flags & file_flags::LIBRARY != 0 {
        artifact_kind::LIBRARY
    } else if flags & file_flags::SERVICE != 0 {
        artifact_kind::SERVICE
    } else if flags & file_flags::DRIVER != 0 {
        artifact_kind::DRIVER
    } else {
        artifact_kind::APPLICATION
    }
}

fn dynamic_symbols(elf: &[u8]) -> Result<Vec<ElfSymbol<'_>>, String> {
    let section_offset =
        usize::try_from(read_u64(elf, 40)?).map_err(|_| "section table offset overflow")?;
    let section_entry_size = read_u16(elf, 58)? as usize;
    let section_count = read_u16(elf, 60)? as usize;
    if section_entry_size < 64 || section_count == 0 {
        return Err("ELF has no usable section table; keep .dynsym while packing RUNE".into());
    }
    let mut dynamic_section = None;
    for index in 0..section_count {
        let offset = section_offset
            .checked_add(
                index
                    .checked_mul(section_entry_size)
                    .ok_or("section overflow")?,
            )
            .ok_or("section overflow")?;
        let section = elf
            .get(offset..offset + 64)
            .ok_or("truncated section table")?;
        if read_u32(section, 4)? == SHT_DYNSYM {
            if dynamic_section.is_some() {
                return Err("ELF contains multiple dynamic symbol tables".into());
            }
            dynamic_section = Some((
                read_u64(section, 24)?,
                read_u64(section, 32)?,
                read_u32(section, 40)? as usize,
                read_u64(section, 56)?,
            ));
        }
    }
    let (symbol_offset, symbol_size, strings_index, symbol_entry_size) =
        dynamic_section.ok_or("ELF has no .dynsym")?;
    if symbol_entry_size != 24 || !symbol_size.is_multiple_of(symbol_entry_size) {
        return Err("invalid .dynsym entry size".into());
    }
    let string_header_offset = section_offset
        .checked_add(
            strings_index
                .checked_mul(section_entry_size)
                .ok_or("section overflow")?,
        )
        .ok_or("section overflow")?;
    let string_header = elf
        .get(string_header_offset..string_header_offset + 64)
        .ok_or("invalid .dynsym string-table link")?;
    let strings_offset = usize::try_from(read_u64(string_header, 24)?)
        .map_err(|_| "string table offset overflow")?;
    let strings_size =
        usize::try_from(read_u64(string_header, 32)?).map_err(|_| "string table size overflow")?;
    let strings = elf
        .get(
            strings_offset
                ..strings_offset
                    .checked_add(strings_size)
                    .ok_or("string overflow")?,
        )
        .ok_or("truncated dynamic string table")?;
    let symbol_offset = usize::try_from(symbol_offset).map_err(|_| "symbol offset overflow")?;
    let symbol_count =
        usize::try_from(symbol_size / symbol_entry_size).map_err(|_| "too many dynamic symbols")?;
    let mut result = Vec::with_capacity(symbol_count);
    for index in 0..symbol_count {
        let offset = symbol_offset
            .checked_add(index.checked_mul(24).ok_or("symbol overflow")?)
            .ok_or("symbol overflow")?;
        let symbol = elf.get(offset..offset + 24).ok_or("truncated .dynsym")?;
        let name_offset = read_u32(symbol, 0)? as usize;
        let tail = strings
            .get(name_offset..)
            .ok_or("invalid dynamic symbol name")?;
        let length = tail
            .iter()
            .position(|byte| *byte == 0)
            .ok_or("unterminated symbol name")?;
        let name = core::str::from_utf8(&tail[..length]).map_err(|_| "non-UTF-8 dynamic symbol")?;
        result.push(ElfSymbol {
            name,
            info: symbol[4],
            section: read_u16(symbol, 6)?,
            value: read_u64(symbol, 8)?,
        });
    }
    Ok(result)
}

fn validate_manifest_symbols(
    manifest: Option<&AbiManifest>,
    symbols: &[ElfSymbol<'_>],
) -> Result<(), String> {
    let undefined: Vec<_> = symbols
        .iter()
        .copied()
        .filter(|symbol| !symbol.is_defined() && !symbol.name.is_empty())
        .collect();
    if manifest.is_none() && !undefined.is_empty() {
        return Err(format!(
            "ELF imports `{}`; use `pack-manifest` and declare its stable interface ABI",
            undefined[0].name
        ));
    }
    let Some(manifest) = manifest else {
        return Ok(());
    };
    for import in &manifest.imports {
        if !undefined
            .iter()
            .any(|symbol| symbol.name == import.elf_name)
        {
            return Err(format!(
                "declared import `{}` is not undefined in ELF",
                import.elf_name
            ));
        }
    }
    for symbol in undefined {
        if !manifest
            .imports
            .iter()
            .any(|import| import.elf_name == symbol.name)
        {
            return Err(format!(
                "ELF import `{}` is absent from RUNE ABI manifest",
                symbol.name
            ));
        }
    }
    for export in &manifest.exports {
        let Some(symbol) = symbols
            .iter()
            .copied()
            .find(|symbol| symbol.is_defined() && symbol.name == export.elf_name)
        else {
            return Err(format!(
                "declared export `{}` is absent from ELF",
                export.elf_name
            ));
        };
        let expected_kind = if export.flags == import_flags::FUNCTION {
            2
        } else if export.flags == import_flags::TLS {
            6
        } else {
            1
        };
        if symbol.kind() != expected_kind {
            return Err(format!(
                "export `{}` has a different ELF symbol kind",
                export.elf_name
            ));
        }
    }
    Ok(())
}

fn append_abi_records(
    records: &mut Vec<OutputRecord>,
    strings: &mut Vec<u8>,
    manifest: &AbiManifest,
    symbols: &[ElfSymbol<'_>],
    min_page: u64,
    architecture: u16,
) -> Result<(), String> {
    if !manifest.imports.is_empty() {
        let mut payload = Vec::with_capacity(manifest.imports.len() * IMPORT_SIZE);
        for import in &manifest.imports {
            let (name_offset, name_length) = add_string(strings, &import.elf_name)?;
            payload.extend_from_slice(&import.interface.0);
            payload.extend_from_slice(&symbol_id(import.interface, &import.signature).0);
            payload.extend_from_slice(&import.minimum_abi.to_le_bytes());
            payload.extend_from_slice(&import.maximum_abi.to_le_bytes());
            payload.extend_from_slice(&import.flags.to_le_bytes());
            payload.extend_from_slice(&name_offset.to_le_bytes());
            payload.extend_from_slice(&name_length.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
        }
        let mut record = OutputRecord::metadata(record_kind::IMPORTS, architecture);
        record.alignment = 8;
        record.file_size = payload.len() as u64;
        record.memory_size = record.file_size;
        record.payload = payload;
        records.push(record);
    }
    if !manifest.exports.is_empty() {
        let interface = manifest.interface.ok_or("exports require an interface")?;
        let mut payload = Vec::with_capacity(manifest.exports.len() * EXPORT_SIZE);
        for export in &manifest.exports {
            let symbol = symbols
                .iter()
                .copied()
                .find(|symbol| symbol.is_defined() && symbol.name == export.elf_name)
                .ok_or_else(|| format!("missing export `{}`", export.elf_name))?;
            let (name_offset, name_length) = add_string(strings, &export.elf_name)?;
            payload.extend_from_slice(&interface.0);
            payload.extend_from_slice(&symbol_id(interface, &export.signature).0);
            payload.extend_from_slice(
                &symbol
                    .value
                    .checked_sub(min_page)
                    .ok_or("export lies below image")?
                    .to_le_bytes(),
            );
            payload.extend_from_slice(&manifest.abi_version.to_le_bytes());
            let flags = match export.flags {
                import_flags::FUNCTION => export_flags::FUNCTION,
                import_flags::DATA => export_flags::DATA,
                import_flags::TLS => export_flags::TLS,
                _ => return Err("invalid export flags".into()),
            };
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.extend_from_slice(&name_offset.to_le_bytes());
            payload.extend_from_slice(&name_length.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
        }
        let mut record = OutputRecord::metadata(record_kind::EXPORTS, architecture);
        record.alignment = 8;
        record.file_size = payload.len() as u64;
        record.memory_size = record.file_size;
        record.payload = payload;
        records.push(record);
    }
    if !manifest.dependencies.is_empty() {
        let mut payload = Vec::with_capacity(manifest.dependencies.len() * DEPENDENCY_SIZE);
        for dependency in &manifest.dependencies {
            let (name_offset, name_length) = add_string(strings, &dependency.file_name)?;
            payload.extend_from_slice(&dependency.interface.0);
            payload.extend_from_slice(&dependency.package.unwrap_or([0; 16]));
            payload.extend_from_slice(&dependency.minimum_abi.to_le_bytes());
            payload.extend_from_slice(&dependency.maximum_abi.to_le_bytes());
            payload.extend_from_slice(
                &(dependency_flags::REQUIRED | dependency_flags::SHARE_CODE).to_le_bytes(),
            );
            payload.extend_from_slice(&name_offset.to_le_bytes());
            payload.extend_from_slice(&name_length.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
        }
        let mut record = OutputRecord::metadata(record_kind::DEPENDENCIES, architecture);
        record.alignment = 8;
        record.file_size = payload.len() as u64;
        record.memory_size = record.file_size;
        record.payload = payload;
        records.push(record);
    }
    Ok(())
}

fn extract_relocations(
    elf: &[u8],
    segments: &[ElfSegment],
    machine: u16,
    min_page: u64,
    symbols: &[ElfSymbol<'_>],
    manifest: Option<&AbiManifest>,
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
    let mut plt_rela_address = 0u64;
    let mut plt_rela_size = 0u64;
    for entry in dynamic_entries(dynamic_bytes)? {
        let tag = read_i64(entry, 0)?;
        let value = read_u64(entry, 8)?;
        match tag {
            DT_NULL => break,
            DT_RELA => rela_address = value,
            DT_RELASZ => rela_size = value,
            DT_RELAENT => rela_entry_size = value,
            DT_JMPREL => plt_rela_address = value,
            DT_PLTRELSZ => plt_rela_size = value,
            _ => {}
        }
    }
    if rela_size == 0 && plt_rela_size == 0 {
        return Ok(Vec::new());
    }
    if rela_entry_size != 24
        || !rela_size.is_multiple_of(rela_entry_size)
        || !plt_rela_size.is_multiple_of(rela_entry_size)
    {
        return Err("only ELF64 RELA entries are supported".into());
    }
    let count = rela_size
        .checked_add(plt_rela_size)
        .ok_or("combined RELA size overflow")?
        / rela_entry_size;
    let capacity = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(RELOCATION_SIZE))
        .ok_or("RELA output is too large")?;
    let mut output = Vec::with_capacity(capacity);
    for (address, size) in [(rela_address, rela_size), (plt_rela_address, plt_rela_size)] {
        if size == 0 {
            continue;
        }
        let file_offset = virtual_to_file_offset(segments, address)?;
        for index in 0..size / rela_entry_size {
            let offset = usize::try_from(
                file_offset
                    .checked_add(index * rela_entry_size)
                    .ok_or("RELA offset overflow")?,
            )
            .map_err(|_| "RELA offset does not fit host usize")?;
            let end = offset.checked_add(24).ok_or("RELA range overflow")?;
            let rela = elf.get(offset..end).ok_or("truncated RELA table")?;
            let target = read_u64(rela, 0)?
                .checked_sub(min_page)
                .ok_or("relocation target below image")?;
            let info = read_u64(rela, 8)?;
            let elf_kind = info as u32;
            let elf_symbol = (info >> 32) as u32;
            let addend = read_i64(rela, 16)?;
            let (normalized_addend, rune_symbol, rune_kind) = normalize_relocation(
                machine, elf_kind, elf_symbol, addend, min_page, segments, symbols, manifest,
            )?;
            output.extend_from_slice(&target.to_le_bytes());
            output.extend_from_slice(&normalized_addend.to_le_bytes());
            output.extend_from_slice(&rune_symbol.to_le_bytes());
            output.extend_from_slice(&rune_kind.to_le_bytes());
            output.extend_from_slice(&0u16.to_le_bytes());
        }
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn normalize_relocation(
    machine: u16,
    elf_kind: u32,
    elf_symbol: u32,
    addend: i64,
    min_page: u64,
    segments: &[ElfSegment],
    symbols: &[ElfSymbol<'_>],
    manifest: Option<&AbiManifest>,
) -> Result<(i64, u32, u16), String> {
    let relative = matches!(
        (machine, elf_kind),
        (EM_X86_64, R_X86_64_RELATIVE) | (EM_AARCH64, R_AARCH64_RELATIVE)
    );
    if relative {
        if elf_symbol != 0 {
            return Err("RELATIVE relocation references a symbol".into());
        }
        let addend = i128::from(addend)
            .checked_sub(i128::from(min_page))
            .and_then(|value| i64::try_from(value).ok())
            .ok_or("relocation addend overflow")?;
        return Ok((addend, 0, relocation_kind::RELATIVE64));
    }

    let tls = matches!(
        (machine, elf_kind),
        (EM_X86_64, R_X86_64_TPOFF64) | (EM_AARCH64, R_AARCH64_TLS_TPREL64)
    );
    // lld кодирует local-exec TLS как STN_UNDEF + addend. Это не импорт:
    // addend уже является смещением внутри PT_TLS текущего module.
    if tls && elf_symbol == 0 {
        return Ok((addend, 0, relocation_kind::TLS_TPOFF64));
    }

    let symbol = symbols
        .get(elf_symbol as usize)
        .copied()
        .ok_or("relocation references invalid dynamic symbol")?;
    let is_absolute = matches!(
        (machine, elf_kind),
        (
            EM_X86_64,
            R_X86_64_64 | R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT
        ) | (
            EM_AARCH64,
            R_AARCH64_ABS64 | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT
        )
    );
    if is_absolute {
        if symbol.is_defined() {
            let addend = i128::from(symbol.value)
                .checked_sub(i128::from(min_page))
                .and_then(|value| value.checked_add(i128::from(addend)))
                .and_then(|value| i64::try_from(value).ok())
                .ok_or("local symbol relocation overflow")?;
            return Ok((addend, 0, relocation_kind::RELATIVE64));
        }
        let import = manifest
            .and_then(|manifest| {
                manifest
                    .imports
                    .iter()
                    .position(|import| import.elf_name == symbol.name)
            })
            .ok_or_else(|| format!("relocation import `{}` is not declared", symbol.name))?;
        return Ok((addend, import as u32, relocation_kind::IMPORT64));
    }
    if machine == EM_X86_64 && elf_kind == R_X86_64_PC32 && !symbol.is_defined() {
        let import = manifest
            .and_then(|manifest| {
                manifest
                    .imports
                    .iter()
                    .position(|import| import.elf_name == symbol.name)
            })
            .ok_or_else(|| format!("PC32 import `{}` is not declared", symbol.name))?;
        return Ok((addend, import as u32, relocation_kind::IMPORT_PC32));
    }
    if tls && symbol.is_defined() {
        let template = segments
            .iter()
            .find(|segment| segment.kind == PT_TLS)
            .ok_or("TLS relocation without PT_TLS")?;
        let addend = i128::from(symbol.value)
            .checked_sub(i128::from(template.virtual_address))
            .and_then(|value| value.checked_add(i128::from(addend)))
            .and_then(|value| i64::try_from(value).ok())
            .ok_or("TLS relocation overflow")?;
        return Ok((addend, 0, relocation_kind::TLS_TPOFF64));
    }
    Err(format!(
        "unsupported ELF relocation type {elf_kind} for symbol `{}`",
        symbol.name
    ))
}

fn encode(
    mut records: Vec<OutputRecord>,
    strings_index: u32,
    manifest_index: u32,
    package_name: &str,
    artifact_flags: u32,
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
    put_u32(&mut output, 12, artifact_flags | file_flags::REPRODUCIBLE);
    put_u64(&mut output, 16, file_size as u64);
    put_u64(&mut output, 24, toc_offset);
    put_u32(&mut output, 32, records.len() as u32);
    put_u32(&mut output, 36, TOC_ENTRY_SIZE as u32);
    put_u32(&mut output, 40, strings_index);
    put_u32(&mut output, 44, manifest_index);
    output[48..64].copy_from_slice(&package_id(package_name));

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
    let segment = segments
        .iter()
        .find(|segment| {
            segment.kind == PT_LOAD
                && address >= segment.virtual_address
                && address < segment.virtual_address.saturating_add(segment.file_size)
        })
        .ok_or_else(|| "dynamic address is not backed by a load region".to_string())?;
    segment
        .offset
        .checked_add(address - segment.virtual_address)
        .ok_or_else(|| "dynamic file offset overflow".into())
}

fn dynamic_entries(bytes: &[u8]) -> Result<&[[u8; 16]], String> {
    let (entries, remainder) = bytes.as_chunks::<16>();
    if !remainder.is_empty() {
        return Err("PT_DYNAMIC size is not a multiple of Elf64_Dyn".into());
    }
    Ok(entries)
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
    Ok(u16::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, String> {
    Ok(i64::from_le_bytes(read_array(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    let end = offset.checked_add(N).ok_or("ELF field offset overflow")?;
    bytes
        .get(offset..end)
        .ok_or_else(|| format!("truncated {N}-byte ELF field"))?
        .try_into()
        .map_err(|_| "internal ELF field length mismatch".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_table_requires_complete_elf64_records() {
        assert_eq!(dynamic_entries(&[0; 32]).unwrap().len(), 2);
        assert!(dynamic_entries(&[0; 17]).is_err());
    }

    #[test]
    fn integer_reader_rejects_truncation_and_offset_overflow() {
        assert_eq!(read_u64(&8u64.to_le_bytes(), 0).unwrap(), 8);
        assert!(read_u64(&[0; 7], 0).is_err());
        assert!(read_u32(&[0; 8], usize::MAX).is_err());
    }

    #[test]
    fn manifest_supports_utf8_metadata_and_packaged_assets() {
        let manifest = parse_manifest(
            r#"
                RUNE-ABI 1
                package org.rustos.example
                kind application
                runtime-abi 1 3
                version 2 4 6
                lifecycle multi-instance
                name default "File Explorer"
                name ru-RU "Проводник файлов"
                summary ru-RU "Открывает файлы # символ не комментарий"
                capability optional org.rustos.vfs/1 1 0x3 4
                icon 64 64 200 svg dark application assets/icon.svg
                resource ui/main application/rui assets/main.rui
            "#,
        )
        .unwrap();
        assert_eq!(manifest.runtime_abi_maximum, 3);
        assert_eq!(manifest.version, (2, 4, 6));
        assert_eq!(manifest.metadata.len(), 3);
        assert_eq!(manifest.metadata[1].value, "Проводник файлов");
        assert_eq!(manifest.icons.len(), 1);
        assert_eq!(manifest.resources[0].logical_name, "ui/main");
        assert_eq!(manifest.capabilities[0].rights, 3);
    }

    #[test]
    fn manifest_rejects_unterminated_quotes_and_parent_resource_path() {
        assert!(parse_manifest(
            "RUNE-ABI 1\npackage x\nkind application\nname ru-RU \"Проводник\n"
        )
        .is_err());
        assert!(parse_manifest(
            "RUNE-ABI 1\npackage x\nkind application\nresource ../secret text/plain file\n"
        )
        .is_err());
    }
}

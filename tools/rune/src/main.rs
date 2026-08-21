//! Host-инструмент миграции toolchain output в RUNE.
//!
//! Rust и C компиляторы не требуется форкать: они создают привычный ELF64
//! PIE, после чего этот инструмент переносит только семантически нужные
//! regions/relocations/TLS/RELRO в компактный нативный контейнер.

#![cfg_attr(target_os = "rustos", feature(restricted_std))]

#[cfg(target_os = "rustos")]
use rustos_crt as _;

use std::{env, fs, path::Path, process};

use rustos_rune_format::{
    architecture, dependency_flags, export_flags, file_flags, import_flags, interface_id,
    record_kind, region_flags, relocation_kind, sha256_with_zeroed_range, symbol_id, Container,
    InterfaceId, CONTENT_HASH_OFFSET, DEPENDENCY_SIZE, EXPORT_SIZE, FORMAT_VERSION, HEADER_SIZE,
    IMPORT_SIZE, MAGIC, PAGE_SIZE, RELOCATION_SIZE, TOC_ENTRY_SIZE,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArtifactKind {
    Application,
    Library,
    Service,
    Driver,
}

impl ArtifactKind {
    const fn flags(self) -> u32 {
        match self {
            Self::Application => file_flags::APPLICATION,
            Self::Library => file_flags::LIBRARY,
            Self::Service => file_flags::SERVICE,
            Self::Driver => file_flags::DRIVER,
        }
    }
}

#[derive(Clone, Debug)]
struct ManifestSymbol {
    elf_name: String,
    interface: InterfaceId,
    signature: String,
    minimum_abi: u16,
    maximum_abi: u16,
    flags: u32,
}

#[derive(Clone, Debug)]
struct ManifestDependency {
    file_name: String,
    interface: InterfaceId,
    minimum_abi: u16,
    maximum_abi: u16,
}

#[derive(Clone, Debug)]
struct AbiManifest {
    package: String,
    kind: ArtifactKind,
    interface: Option<InterfaceId>,
    abi_version: u16,
    imports: Vec<ManifestSymbol>,
    exports: Vec<ManifestSymbol>,
    dependencies: Vec<ManifestDependency>,
}

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
        [_, input, output] => pack(Path::new(input), Path::new(output), None, None),
        [_, command, input, output, name] if command == "pack" => {
            pack(Path::new(input), Path::new(output), Some(name), None)
        }
        [_, command, input, output, manifest] if command == "pack-manifest" => {
            let manifest_path = Path::new(manifest);
            let source = fs::read_to_string(manifest_path)
                .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
            let manifest = parse_manifest(&source)?;
            pack(Path::new(input), Path::new(output), None, Some(&manifest))
        }
        _ => Err(
            "usage: rustos-rune <input.elf> <output.rune> | pack <input> <output> <package> | pack-manifest <input> <output> <abi.rune> | verify|inspect <file>"
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

/// Минимальный декларативный ABI-язык намеренно не требует TOML/JSON parser:
/// этот же код позднее станет маленьким native SDK tool внутри RustOS.
/// Значения не содержат пробелов, комментарий начинается с `#`.
fn parse_manifest(source: &str) -> Result<AbiManifest, String> {
    let mut package = None;
    let mut kind = None;
    let mut canonical_interface = None;
    let mut abi_version = 1u16;
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut dependencies = Vec::new();
    let mut saw_header = false;

    for (line_index, raw_line) in source.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        let line_number = line_index + 1;
        if !saw_header {
            if fields.as_slice() != ["RUNE-ABI", "1"] {
                return Err(format!(
                    "manifest line {line_number}: expected `RUNE-ABI 1`"
                ));
            }
            saw_header = true;
            continue;
        }
        match fields.as_slice() {
            ["package", value] => package = Some((*value).to_owned()),
            ["kind", value] => {
                kind = Some(match *value {
                    "application" => ArtifactKind::Application,
                    "library" => ArtifactKind::Library,
                    "service" => ArtifactKind::Service,
                    "driver" => ArtifactKind::Driver,
                    _ => {
                        return Err(format!(
                            "manifest line {line_number}: unknown artifact kind"
                        ))
                    }
                })
            }
            ["interface", value] => canonical_interface = Some(interface_id(value)),
            ["abi", value] => {
                abi_version = value
                    .parse()
                    .map_err(|_| format!("manifest line {line_number}: invalid ABI version"))?;
            }
            ["dependency", file, interface, minimum, maximum] => {
                dependencies.push(ManifestDependency {
                    file_name: (*file).to_owned(),
                    interface: interface_id(interface),
                    minimum_abi: parse_abi(minimum, line_number)?,
                    maximum_abi: parse_abi(maximum, line_number)?,
                });
            }
            ["import", name, interface, signature, minimum, maximum, symbol_kind] => {
                imports.push(ManifestSymbol {
                    elf_name: (*name).to_owned(),
                    interface: interface_id(interface),
                    signature: (*signature).to_owned(),
                    minimum_abi: parse_abi(minimum, line_number)?,
                    maximum_abi: parse_abi(maximum, line_number)?,
                    flags: parse_symbol_kind(symbol_kind, line_number)?,
                });
            }
            ["export", name, signature, symbol_kind] => {
                let interface = canonical_interface.ok_or_else(|| {
                    format!("manifest line {line_number}: `interface` must precede exports")
                })?;
                exports.push(ManifestSymbol {
                    elf_name: (*name).to_owned(),
                    interface,
                    signature: (*signature).to_owned(),
                    minimum_abi: abi_version,
                    maximum_abi: abi_version,
                    flags: parse_symbol_kind(symbol_kind, line_number)?,
                });
            }
            _ => return Err(format!("manifest line {line_number}: invalid directive")),
        }
    }
    if !saw_header {
        return Err("manifest is empty".into());
    }
    let manifest = AbiManifest {
        package: package.ok_or("manifest has no package")?,
        kind: kind.ok_or("manifest has no kind")?,
        interface: canonical_interface,
        abi_version,
        imports,
        exports,
        dependencies,
    };
    for import in &manifest.imports {
        if import.minimum_abi == 0
            || import.minimum_abi > import.maximum_abi
            || !manifest.dependencies.iter().any(|dependency| {
                dependency.interface == import.interface
                    && dependency.minimum_abi <= import.maximum_abi
                    && dependency.maximum_abi >= import.minimum_abi
            })
        {
            return Err(format!(
                "import {} has no compatible declared dependency",
                import.elf_name
            ));
        }
    }
    Ok(manifest)
}

fn parse_abi(value: &str, line: usize) -> Result<u16, String> {
    value
        .parse::<u16>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| format!("manifest line {line}: ABI version must be 1..65535"))
}

fn parse_symbol_kind(value: &str, line: usize) -> Result<u32, String> {
    match value {
        "function" => Ok(import_flags::FUNCTION),
        "data" => Ok(import_flags::DATA),
        "tls" => Ok(import_flags::TLS),
        _ => Err(format!(
            "manifest line {line}: symbol kind must be function, data or tls"
        )),
    }
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
        .map(|manifest| manifest.kind.flags())
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

    let strings_index = records.len() as u32;
    let mut string_record = OutputRecord::metadata(record_kind::STRINGS, architecture::ANY);
    string_record.alignment = 1;
    string_record.file_size = strings.len() as u64;
    string_record.memory_size = strings.len() as u64;
    string_record.payload = strings;
    records.push(string_record);

    encode(records, strings_index, package_name, artifact_flags)
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
            payload.extend_from_slice(&[0u8; 16]);
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
}

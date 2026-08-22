//! Детерминированный code generator RUIDL.
//!
//! На диске создаётся пара crates: полный `-sys` raw ABI и safe facade.
//! Функции только со скалярными параметрами автоматически безопасны. Pointer
//! contracts остаются доступны исключительно через `unsafe_api`, пока схема
//! явно не опишет ownership/borrow — generator никогда не угадывает safety.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use rustos_ruidl::{
    parse_function_signature, parse_manifest, AbiManifest, AbiType, ArtifactKind,
    FunctionSignature, HandleUseKind, ManifestErrorSet, PointerContractKind, ScalarType,
    SliceEncoding,
};
use rustos_rune_format::{
    parse_interface_schema_header, record_kind, sha256, Container, INTERFACE_SCHEMA_HEADER_SIZE,
};

pub const GENERATOR_VERSION: &str = "6";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedPackage {
    pub cache_key: String,
    pub path: PathBuf,
    pub safe_package: String,
    pub raw_package: String,
}

pub fn embedded_schema(bytes: &[u8]) -> Result<&[u8], String> {
    let container = Container::parse(bytes).map_err(|error| format!("invalid RUNE: {error:?}"))?;
    let mut found = None;
    for entry in container
        .entries()
        .filter(|entry| entry.kind == record_kind::INTERFACE_SCHEMA)
    {
        if found.is_some() {
            return Err("RUNE contains several interface schemas".into());
        }
        let payload = container
            .payload(entry)
            .ok_or_else(|| String::from("truncated interface schema"))?;
        let header = parse_interface_schema_header(payload)
            .ok_or_else(|| String::from("invalid interface schema header"))?;
        let source_size = usize::try_from(header.source_size)
            .map_err(|_| String::from("interface schema is too large"))?;
        let end = INTERFACE_SCHEMA_HEADER_SIZE
            .checked_add(source_size)
            .ok_or_else(|| String::from("interface schema size overflow"))?;
        let source = payload
            .get(INTERFACE_SCHEMA_HEADER_SIZE..end)
            .ok_or_else(|| String::from("truncated interface schema source"))?;
        if end != payload.len() {
            return Err("interface schema has trailing bytes".into());
        }
        found = Some(source);
    }
    found.ok_or_else(|| String::from("RUNE has no embedded interface schema"))
}

pub fn load_schema(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.starts_with(b"RUNE-ABI ") {
        return Ok(bytes);
    }
    embedded_schema(&bytes).map(<[u8]>::to_vec)
}

pub fn resolve_to_cache(
    schema: &[u8],
    cache: &Path,
    target_abi: &str,
) -> Result<GeneratedPackage, String> {
    let source = core::str::from_utf8(schema)
        .map_err(|_| String::from("RUIDL schema is not valid UTF-8"))?;
    let manifest = parse_manifest(source)?;
    if manifest.kind != ArtifactKind::Library {
        return Err("SDK bindings can be generated only for a library manifest".into());
    }
    let interface = manifest
        .interface
        .ok_or_else(|| String::from("library manifest has no interface"))?;
    validate_target(target_abi)?;

    let mut key_input = Vec::new();
    key_input.extend_from_slice(b"RustOS/RUIDL-SDK-cache/v1\0");
    key_input.extend_from_slice(GENERATOR_VERSION.as_bytes());
    key_input.push(0);
    key_input.extend_from_slice(target_abi.as_bytes());
    key_input.push(0);
    key_input.extend_from_slice(&interface.0);
    key_input.extend_from_slice(schema);
    let key = hex(&sha256(&key_input));
    let destination = cache.join(&key);
    let package_names = package_names(&manifest.package);
    let result = GeneratedPackage {
        cache_key: key.clone(),
        path: destination.clone(),
        safe_package: package_names.0.clone(),
        raw_package: package_names.1.clone(),
    };
    fs::create_dir_all(cache).map_err(|error| format!("{}: {error}", cache.display()))?;
    if destination.exists() {
        verify_cached(&destination, &key, target_abi)?;
        return Ok(result);
    }

    let temporary = cache.join(format!(".{key}.tmp-{}", std::process::id()));
    match fs::remove_dir_all(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", temporary.display())),
    }
    fs::create_dir(&temporary).map_err(|error| format!("{}: {error}", temporary.display()))?;
    let generation = generate(&manifest, &package_names.0, &package_names.1, target_abi)?;
    if let Err(error) = write_generation(&temporary, &generation, &key, target_abi, schema) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    match fs::rename(&temporary, &destination) {
        Ok(()) => {}
        Err(_error) if destination.exists() => {
            let _ = fs::remove_dir_all(&temporary);
            verify_cached(&destination, &key, target_abi)?;
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(format!(
                "atomic cache commit {} -> {}: {error}",
                temporary.display(),
                destination.display()
            ));
        }
    }
    Ok(result)
}

struct Generation {
    root_manifest: String,
    cargo_lock: String,
    safe_source: String,
    raw_manifest: String,
    raw_source: String,
    report: String,
}

fn generate(
    manifest: &AbiManifest,
    safe_package: &str,
    raw_package: &str,
    target: &str,
) -> Result<Generation, String> {
    let interface = manifest
        .interface
        .ok_or_else(|| String::from("library manifest has no interface"))?;
    let mut signatures = Vec::new();
    for export in &manifest.exports {
        let signature = parse_function_signature(&export.signature)?;
        signatures.push((export, signature));
    }
    let raw_crate = raw_package.replace('-', "_");
    let root_manifest = format!(
        "[workspace]\nmembers = [\"raw\"]\nresolver = \"2\"\n\n[package]\nname = \"{safe_package}\"\nversion = \"{}.{}.{}\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\n{raw_crate} = {{ package = \"{raw_package}\", path = \"raw\" }}\n",
        manifest.version.0, manifest.version.1, manifest.version.2
    );
    let raw_manifest = format!(
        "[package]\nname = \"{raw_package}\"\nversion = \"{}.{}.{}\"\nedition = \"2021\"\npublish = false\n\n[lib]\ndoctest = false\n",
        manifest.version.0, manifest.version.1, manifest.version.2
    );
    let version = format!(
        "{}.{}.{}",
        manifest.version.0, manifest.version.1, manifest.version.2
    );
    let cargo_lock = format!(
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"{safe_package}\"\nversion = \"{version}\"\ndependencies = [\n \"{raw_package}\",\n]\n\n[[package]]\nname = \"{raw_package}\"\nversion = \"{version}\"\n"
    );
    let mut raw_source = String::from(
        "//! Сгенерированный raw ABI. Не редактировать: источник — встроенная RUIDL-схема.\n#![no_std]\n#![allow(non_camel_case_types)]\n\n",
    );
    for opaque in &manifest.opaque_types {
        let rust_name = rust_type_name(&opaque.name);
        raw_source.push_str(&format!(
            "#[repr(C, align({}))]\npub struct {rust_name} {{ _storage: [u8; {}] }}\nconst _: [(); {}] = [(); core::mem::size_of::<{rust_name}>()];\nconst _: [(); {}] = [(); core::mem::align_of::<{rust_name}>()];\n\n",
            opaque.alignment, opaque.size, opaque.size, opaque.alignment
        ));
    }
    for structure in &manifest.structs {
        raw_source.push_str(&raw_struct(structure));
    }
    raw_source.push_str("unsafe extern \"C\" {\n");
    for (export, signature) in &signatures {
        raw_source.push_str(&format!(
            "    #[link_name = \"{}\"]\n    pub fn {}({}){};\n",
            export.elf_name,
            rust_identifier(&signature.name),
            parameter_list(signature, false),
            result_type(&signature.result)
        ));
    }
    raw_source.push_str("}\n");

    let interface_bytes = interface
        .0
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut safe_source = format!(
        "//! Safe facade, сгенерированный RUIDL compiler для `{target}`.\n//! Все unsafe-вызовы замкнуты внутри проверенных ownership/layout/error contracts.\n#![no_std]\n\npub use {raw_crate} as unsafe_api;\n\n/// Canonical RUNE package.\npub const PACKAGE: &str = \"{}\";\n/// InterfaceId, по которому package resolver выбирает DLL.\npub const INTERFACE_ID: [u8; 16] = [{interface_bytes}];\n/// Версия C ABI этого facade.\npub const ABI_VERSION: u16 = {};\n\n",
        manifest.package, manifest.abi_version
    );
    safe_source.push_str(&safe_types(manifest));
    let mut safe_count = 0usize;
    let mut unsafe_count = 0usize;
    for (_, signature) in &signatures {
        if let Some(result) = manifest
            .result_contracts
            .iter()
            .find(|contract| contract.function == signature.name)
        {
            safe_source.push_str(&safe_pointer_wrapper(manifest, signature, &result.errors)?);
            safe_count += 1;
        } else if signature.parameters.iter().all(AbiType::is_safe_by_value)
            && signature.result.is_safe_by_value()
        {
            let name = rust_identifier(&signature.name);
            safe_source.push_str(&format!(
                "#[inline]\npub fn {name}({}){} {{\n    // SAFETY: RUIDL contract contains only by-value scalar types.\n    unsafe {{ unsafe_api::{name}({}) }}\n}}\n\n",
                parameter_list(signature, true),
                result_type(&signature.result),
                argument_list(signature)
            ));
            safe_count += 1;
        } else {
            unsafe_count += 1;
        }
    }
    let report = format!(
        "# Generated RUIDL bindings\n\n- package: `{}`\n- target ABI: `{target}`\n- fully safe functions: {safe_count}\n- functions available only in `unsafe_api`: {unsafe_count}\n\nA pointer function is promoted only when RUIDL defines layout, borrow/slice/out ownership, error semantics and every required output bound.\n",
        manifest.package
    );
    Ok(Generation {
        root_manifest,
        cargo_lock,
        safe_source,
        raw_manifest,
        raw_source,
        report,
    })
}

fn raw_struct(structure: &rustos_ruidl::ManifestStruct) -> String {
    let rust_name = rust_type_name(&structure.name);
    let mut source = format!(
        "#[repr(C, align({}))]\npub struct {rust_name} {{\n",
        structure.alignment
    );
    let mut fields = structure.fields.iter().collect::<Vec<_>>();
    fields.sort_by_key(|field| field.offset);
    let mut cursor = 0u32;
    let mut padding = 0usize;
    for field in fields {
        if cursor < field.offset {
            source.push_str(&format!(
                "    _padding{padding}: [u8; {}],\n",
                field.offset - cursor
            ));
            padding += 1;
        }
        let scalar = scalar_rust(field.scalar);
        let ty = if field.count == 1 {
            scalar.into()
        } else {
            format!("[{scalar}; {}]", field.count)
        };
        source.push_str(&format!(
            "    pub {}: {ty},\n",
            rust_identifier(&field.name)
        ));
        cursor = field.offset + field.scalar.byte_size(8) * field.count;
    }
    if cursor < structure.size {
        source.push_str(&format!(
            "    _padding{padding}: [u8; {}],\n",
            structure.size - cursor
        ));
    }
    source.push_str(&format!(
        "}}\nconst _: [(); {}] = [(); core::mem::size_of::<{rust_name}>()];\nconst _: [(); {}] = [(); core::mem::align_of::<{rust_name}>()];\n\n",
        structure.size, structure.alignment
    ));
    source
}

fn safe_types(manifest: &AbiManifest) -> String {
    let mut source = String::new();
    for opaque in &manifest.opaque_types {
        let name = rust_type_name(&opaque.name);
        source.push_str(&format!(
            "/// Непрозрачное состояние provider: размер и alignment проверены RUIDL.\npub struct {name} {{ raw: unsafe_api::{name} }}\n\n"
        ));
    }
    for structure in &manifest.structs {
        let name = rust_type_name(&structure.name);
        source.push_str(&format!("pub use unsafe_api::{name};\n"));
    }
    if !manifest.structs.is_empty() {
        source.push('\n');
    }
    for handle in &manifest.handles {
        let name = rust_type_name(&handle.name);
        let repr = scalar_rust(handle.repr);
        source.push_str(&format!(
            "/// Линейный capability/handle: намеренно не реализует Copy и Clone.\n#[repr(transparent)]\n#[derive(Debug, Eq, PartialEq)]\npub struct {name} {{ raw: {repr} }}\nimpl {name} {{\n    const INVALID: {repr} = {} as {repr};\n    #[inline] fn from_raw(raw: {repr}) -> Self {{ Self {{ raw }} }}\n    #[inline] fn raw(&self) -> {repr} {{ self.raw }}\n    #[inline] fn into_raw(self) -> {repr} {{ self.raw }}\n}}\n\n",
            handle.invalid
        ));
    }
    for errors in &manifest.error_sets {
        source.push_str(&safe_error_type(errors));
    }
    source
}

fn safe_error_type(errors: &ManifestErrorSet) -> String {
    let name = rust_type_name(&errors.name);
    let repr = scalar_rust(errors.repr);
    let mut source = format!(
        "/// Расширяемый код ошибки ABI. Newtype безопасно сохраняет неизвестные будущие коды.\n#[repr(transparent)]\n#[derive(Clone, Copy, Debug, Eq, PartialEq)]\npub struct {name}({repr});\nimpl {name} {{\n    pub const SUCCESS: {repr} = {} as {repr};\n    pub const CONTRACT_VIOLATION: Self = Self({} as {repr});\n",
        errors.success, errors.contract_violation
    );
    for case in &errors.cases {
        source.push_str(&format!(
            "    pub const {}: Self = Self({} as {repr});\n",
            rust_identifier(&case.name),
            case.value
        ));
    }
    source.push_str(&format!(
        "    #[inline] pub const fn from_code(code: {repr}) -> Self {{ Self(code) }}\n    #[inline] pub const fn code(self) -> {repr} {{ self.0 }}\n}}\n\n"
    ));
    source
}

fn safe_pointer_wrapper(
    manifest: &AbiManifest,
    signature: &FunctionSignature,
    error_name: &str,
) -> Result<String, String> {
    if !manifest
        .error_sets
        .iter()
        .any(|candidate| candidate.name == error_name)
    {
        return Err(format!("missing error set {error_name}"));
    }
    let error_type = rust_type_name(error_name);
    let pointer_contracts = manifest
        .pointer_contracts
        .iter()
        .filter(|contract| contract.function == signature.name)
        .map(|contract| (usize::from(contract.parameter), contract))
        .collect::<BTreeMap<_, _>>();
    let handle_uses = manifest
        .handle_uses
        .iter()
        .filter(|contract| contract.function == signature.name)
        .map(|contract| (usize::from(contract.parameter), contract))
        .collect::<BTreeMap<_, _>>();
    let length_parameters = pointer_contracts
        .values()
        .filter_map(|contract| contract.length_parameter.map(usize::from))
        .collect::<BTreeSet<_>>();
    let outputs = pointer_contracts
        .iter()
        .filter(|(_, contract)| contract.kind == PointerContractKind::Out)
        .map(|(index, contract)| (*index, *contract))
        .collect::<Vec<_>>();

    let mut safe_parameters = Vec::new();
    let mut raw_arguments = Vec::new();
    let mut prelude = String::new();
    for (index, abi_type) in signature.parameters.iter().enumerate() {
        let argument = format!("arg{index}");
        if let Some(contract) = pointer_contracts.get(&index) {
            match contract.kind {
                PointerContractKind::BorrowShared | PointerContractKind::BorrowExclusive => {
                    let ty = named_safe_type(manifest, &contract.type_name)?;
                    let borrow = if contract.kind == PointerContractKind::BorrowExclusive {
                        "&mut "
                    } else {
                        "&"
                    };
                    safe_parameters.push(format!("{argument}: {borrow}{ty}"));
                    let mutable = contract.kind == PointerContractKind::BorrowExclusive;
                    let raw = if manifest
                        .opaque_types
                        .iter()
                        .any(|opaque| opaque.name == contract.type_name)
                    {
                        if mutable {
                            format!("&mut {argument}.raw")
                        } else {
                            format!("&{argument}.raw")
                        }
                    } else {
                        argument.clone()
                    };
                    raw_arguments.push(raw);
                }
                PointerContractKind::SliceIn | PointerContractKind::SliceOut => {
                    let ty = match (contract.kind, contract.encoding) {
                        (PointerContractKind::SliceIn, SliceEncoding::Utf8) => "&str",
                        (PointerContractKind::SliceIn, SliceEncoding::Bytes) => "&[u8]",
                        (PointerContractKind::SliceOut, SliceEncoding::Bytes) => "&mut [u8]",
                        (PointerContractKind::SliceOut, SliceEncoding::Utf8) => {
                            return Err(format!(
                                "{}: mutable UTF-8 output cannot be represented safely",
                                signature.name
                            ));
                        }
                        _ => unreachable!(),
                    };
                    safe_parameters.push(format!("{argument}: {ty}"));
                    let pointer = if contract.kind == PointerContractKind::SliceOut {
                        "as_mut_ptr"
                    } else {
                        "as_ptr"
                    };
                    raw_arguments.push(format!("{argument}.{pointer}()"));
                }
                PointerContractKind::Out => {
                    let raw = named_raw_type(manifest, &contract.type_name)?;
                    prelude.push_str(&format!(
                        "    // Нули заранее инициализируют explicit padding raw-структур; provider заменяет поля.\n    let mut out{index} = core::mem::MaybeUninit::<{raw}>::zeroed();\n"
                    ));
                    raw_arguments.push(format!("out{index}.as_mut_ptr()"));
                }
            }
        } else if let Some(contract) = handle_uses.get(&index) {
            let ty = rust_type_name(&contract.handle);
            match contract.kind {
                HandleUseKind::Borrow => {
                    safe_parameters.push(format!("{argument}: &{ty}"));
                    raw_arguments.push(format!("{argument}.raw()"));
                }
                HandleUseKind::Consume => {
                    safe_parameters.push(format!("{argument}: {ty}"));
                    raw_arguments.push(format!("{argument}.into_raw()"));
                }
            }
        } else if length_parameters.contains(&index) {
            let owner = pointer_contracts
                .values()
                .find(|contract| contract.length_parameter == Some(index as u16))
                .ok_or_else(|| format!("{}: orphan length", signature.name))?;
            raw_arguments.push(format!("arg{}.len()", owner.parameter));
        } else {
            safe_parameters.push(format!("{argument}: {}", raw_type(abi_type)));
            raw_arguments.push(argument);
        }
    }

    let output_types = outputs
        .iter()
        .map(|(_, contract)| named_safe_type(manifest, &contract.type_name))
        .collect::<Result<Vec<_>, _>>()?;
    let success_type = match output_types.as_slice() {
        [] => "()".into(),
        [single] => single.clone(),
        several => format!("({})", several.join(", ")),
    };
    let name = rust_identifier(&signature.name);
    let mut source = format!(
        "#[inline]\npub fn {name}({}) -> core::result::Result<{success_type}, {error_type}> {{\n{prelude}    // SAFETY: generator проверил layout и контракт каждого pointer/handle.\n    let status = unsafe {{ unsafe_api::{name}({}) }};\n    if status != {error_type}::SUCCESS {{\n        return Err({error_type}::from_code(status));\n    }}\n",
        safe_parameters.join(", "),
        raw_arguments.join(", ")
    );
    for (index, contract) in &outputs {
        source.push_str(&format!(
            "    // SAFETY: result contract требует инициализации out{index} при SUCCESS.\n    let out{index} = unsafe {{ out{index}.assume_init() }};\n"
        ));
        if let Some(handle) = manifest
            .handles
            .iter()
            .find(|handle| handle.name == contract.type_name)
        {
            let handle_type = rust_type_name(&handle.name);
            source.push_str(&format!(
                "    if out{index} == {handle_type}::INVALID {{\n        return Err({error_type}::CONTRACT_VIOLATION);\n    }}\n    let out{index} = {handle_type}::from_raw(out{index});\n"
            ));
        } else if manifest
            .opaque_types
            .iter()
            .any(|opaque| opaque.name == contract.type_name)
        {
            let opaque_type = rust_type_name(&contract.type_name);
            source.push_str(&format!(
                "    let out{index} = {opaque_type} {{ raw: out{index} }};\n"
            ));
        }
        if let Some(bound) = manifest.bounds.iter().find(|bound| {
            bound.function == signature.name && usize::from(bound.output_parameter) == *index
        }) {
            let maximum = pointer_contracts
                .values()
                .find(|contract| contract.length_parameter == Some(bound.maximum_parameter))
                .ok_or_else(|| format!("{}: missing bound owner", signature.name))?;
            source.push_str(&format!(
                "    if out{index} > arg{}.len() {{\n        return Err({error_type}::CONTRACT_VIOLATION);\n    }}\n",
                maximum.parameter
            ));
        }
    }
    let success = match outputs.as_slice() {
        [] => "()".into(),
        [(index, _)] => format!("out{index}"),
        several => format!(
            "({})",
            several
                .iter()
                .map(|(index, _)| format!("out{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    source.push_str(&format!("    Ok({success})\n}}\n\n"));
    Ok(source)
}

fn named_raw_type(manifest: &AbiManifest, name: &str) -> Result<String, String> {
    if let Some(scalar) = scalar_name(name) {
        return Ok(scalar.into());
    }
    if let Some(handle) = manifest.handles.iter().find(|handle| handle.name == name) {
        return Ok(scalar_rust(handle.repr).into());
    }
    if manifest
        .opaque_types
        .iter()
        .any(|opaque| opaque.name == name)
        || manifest
            .structs
            .iter()
            .any(|structure| structure.name == name)
    {
        return Ok(format!("unsafe_api::{}", rust_type_name(name)));
    }
    Err(format!("unknown RUIDL type {name}"))
}

fn named_safe_type(manifest: &AbiManifest, name: &str) -> Result<String, String> {
    if let Some(scalar) = scalar_name(name) {
        return Ok(scalar.into());
    }
    if manifest.handles.iter().any(|handle| handle.name == name)
        || manifest
            .opaque_types
            .iter()
            .any(|opaque| opaque.name == name)
        || manifest
            .structs
            .iter()
            .any(|structure| structure.name == name)
    {
        return Ok(rust_type_name(name));
    }
    Err(format!("unknown RUIDL type {name}"))
}

fn write_generation(
    root: &Path,
    generation: &Generation,
    key: &str,
    target: &str,
    schema: &[u8],
) -> Result<(), String> {
    fs::create_dir(root.join("src")).map_err(|error| error.to_string())?;
    fs::create_dir(root.join("raw")).map_err(|error| error.to_string())?;
    fs::create_dir(root.join("raw/src")).map_err(|error| error.to_string())?;
    let files = [
        ("Cargo.toml", generation.root_manifest.as_bytes()),
        ("Cargo.lock", generation.cargo_lock.as_bytes()),
        ("src/lib.rs", generation.safe_source.as_bytes()),
        ("raw/Cargo.toml", generation.raw_manifest.as_bytes()),
        ("raw/src/lib.rs", generation.raw_source.as_bytes()),
        ("RUIDL.md", generation.report.as_bytes()),
        ("schema.ruidl", schema),
    ];
    for (relative, bytes) in files {
        write(root.join(relative), bytes)?;
    }
    let lock = cache_lock(key, target, &files);
    write(root.join("ruidl.lock"), lock.as_bytes())
}

fn write(path: PathBuf, bytes: &[u8]) -> Result<(), String> {
    fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn verify_cached(path: &Path, key: &str, target: &str) -> Result<(), String> {
    verify_cache_shape(path)?;
    let actual = fs::read_to_string(path.join("ruidl.lock"))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reconstructed =
        format!("RUIDL-CACHE 1\nkey {key}\ngenerator {GENERATOR_VERSION}\ntarget {target}\n");
    if !actual.starts_with(&reconstructed) {
        return Err(format!(
            "{}: cache entry failed integrity check",
            path.display()
        ));
    }
    for required in [
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        "raw/Cargo.toml",
        "raw/src/lib.rs",
        "RUIDL.md",
        "schema.ruidl",
    ] {
        let bytes = fs::read(path.join(required))
            .map_err(|_| format!("{}: incomplete cache entry", path.display()))?;
        reconstructed.push_str(&format!("file {required} {}\n", hex(&sha256(&bytes))));
    }
    if actual != reconstructed {
        return Err(format!("{}: cached file hash mismatch", path.display()));
    }
    Ok(())
}

fn verify_cache_shape(path: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    collect_cache_files(path, path, &mut files)?;
    files.sort();
    let expected = [
        "Cargo.lock",
        "Cargo.toml",
        "RUIDL.md",
        "raw/Cargo.toml",
        "raw/src/lib.rs",
        "ruidl.lock",
        "schema.ruidl",
        "src/lib.rs",
    ];
    if files != expected {
        return Err(format!(
            "{}: cache contains an unexpected or missing source file",
            path.display()
        ));
    }
    Ok(())
}

fn collect_cache_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<String>,
) -> Result<(), String> {
    for entry in
        fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", entry.path().display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "{}: symlink is forbidden in SDK cache",
                entry.path().display()
            ));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| String::from("cache path escaped its root"))?
            .to_str()
            .ok_or_else(|| String::from("cache path is not UTF-8"))?
            .replace('\\', "/");
        if file_type.is_dir() {
            if relative == "target" {
                continue;
            }
            if !matches!(relative.as_str(), "raw" | "raw/src" | "src") {
                return Err(format!(
                    "{}: unexpected directory in SDK cache",
                    entry.path().display()
                ));
            }
            collect_cache_files(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(relative);
        } else {
            return Err(format!(
                "{}: unsupported entry type in SDK cache",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn cache_lock(key: &str, target: &str, files: &[(&str, &[u8])]) -> String {
    let mut lock =
        format!("RUIDL-CACHE 1\nkey {key}\ngenerator {GENERATOR_VERSION}\ntarget {target}\n");
    for (relative, bytes) in files {
        lock.push_str(&format!("file {relative} {}\n", hex(&sha256(bytes))));
    }
    lock
}

fn parameter_list(signature: &FunctionSignature, safe: bool) -> String {
    signature
        .parameters
        .iter()
        .enumerate()
        .map(|(index, abi_type)| {
            let ty = if safe {
                safe_type(abi_type)
            } else {
                raw_type(abi_type)
            };
            format!("arg{index}: {ty}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn argument_list(signature: &FunctionSignature) -> String {
    (0..signature.parameters.len())
        .map(|index| format!("arg{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn result_type(result: &AbiType) -> String {
    match result {
        AbiType::Void => String::new(),
        _ => format!(" -> {}", raw_type(result)),
    }
}

fn safe_type(abi_type: &AbiType) -> String {
    raw_type(abi_type)
}

fn raw_type(abi_type: &AbiType) -> String {
    match abi_type {
        AbiType::Void => "()".into(),
        AbiType::Scalar(scalar) => scalar_rust(*scalar).into(),
        AbiType::Pointer { mutable, pointee } => {
            let prefix = if *mutable { "*mut " } else { "*const " };
            let target = scalar_name(pointee)
                .map(str::to_owned)
                .unwrap_or_else(|| rust_type_name(pointee));
            format!("{prefix}{target}")
        }
    }
}

fn scalar_rust(value: ScalarType) -> &'static str {
    match value {
        ScalarType::U8 => "u8",
        ScalarType::U16 => "u16",
        ScalarType::U32 => "u32",
        ScalarType::U64 => "u64",
        ScalarType::U128 => "u128",
        ScalarType::I8 => "i8",
        ScalarType::I16 => "i16",
        ScalarType::I32 => "i32",
        ScalarType::I64 => "i64",
        ScalarType::I128 => "i128",
        ScalarType::Usize => "usize",
        ScalarType::Isize => "isize",
        ScalarType::F32 => "f32",
        ScalarType::F64 => "f64",
    }
}

fn scalar_name(value: &str) -> Option<&'static str> {
    Some(match value {
        "u8" => "u8",
        "u16" => "u16",
        "u32" => "u32",
        "u64" => "u64",
        "u128" => "u128",
        "i8" => "i8",
        "i16" => "i16",
        "i32" => "i32",
        "i64" => "i64",
        "i128" => "i128",
        "usize" => "usize",
        "isize" => "isize",
        "f32" => "f32",
        "f64" => "f64",
        _ => return None,
    })
}

fn package_names(canonical: &str) -> (String, String) {
    let base = canonical
        .strip_prefix("org.rustos.")
        .unwrap_or(canonical)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let base = base.trim_matches('-');
    let safe = format!("rustos-{base}");
    let raw = format!("{safe}-sys");
    (safe, raw)
}

fn rust_identifier(value: &str) -> String {
    match value {
        "type" | "match" | "loop" | "move" | "ref" | "self" | "crate" | "super" | "fn"
        | "struct" | "enum" | "mod" | "use" | "unsafe" | "extern" => {
            format!("r#{value}")
        }
        _ => value.into(),
    }
}

fn rust_type_name(value: &str) -> String {
    let mut output = String::new();
    let mut uppercase = true;
    for character in value.chars() {
        if character == '_' {
            uppercase = true;
        } else if uppercase {
            output.push(character.to_ascii_uppercase());
            uppercase = false;
        } else {
            output.push(character);
        }
    }
    output
}

fn validate_target(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("target ABI is not a canonical target name".into());
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_raw_and_safe_facades_deterministically() {
        let source = include_bytes!("../../../sdk/abi/fixture-answer.rune-abi");
        let manifest = parse_manifest(core::str::from_utf8(source).unwrap()).unwrap();
        let generation = generate(
            &manifest,
            "rustos-fixture-answer",
            "rustos-fixture-answer-sys",
            "x86_64-unknown-rustos",
        )
        .unwrap();
        assert!(generation
            .raw_source
            .contains("pub fn fixture_answer() -> u64"));
        assert!(generation
            .safe_source
            .contains("pub fn fixture_answer() -> u64"));
        assert!(!generation.safe_source.contains("pub unsafe fn"));
    }

    #[test]
    fn complete_pointer_contract_generates_safe_facade() {
        let source = include_str!("../../../sdk/abi/vfs-1.rune-abi");
        let manifest = parse_manifest(source).unwrap();
        let generation = generate(
            &manifest,
            "rustos-vfs-client",
            "rustos-vfs-client-sys",
            "aarch64-unknown-rustos",
        )
        .unwrap();
        assert!(generation.raw_source.contains("pub struct Client"));
        assert!(generation
            .raw_source
            .contains("core::mem::size_of::<Client>"));
        assert!(generation.safe_source.contains("pub fn open("));
        assert!(generation.safe_source.contains("arg1: &str"));
        assert!(generation
            .safe_source
            .contains("Result<VfsObject, VfsError>"));
        assert!(generation.safe_source.contains("arg2: &mut [u8]"));
        assert!(!generation.safe_source.contains("pub unsafe fn"));
    }

    #[test]
    fn cache_reuse_verifies_every_generated_file_hash() {
        let cache = std::env::temp_dir().join(format!(
            "rustos-ruidl-test-{}-{}",
            std::process::id(),
            GENERATOR_VERSION
        ));
        let _ = fs::remove_dir_all(&cache);
        let source = include_bytes!("../../../sdk/abi/fixture-answer.rune-abi");
        let generated = resolve_to_cache(source, &cache, "x86_64-unknown-rustos").unwrap();
        assert_eq!(
            resolve_to_cache(source, &cache, "x86_64-unknown-rustos")
                .unwrap()
                .path,
            generated.path
        );
        let original = fs::read(generated.path.join("src/lib.rs")).unwrap();
        fs::write(generated.path.join("src/lib.rs"), b"tampered").unwrap();
        assert!(resolve_to_cache(source, &cache, "x86_64-unknown-rustos").is_err());
        fs::write(generated.path.join("src/lib.rs"), original).unwrap();
        fs::write(generated.path.join("build.rs"), b"fn main() {}").unwrap();
        assert!(resolve_to_cache(source, &cache, "x86_64-unknown-rustos").is_err());
        fs::remove_dir_all(cache).unwrap();
    }
}

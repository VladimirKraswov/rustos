//! RUIDL — единый декларативный источник ABI программ и библиотек RustOS.
//!
//! Parser не читает файлы и не доверяет host path: на вход он получает уже
//! проверенную UTF-8 строку. Поэтому один и тот же код используют упаковщик
//! RUNE, host SDK compiler и, после self-hosting, native SDK compiler RustOS.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

use rustos_rune_format::{
    artifact_kind, capability_flags, file_flags, icon_format, icon_purpose, icon_theme,
    import_flags, interface_id, lifecycle, manifest_flags, metadata_key, package_id, InterfaceId,
};

/// Текущая версия текста RUIDL, встроенного в `INTERFACE_SCHEMA`.
pub const LANGUAGE_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKind {
    Application,
    Library,
    Service,
    Driver,
}

impl ArtifactKind {
    pub const fn file_flags(self) -> u32 {
        match self {
            Self::Application => file_flags::APPLICATION,
            Self::Library => file_flags::LIBRARY,
            Self::Service => file_flags::SERVICE,
            Self::Driver => file_flags::DRIVER,
        }
    }

    pub const fn wire_kind(self) -> u16 {
        match self {
            Self::Application => artifact_kind::APPLICATION,
            Self::Library => artifact_kind::LIBRARY,
            Self::Service => artifact_kind::SERVICE,
            Self::Driver => artifact_kind::DRIVER,
        }
    }

    pub const fn default_lifecycle(self) -> u16 {
        match self {
            Self::Application => lifecycle::MULTI_INSTANCE,
            Self::Library => lifecycle::IN_PROCESS_LIBRARY,
            Self::Service => lifecycle::MANAGED_SERVICE,
            Self::Driver => lifecycle::MANAGED_DRIVER,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestSymbol {
    pub elf_name: String,
    pub interface: InterfaceId,
    pub signature: String,
    pub minimum_abi: u16,
    pub maximum_abi: u16,
    pub flags: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestDependency {
    pub file_name: String,
    pub interface: InterfaceId,
    pub minimum_abi: u16,
    pub maximum_abi: u16,
    /// Optional pin конкретного package provider. Нули в wire record означают,
    /// что supervisor может выбрать любой доверенный provider интерфейса.
    pub package: Option<[u8; 16]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestMetadata {
    pub key: u16,
    pub name: String,
    pub locale: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestIcon {
    pub width: u16,
    pub height: u16,
    pub scale_percent: u16,
    pub format: u16,
    pub theme: u16,
    pub purpose: u16,
    pub path: String,
    /// Производное содержимое asset. Parser оставляет поле пустым; packer
    /// заполняет его только после канонического разрешения пути manifest.
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestResource {
    pub logical_name: String,
    pub content_type: String,
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestCapability {
    pub service: InterfaceId,
    pub rights: u64,
    pub abi_version: u16,
    pub slot_hint: u16,
    pub flags: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbiManifest {
    pub package: String,
    pub kind: ArtifactKind,
    pub interface: Option<InterfaceId>,
    pub abi_version: u16,
    pub imports: Vec<ManifestSymbol>,
    pub exports: Vec<ManifestSymbol>,
    pub dependencies: Vec<ManifestDependency>,
    pub runtime_abi_minimum: u16,
    pub runtime_abi_maximum: u16,
    pub lifecycle: Option<u16>,
    pub flags: u32,
    pub version: (u32, u32, u32),
    pub metadata: Vec<ManifestMetadata>,
    pub icons: Vec<ManifestIcon>,
    pub resources: Vec<ManifestResource>,
    pub capabilities: Vec<ManifestCapability>,
    /// Точные проверенные bytes, которые packer помещает в RUNE. Сохранение
    /// исходника делает hash SDK cache независимым от host parser/форматтера.
    pub schema_source: Vec<u8>,
}

/// Parsed C-подобная canonical function signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionSignature {
    pub name: String,
    pub parameters: Vec<AbiType>,
    pub result: AbiType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AbiType {
    Void,
    Scalar(ScalarType),
    Pointer { mutable: bool, pointee: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScalarType {
    U8,
    U16,
    U32,
    U64,
    U128,
    I8,
    I16,
    I32,
    I64,
    I128,
    Usize,
    Isize,
    F32,
    F64,
}

impl AbiType {
    pub const fn is_safe_by_value(&self) -> bool {
        matches!(self, Self::Void | Self::Scalar(_))
    }
}

/// Разбирает manifest/RUIDL полностью и возвращает line-oriented diagnostic.
pub fn parse_manifest(source: &str) -> Result<AbiManifest, String> {
    let mut package = None;
    let mut kind = None;
    let mut canonical_interface = None;
    let mut abi_version = 1u16;
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut dependencies = Vec::new();
    let mut runtime_abi_minimum = 1u16;
    let mut runtime_abi_maximum = 1u16;
    let mut requested_lifecycle = None;
    let mut flags = 0u32;
    let mut version = (0u32, 1u32, 0u32);
    let mut metadata = Vec::new();
    let mut icons = Vec::new();
    let mut resources = Vec::new();
    let mut capabilities = Vec::new();
    let mut saw_header = false;

    for (line_index, raw_line) in source.lines().enumerate() {
        let line_number = line_index + 1;
        let fields = fields(raw_line, line_number)?;
        if fields.is_empty() {
            continue;
        }
        let fields: Vec<_> = fields.iter().map(String::as_str).collect();
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
            ["package", value] => package = Some((*value).into()),
            ["kind", value] => {
                kind = Some(match *value {
                    "application" => ArtifactKind::Application,
                    "library" => ArtifactKind::Library,
                    "service" => ArtifactKind::Service,
                    "driver" => ArtifactKind::Driver,
                    _ => return Err(line_error(line_number, "unknown artifact kind")),
                });
            }
            ["interface", value] => canonical_interface = Some(interface_id(value)),
            ["abi", value] => abi_version = parse_abi(value, line_number)?,
            ["runtime-abi", minimum, maximum] => {
                runtime_abi_minimum = parse_abi(minimum, line_number)?;
                runtime_abi_maximum = parse_abi(maximum, line_number)?;
                if runtime_abi_minimum > runtime_abi_maximum {
                    return Err(line_error(line_number, "invalid runtime ABI range"));
                }
            }
            ["version", major, minor, patch] => {
                version = (
                    parse_u32(major, line_number, "major version")?,
                    parse_u32(minor, line_number, "minor version")?,
                    parse_u32(patch, line_number, "patch version")?,
                );
            }
            ["lifecycle", value] => {
                requested_lifecycle = Some(parse_lifecycle(value, line_number)?);
            }
            ["flag", "background-allowed"] => flags |= manifest_flags::BACKGROUND_ALLOWED,
            ["flag", "restartable"] => flags |= manifest_flags::RESTARTABLE,
            ["name", locale, value] => metadata.push(ManifestMetadata {
                key: metadata_key::DISPLAY_NAME,
                name: String::new(),
                locale: parse_locale(locale, line_number)?,
                value: (*value).into(),
            }),
            ["summary", locale, value] => metadata.push(ManifestMetadata {
                key: metadata_key::SUMMARY,
                name: String::new(),
                locale: parse_locale(locale, line_number)?,
                value: (*value).into(),
            }),
            ["vendor", value] => metadata.push(simple_metadata(metadata_key::VENDOR, value)),
            ["category", value] => metadata.push(simple_metadata(metadata_key::CATEGORY, value)),
            ["homepage", value] => metadata.push(simple_metadata(metadata_key::HOMEPAGE, value)),
            ["metadata", name, locale, value] => metadata.push(ManifestMetadata {
                key: metadata_key::CUSTOM,
                name: (*name).into(),
                locale: parse_locale(locale, line_number)?,
                value: (*value).into(),
            }),
            ["icon", width, height, scale, format, theme, purpose, path] => {
                icons.push(ManifestIcon {
                    width: parse_nonzero_u16(width, line_number, "icon width")?,
                    height: parse_nonzero_u16(height, line_number, "icon height")?,
                    scale_percent: parse_nonzero_u16(scale, line_number, "icon scale")?,
                    format: parse_icon_format(format, line_number)?,
                    theme: parse_icon_theme(theme, line_number)?,
                    purpose: parse_icon_purpose(purpose, line_number)?,
                    path: (*path).into(),
                    bytes: Vec::new(),
                });
            }
            ["resource", logical_name, content_type, path] => {
                validate_resource_name(logical_name, line_number)?;
                validate_content_type(content_type, line_number)?;
                resources.push(ManifestResource {
                    logical_name: (*logical_name).into(),
                    content_type: (*content_type).into(),
                    path: (*path).into(),
                    bytes: Vec::new(),
                });
            }
            ["capability", policy, service, abi, rights, slot] => {
                capabilities.push(ManifestCapability {
                    service: interface_id(service),
                    rights: parse_u64(rights, line_number, "capability rights")?,
                    abi_version: parse_abi(abi, line_number)?,
                    slot_hint: parse_nonzero_u16(slot, line_number, "capability slot")?,
                    flags: parse_capability_policy(policy, line_number)?,
                });
            }
            ["dependency", file, interface, minimum, maximum] => {
                dependencies.push(ManifestDependency {
                    file_name: (*file).into(),
                    interface: interface_id(interface),
                    minimum_abi: parse_abi(minimum, line_number)?,
                    maximum_abi: parse_abi(maximum, line_number)?,
                    package: None,
                });
            }
            ["dependency", file, interface, minimum, maximum, provider] => {
                dependencies.push(ManifestDependency {
                    file_name: (*file).into(),
                    interface: interface_id(interface),
                    minimum_abi: parse_abi(minimum, line_number)?,
                    maximum_abi: parse_abi(maximum, line_number)?,
                    package: Some(package_id(provider)),
                });
            }
            ["import", name, interface, signature, minimum, maximum, symbol_kind] => {
                parse_function_signature(signature).map_err(|error| {
                    format!("manifest line {line_number}: invalid signature: {error}")
                })?;
                imports.push(ManifestSymbol {
                    elf_name: (*name).into(),
                    interface: interface_id(interface),
                    signature: (*signature).into(),
                    minimum_abi: parse_abi(minimum, line_number)?,
                    maximum_abi: parse_abi(maximum, line_number)?,
                    flags: parse_symbol_kind(symbol_kind, line_number)?,
                });
            }
            ["export", name, signature, symbol_kind] => {
                let interface = canonical_interface
                    .ok_or_else(|| line_error(line_number, "`interface` must precede exports"))?;
                parse_function_signature(signature).map_err(|error| {
                    format!("manifest line {line_number}: invalid signature: {error}")
                })?;
                exports.push(ManifestSymbol {
                    elf_name: (*name).into(),
                    interface,
                    signature: (*signature).into(),
                    minimum_abi: abi_version,
                    maximum_abi: abi_version,
                    flags: parse_symbol_kind(symbol_kind, line_number)?,
                });
            }
            _ => return Err(line_error(line_number, "invalid directive")),
        }
    }
    if !saw_header {
        return Err("manifest is empty".into());
    }
    let manifest = AbiManifest {
        package: package.ok_or_else(|| String::from("manifest has no package"))?,
        kind: kind.ok_or_else(|| String::from("manifest has no kind"))?,
        interface: canonical_interface,
        abi_version,
        imports,
        exports,
        dependencies,
        runtime_abi_minimum,
        runtime_abi_maximum,
        lifecycle: requested_lifecycle,
        flags,
        version,
        metadata,
        icons,
        resources,
        capabilities,
        schema_source: source.as_bytes().to_vec(),
    };
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Разбирает canonical C ABI signature без догадок о Rust ABI.
pub fn parse_function_signature(source: &str) -> Result<FunctionSignature, String> {
    if source.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err("whitespace is not canonical".into());
    }
    let (head, result) = source
        .rsplit_once("->")
        .ok_or_else(|| String::from("missing `->`"))?;
    let open = head.find('(').ok_or_else(|| String::from("missing `(`"))?;
    if !head.ends_with(')') || head[..open].is_empty() {
        return Err("invalid function name or parameter list".into());
    }
    let name = &head[..open];
    validate_identifier(name)?;
    let raw_parameters = &head[open + 1..head.len() - 1];
    let mut parameters = Vec::new();
    if !raw_parameters.is_empty() {
        for parameter in raw_parameters.split(',') {
            let parsed = parse_type(parameter)?;
            if parsed == AbiType::Void {
                return Err("void parameter is not allowed".into());
            }
            parameters.push(parsed);
        }
    }
    Ok(FunctionSignature {
        name: name.into(),
        parameters,
        result: parse_type(result)?,
    })
}

fn parse_type(value: &str) -> Result<AbiType, String> {
    let scalar = match value {
        "void" => return Ok(AbiType::Void),
        "u8" => ScalarType::U8,
        "u16" => ScalarType::U16,
        "u32" => ScalarType::U32,
        "u64" => ScalarType::U64,
        "u128" => ScalarType::U128,
        "i8" => ScalarType::I8,
        "i16" => ScalarType::I16,
        "i32" => ScalarType::I32,
        "i64" => ScalarType::I64,
        "i128" => ScalarType::I128,
        "usize" => ScalarType::Usize,
        "isize" => ScalarType::Isize,
        "f32" => ScalarType::F32,
        "f64" => ScalarType::F64,
        value => {
            let (mutable, pointee) = if let Some(pointee) = value.strip_prefix("*mut_") {
                (true, pointee)
            } else if let Some(pointee) = value.strip_prefix("*const_") {
                (false, pointee)
            } else {
                return Err(format!("unsupported ABI type `{value}`"));
            };
            validate_identifier(pointee)?;
            return Ok(AbiType::Pointer {
                mutable,
                pointee: pointee.into(),
            });
        }
    };
    Ok(AbiType::Scalar(scalar))
}

fn validate_manifest(manifest: &AbiManifest) -> Result<(), String> {
    if manifest.package.is_empty() || !manifest.package.is_ascii() {
        return Err("package must be a non-empty ASCII canonical name".into());
    }
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
    for (index, dependency) in manifest.dependencies.iter().enumerate() {
        if dependency.minimum_abi > dependency.maximum_abi
            || dependency.file_name.is_empty()
            || dependency.file_name.as_bytes().contains(&b'/')
            || manifest.dependencies[..index].iter().any(|previous| {
                previous.interface == dependency.interface
                    && previous.minimum_abi == dependency.minimum_abi
                    && previous.maximum_abi == dependency.maximum_abi
                    && previous.package == dependency.package
            })
        {
            return Err(format!(
                "dependency {} is invalid or duplicated",
                dependency.file_name
            ));
        }
    }
    let actual = manifest
        .lifecycle
        .unwrap_or_else(|| manifest.kind.default_lifecycle());
    let lifecycle_matches = match manifest.kind {
        ArtifactKind::Application => {
            matches!(
                actual,
                lifecycle::MULTI_INSTANCE | lifecycle::SINGLE_INSTANCE
            )
        }
        ArtifactKind::Library => actual == lifecycle::IN_PROCESS_LIBRARY,
        ArtifactKind::Service => actual == lifecycle::MANAGED_SERVICE,
        ArtifactKind::Driver => actual == lifecycle::MANAGED_DRIVER,
    };
    if !lifecycle_matches {
        return Err("manifest lifecycle does not match artifact kind".into());
    }
    if manifest.flags & manifest_flags::BACKGROUND_ALLOWED != 0
        && manifest.kind != ArtifactKind::Application
    {
        return Err("background-allowed is valid only for applications".into());
    }
    for (index, capability) in manifest.capabilities.iter().enumerate() {
        if manifest.capabilities[..index]
            .iter()
            .any(|previous| previous.slot_hint == capability.slot_hint)
        {
            return Err(format!(
                "capability slot {} is declared more than once",
                capability.slot_hint
            ));
        }
    }
    Ok(())
}

fn fields(line: &str, line_number: usize) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            field.push(match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '"' => '"',
                '\\' => '\\',
                _ => return Err(line_error(line_number, "unsupported escape")),
            });
            escaped = false;
        } else if quoted && character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if !quoted && character == '#' {
            break;
        } else if !quoted && character.is_ascii_whitespace() {
            if !field.is_empty() {
                fields.push(core::mem::take(&mut field));
            }
        } else {
            field.push(character);
        }
    }
    if quoted || escaped {
        return Err(line_error(line_number, "unterminated quoted value"));
    }
    if !field.is_empty() {
        fields.push(field);
    }
    Ok(fields)
}

fn validate_identifier(value: &str) -> Result<(), String> {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err("empty identifier".into());
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!("invalid identifier `{value}`"));
    }
    Ok(())
}

fn simple_metadata(key: u16, value: &str) -> ManifestMetadata {
    ManifestMetadata {
        key,
        name: String::new(),
        locale: String::new(),
        value: value.into(),
    }
}

fn parse_abi(value: &str, line: usize) -> Result<u16, String> {
    value
        .parse::<u16>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| line_error(line, "ABI version must be 1..65535"))
}

fn parse_nonzero_u16(value: &str, line: usize, name: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| line_error(line, &format!("invalid {name}")))
}

fn parse_u32(value: &str, line: usize, name: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| line_error(line, &format!("invalid {name}")))
}

fn parse_u64(value: &str, line: usize, name: &str) -> Result<u64, String> {
    value
        .strip_prefix("0x")
        .map(|hex| u64::from_str_radix(hex, 16))
        .unwrap_or_else(|| value.parse::<u64>())
        .map_err(|_| line_error(line, &format!("invalid {name}")))
}

fn parse_capability_policy(value: &str, line: usize) -> Result<u32, String> {
    match value {
        "required" => Ok(capability_flags::REQUIRED),
        "optional" => Ok(capability_flags::OPTIONAL),
        "required-many" => Ok(capability_flags::REQUIRED | capability_flags::MULTIPLE),
        "optional-many" => Ok(capability_flags::OPTIONAL | capability_flags::MULTIPLE),
        _ => Err(line_error(line, "unknown capability policy")),
    }
}

fn parse_locale(value: &str, line: usize) -> Result<String, String> {
    if value == "default" {
        return Ok(String::new());
    }
    if value.len() > 35
        || !value.is_ascii()
        || value
            .split('-')
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    {
        return Err(line_error(line, "invalid BCP 47 locale"));
    }
    Ok(value.into())
}

fn parse_lifecycle(value: &str, line: usize) -> Result<u16, String> {
    match value {
        "multi-instance" => Ok(lifecycle::MULTI_INSTANCE),
        "single-instance" => Ok(lifecycle::SINGLE_INSTANCE),
        "managed-service" => Ok(lifecycle::MANAGED_SERVICE),
        "managed-driver" => Ok(lifecycle::MANAGED_DRIVER),
        "in-process-library" => Ok(lifecycle::IN_PROCESS_LIBRARY),
        _ => Err(line_error(line, "unknown lifecycle")),
    }
}

fn parse_icon_format(value: &str, line: usize) -> Result<u16, String> {
    match value {
        "rgba8" => Ok(icon_format::RGBA8_PREMULTIPLIED),
        "png" => Ok(icon_format::PNG),
        "svg" => Ok(icon_format::SVG_UTF8),
        _ => Err(line_error(line, "unknown icon format")),
    }
}

fn parse_icon_theme(value: &str, line: usize) -> Result<u16, String> {
    match value {
        "any" => Ok(icon_theme::ANY),
        "light" => Ok(icon_theme::LIGHT),
        "dark" => Ok(icon_theme::DARK),
        "high-contrast" => Ok(icon_theme::HIGH_CONTRAST),
        _ => Err(line_error(line, "unknown icon theme")),
    }
}

fn parse_icon_purpose(value: &str, line: usize) -> Result<u16, String> {
    match value {
        "application" => Ok(icon_purpose::APPLICATION),
        "badge" => Ok(icon_purpose::SMALL_BADGE),
        "document" => Ok(icon_purpose::DOCUMENT),
        _ => Err(line_error(line, "unknown icon purpose")),
    }
}

fn validate_resource_name(value: &str, line: usize) -> Result<(), String> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(line_error(
            line,
            "resource name must be a relative canonical path",
        ));
    }
    Ok(())
}

fn validate_content_type(value: &str, line: usize) -> Result<(), String> {
    let Some((kind, subtype)) = value.split_once('/') else {
        return Err(line_error(line, "invalid content type"));
    };
    if kind.is_empty()
        || subtype.is_empty()
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(line_error(line, "invalid content type"));
    }
    Ok(())
}

fn parse_symbol_kind(value: &str, line: usize) -> Result<u32, String> {
    match value {
        "function" => Ok(import_flags::FUNCTION),
        "data" => Ok(import_flags::DATA),
        "tls" => Ok(import_flags::TLS),
        _ => Err(line_error(
            line,
            "symbol kind must be function, data or tls",
        )),
    }
}

fn line_error(line: usize, message: &str) -> String {
    format!("manifest line {line}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utf8_manifest_and_function_contracts() {
        let manifest = parse_manifest(
            r#"
RUNE-ABI 1
package org.rustos.example
kind library
interface org.rustos.example/1
abi 2
name ru-RU "Пример"
export answer answer(u64,*const_u8)->i32 function
"#,
        )
        .unwrap();
        assert_eq!(manifest.metadata[0].value, "Пример");
        let signature = parse_function_signature(&manifest.exports[0].signature).unwrap();
        assert_eq!(signature.name, "answer");
        assert!(matches!(
            signature.parameters[1],
            AbiType::Pointer { mutable: false, .. }
        ));
    }

    #[test]
    fn rejects_noncanonical_and_unbound_imports() {
        assert!(parse_function_signature("f( u64)->u64").is_err());
        assert!(parse_manifest(
            "RUNE-ABI 1\npackage x\nkind application\nimport x org.x/1 x()->u64 1 1 function\n"
        )
        .is_err());
    }

    #[test]
    fn dependency_can_pin_a_canonical_package() {
        let manifest = parse_manifest(
            "RUNE-ABI 1\npackage app\nkind application\ndependency math.rune org.math/1 1 2 org.math.reference\n",
        )
        .unwrap();
        assert_eq!(
            manifest.dependencies[0].package,
            Some(package_id("org.math.reference"))
        );
    }
}

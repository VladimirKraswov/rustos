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

/// Непрозрачный ABI-тип с известными размером и выравниванием. Generated raw
/// crate резервирует точное место, но не раскрывает private поля provider'а.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestOpaque {
    pub name: String,
    pub size: u32,
    pub alignment: u16,
}

/// Открытая `#[repr(C)]` структура. Offset каждого поля задаётся явно, чтобы
/// generator не зависел от layout Rust-компилятора.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestStruct {
    pub name: String,
    pub size: u32,
    pub alignment: u16,
    pub fields: Vec<ManifestField>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestField {
    pub name: String,
    pub scalar: ScalarType,
    pub offset: u32,
    pub count: u32,
}

/// Ошибка остаётся newtype над integer, а не Rust enum: неизвестный будущий
/// код ABI нельзя превращать в недопустимый discriminant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestErrorSet {
    pub name: String,
    pub repr: ScalarType,
    pub success: i64,
    pub contract_violation: i64,
    pub cases: Vec<ManifestErrorCase>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestErrorCase {
    pub name: String,
    pub value: i64,
}

/// Linear handle не получает `Copy`/`Clone` в safe facade. `consume`-контракт
/// действительно перемещает Rust-значение и не позволяет закрыть его дважды.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestHandle {
    pub name: String,
    pub repr: ScalarType,
    pub invalid: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerContractKind {
    BorrowShared,
    BorrowExclusive,
    SliceIn,
    SliceOut,
    Out,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SliceEncoding {
    Bytes,
    Utf8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestPointerContract {
    pub function: String,
    pub parameter: u16,
    pub kind: PointerContractKind,
    /// Для borrow/out это ABI type; для slice поле пустое.
    pub type_name: String,
    pub length_parameter: Option<u16>,
    pub encoding: SliceEncoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleUseKind {
    Borrow,
    /// Владение передаётся provider'у в момент вызова независимо от status.
    /// Поэтому safe wrapper не может повторно использовать handle при ошибке.
    Consume,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestHandleUse {
    pub function: String,
    pub parameter: u16,
    pub handle: String,
    pub kind: HandleUseKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestBound {
    pub function: String,
    pub output_parameter: u16,
    pub maximum_parameter: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestResultContract {
    pub function: String,
    pub errors: String,
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
    pub opaque_types: Vec<ManifestOpaque>,
    pub structs: Vec<ManifestStruct>,
    pub error_sets: Vec<ManifestErrorSet>,
    pub handles: Vec<ManifestHandle>,
    pub pointer_contracts: Vec<ManifestPointerContract>,
    pub handle_uses: Vec<ManifestHandleUse>,
    pub bounds: Vec<ManifestBound>,
    pub result_contracts: Vec<ManifestResultContract>,
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

impl ScalarType {
    pub const fn byte_size(self, pointer_size: u32) -> u32 {
        match self {
            Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
            Self::U128 | Self::I128 => 16,
            Self::Usize | Self::Isize => pointer_size,
        }
    }

    pub const fn is_signed_integer(self) -> bool {
        matches!(
            self,
            Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::I128 | Self::Isize
        )
    }

    pub const fn is_unsigned_integer(self) -> bool {
        matches!(
            self,
            Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::U128 | Self::Usize
        )
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
    let mut opaque_types = Vec::new();
    let mut structs: Vec<ManifestStruct> = Vec::new();
    let mut error_sets: Vec<ManifestErrorSet> = Vec::new();
    let mut handles = Vec::new();
    let mut pointer_contracts = Vec::new();
    let mut handle_uses = Vec::new();
    let mut bounds = Vec::new();
    let mut result_contracts = Vec::new();
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
            ["opaque", name, size, alignment] => opaque_types.push(ManifestOpaque {
                name: parse_type_name(name, line_number)?,
                size: parse_nonzero_u32(size, line_number, "opaque size")?,
                alignment: parse_alignment(alignment, line_number)?,
            }),
            ["struct", name, size, alignment] => structs.push(ManifestStruct {
                name: parse_type_name(name, line_number)?,
                size: parse_nonzero_u32(size, line_number, "struct size")?,
                alignment: parse_alignment(alignment, line_number)?,
                fields: Vec::new(),
            }),
            ["field", owner, name, scalar, offset] => {
                push_field(&mut structs, owner, name, scalar, offset, "1", line_number)?;
            }
            ["field", owner, name, scalar, offset, count] => {
                push_field(
                    &mut structs,
                    owner,
                    name,
                    scalar,
                    offset,
                    count,
                    line_number,
                )?;
            }
            ["error-set", name, repr, success, contract] => {
                error_sets.push(ManifestErrorSet {
                    name: parse_type_name(name, line_number)?,
                    repr: parse_error_repr(repr, line_number)?,
                    success: parse_i64(success, line_number, "success status")?,
                    contract_violation: parse_i64(
                        contract,
                        line_number,
                        "contract violation status",
                    )?,
                    cases: Vec::new(),
                });
            }
            ["error", set, name, value] => {
                let errors = error_sets
                    .iter_mut()
                    .find(|errors| errors.name == *set)
                    .ok_or_else(|| line_error(line_number, "error-set must precede error"))?;
                validate_identifier(name).map_err(|error| line_error(line_number, &error))?;
                errors.cases.push(ManifestErrorCase {
                    name: (*name).into(),
                    value: parse_i64(value, line_number, "error value")?,
                });
            }
            ["handle", name, repr, invalid] => handles.push(ManifestHandle {
                name: parse_type_name(name, line_number)?,
                repr: parse_handle_repr(repr, line_number)?,
                invalid: parse_u64(invalid, line_number, "invalid handle")?,
            }),
            ["borrow", function, parameter, mode, type_name] => {
                validate_identifier(function).map_err(|error| line_error(line_number, &error))?;
                pointer_contracts.push(ManifestPointerContract {
                    function: (*function).into(),
                    parameter: parse_u16(parameter, line_number, "parameter index")?,
                    kind: match *mode {
                        "shared" => PointerContractKind::BorrowShared,
                        "exclusive" => PointerContractKind::BorrowExclusive,
                        _ => return Err(line_error(line_number, "unknown borrow mode")),
                    },
                    type_name: parse_type_name(type_name, line_number)?,
                    length_parameter: None,
                    encoding: SliceEncoding::Bytes,
                });
            }
            ["slice", function, pointer, length, direction, encoding] => {
                validate_identifier(function).map_err(|error| line_error(line_number, &error))?;
                let kind = match *direction {
                    "in" => PointerContractKind::SliceIn,
                    "out" => PointerContractKind::SliceOut,
                    _ => return Err(line_error(line_number, "unknown slice direction")),
                };
                let encoding = match *encoding {
                    "bytes" => SliceEncoding::Bytes,
                    "utf8" if kind == PointerContractKind::SliceIn => SliceEncoding::Utf8,
                    _ => return Err(line_error(line_number, "invalid slice encoding")),
                };
                pointer_contracts.push(ManifestPointerContract {
                    function: (*function).into(),
                    parameter: parse_u16(pointer, line_number, "pointer parameter")?,
                    kind,
                    type_name: String::new(),
                    length_parameter: Some(parse_u16(length, line_number, "length parameter")?),
                    encoding,
                });
            }
            ["out", function, parameter, type_name] => {
                validate_identifier(function).map_err(|error| line_error(line_number, &error))?;
                pointer_contracts.push(ManifestPointerContract {
                    function: (*function).into(),
                    parameter: parse_u16(parameter, line_number, "parameter index")?,
                    kind: PointerContractKind::Out,
                    type_name: parse_type_name(type_name, line_number)?,
                    length_parameter: None,
                    encoding: SliceEncoding::Bytes,
                });
            }
            ["handle-use", function, parameter, handle, mode] => {
                validate_identifier(function).map_err(|error| line_error(line_number, &error))?;
                handle_uses.push(ManifestHandleUse {
                    function: (*function).into(),
                    parameter: parse_u16(parameter, line_number, "parameter index")?,
                    handle: parse_type_name(handle, line_number)?,
                    kind: match *mode {
                        "borrow" => HandleUseKind::Borrow,
                        "consume" => HandleUseKind::Consume,
                        _ => return Err(line_error(line_number, "unknown handle use mode")),
                    },
                });
            }
            ["bound", function, output, maximum] => bounds.push(ManifestBound {
                function: parse_function_name(function, line_number)?,
                output_parameter: parse_u16(output, line_number, "output parameter")?,
                maximum_parameter: parse_u16(maximum, line_number, "maximum parameter")?,
            }),
            ["result", function, errors] => result_contracts.push(ManifestResultContract {
                function: parse_function_name(function, line_number)?,
                errors: parse_type_name(errors, line_number)?,
            }),
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
        opaque_types,
        structs,
        error_sets,
        handles,
        pointer_contracts,
        handle_uses,
        bounds,
        result_contracts,
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

fn parse_scalar(value: &str) -> Result<ScalarType, String> {
    match parse_type(value)? {
        AbiType::Scalar(scalar) => Ok(scalar),
        _ => Err(format!("`{value}` is not a scalar ABI type")),
    }
}

fn parse_type_name(value: &str, line: usize) -> Result<String, String> {
    validate_identifier(value).map_err(|error| line_error(line, &error))?;
    Ok(value.into())
}

fn parse_function_name(value: &str, line: usize) -> Result<String, String> {
    parse_type_name(value, line)
}

fn push_field(
    structs: &mut [ManifestStruct],
    owner: &str,
    name: &str,
    scalar: &str,
    offset: &str,
    count: &str,
    line: usize,
) -> Result<(), String> {
    validate_identifier(name).map_err(|error| line_error(line, &error))?;
    let structure = structs
        .iter_mut()
        .find(|structure| structure.name == owner)
        .ok_or_else(|| line_error(line, "struct must precede its fields"))?;
    structure.fields.push(ManifestField {
        name: name.into(),
        scalar: parse_scalar(scalar).map_err(|error| line_error(line, &error))?,
        offset: parse_u32(offset, line, "field offset")?,
        count: parse_nonzero_u32(count, line, "field count")?,
    });
    Ok(())
}

fn parse_error_repr(value: &str, line: usize) -> Result<ScalarType, String> {
    let scalar = parse_scalar(value).map_err(|error| line_error(line, &error))?;
    if !scalar.is_signed_integer() || matches!(scalar, ScalarType::I128) {
        return Err(line_error(line, "error repr must be i8/i16/i32/i64/isize"));
    }
    Ok(scalar)
}

fn parse_handle_repr(value: &str, line: usize) -> Result<ScalarType, String> {
    let scalar = parse_scalar(value).map_err(|error| line_error(line, &error))?;
    if !scalar.is_unsigned_integer() || matches!(scalar, ScalarType::U128) {
        return Err(line_error(line, "handle repr must be u8/u16/u32/u64/usize"));
    }
    Ok(scalar)
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
    validate_declared_types(manifest)?;
    validate_function_contracts(manifest)?;
    Ok(())
}

fn validate_declared_types(manifest: &AbiManifest) -> Result<(), String> {
    let mut names: Vec<&str> = Vec::new();
    for opaque in &manifest.opaque_types {
        validate_unique_type_name(&mut names, &opaque.name)?;
        if opaque.size % u32::from(opaque.alignment) != 0 {
            return Err(format!(
                "opaque {} size is not a multiple of alignment",
                opaque.name
            ));
        }
    }
    for structure in &manifest.structs {
        validate_unique_type_name(&mut names, &structure.name)?;
        if structure.size % u32::from(structure.alignment) != 0 {
            return Err(format!(
                "struct {} size is not a multiple of alignment",
                structure.name
            ));
        }
        for (index, field) in structure.fields.iter().enumerate() {
            if structure.fields[..index]
                .iter()
                .any(|previous| previous.name == field.name)
            {
                return Err(format!(
                    "struct {} has duplicate field {}",
                    structure.name, field.name
                ));
            }
            let element_size = field.scalar.byte_size(8);
            let field_size = element_size
                .checked_mul(field.count)
                .ok_or_else(|| format!("struct {} field size overflow", structure.name))?;
            let end = field
                .offset
                .checked_add(field_size)
                .ok_or_else(|| format!("struct {} field offset overflow", structure.name))?;
            if end > structure.size
                || !field.offset.is_multiple_of(element_size)
                || u32::from(structure.alignment) < element_size
            {
                return Err(format!(
                    "struct {} field {} is outside or misaligned",
                    structure.name, field.name
                ));
            }
            for previous in &structure.fields[..index] {
                let previous_end = previous.offset + previous.scalar.byte_size(8) * previous.count;
                if field.offset < previous_end && previous.offset < end {
                    return Err(format!(
                        "struct {} fields {} and {} overlap",
                        structure.name, previous.name, field.name
                    ));
                }
            }
        }
    }
    for handle in &manifest.handles {
        validate_unique_type_name(&mut names, &handle.name)?;
        if handle.invalid > scalar_unsigned_max(handle.repr) {
            return Err(format!(
                "handle {} invalid value does not fit repr",
                handle.name
            ));
        }
    }
    for errors in &manifest.error_sets {
        validate_unique_type_name(&mut names, &errors.name)?;
        if !scalar_signed_fits(errors.repr, errors.success)
            || !scalar_signed_fits(errors.repr, errors.contract_violation)
            || errors.success == errors.contract_violation
        {
            return Err(format!(
                "error-set {} has invalid sentinel values",
                errors.name
            ));
        }
        for (index, case) in errors.cases.iter().enumerate() {
            if !scalar_signed_fits(errors.repr, case.value)
                || case.value == errors.success
                || case.value == errors.contract_violation
                || errors.cases[..index]
                    .iter()
                    .any(|previous| previous.name == case.name || previous.value == case.value)
            {
                return Err(format!(
                    "error-set {} has invalid or duplicate case {}",
                    errors.name, case.name
                ));
            }
        }
    }
    Ok(())
}

fn validate_unique_type_name<'a>(names: &mut Vec<&'a str>, name: &'a str) -> Result<(), String> {
    if names.contains(&name) {
        return Err(format!("ABI type `{name}` is declared more than once"));
    }
    names.push(name);
    Ok(())
}

fn scalar_unsigned_max(scalar: ScalarType) -> u64 {
    match scalar {
        ScalarType::U8 => u8::MAX.into(),
        ScalarType::U16 => u16::MAX.into(),
        ScalarType::U32 => u32::MAX.into(),
        ScalarType::U64 | ScalarType::Usize => u64::MAX,
        _ => 0,
    }
}

fn scalar_signed_fits(scalar: ScalarType, value: i64) -> bool {
    match scalar {
        ScalarType::I8 => i8::try_from(value).is_ok(),
        ScalarType::I16 => i16::try_from(value).is_ok(),
        ScalarType::I32 => i32::try_from(value).is_ok(),
        ScalarType::I64 | ScalarType::Isize => true,
        _ => false,
    }
}

fn validate_function_contracts(manifest: &AbiManifest) -> Result<(), String> {
    let mut signatures = Vec::new();
    for export in &manifest.exports {
        let signature = parse_function_signature(&export.signature)?;
        if signatures
            .iter()
            .any(|previous: &FunctionSignature| previous.name == signature.name)
        {
            return Err(format!(
                "safe ABI function name {} is overloaded",
                signature.name
            ));
        }
        signatures.push(signature);
    }
    for (index, contract) in manifest.pointer_contracts.iter().enumerate() {
        if manifest.pointer_contracts[..index].iter().any(|previous| {
            previous.function == contract.function && previous.parameter == contract.parameter
        }) {
            return Err(format!(
                "pointer parameter {}:{} has several contracts",
                contract.function, contract.parameter
            ));
        }
        validate_pointer_contract(manifest, &signatures, contract)?;
        if let Some(length) = contract.length_parameter {
            if manifest.pointer_contracts[..index].iter().any(|previous| {
                previous.function == contract.function && previous.length_parameter == Some(length)
            }) {
                return Err(format!(
                    "slice length parameter {}:{} has several owners",
                    contract.function, length
                ));
            }
        }
    }
    for (index, usage) in manifest.handle_uses.iter().enumerate() {
        if manifest.handle_uses[..index].iter().any(|previous| {
            previous.function == usage.function && previous.parameter == usage.parameter
        }) {
            return Err(format!(
                "handle parameter {}:{} has several contracts",
                usage.function, usage.parameter
            ));
        }
        let signature = function_signature(&signatures, &usage.function)?;
        let raw = signature
            .parameters
            .get(usize::from(usage.parameter))
            .ok_or_else(|| format!("handle parameter {} is out of range", usage.parameter))?;
        let handle = manifest
            .handles
            .iter()
            .find(|handle| handle.name == usage.handle)
            .ok_or_else(|| format!("unknown handle type {}", usage.handle))?;
        if raw != &AbiType::Scalar(handle.repr) {
            return Err(format!(
                "handle {} does not match {} parameter {}",
                usage.handle, usage.function, usage.parameter
            ));
        }
    }
    for (index, result) in manifest.result_contracts.iter().enumerate() {
        if manifest.result_contracts[..index]
            .iter()
            .any(|previous| previous.function == result.function)
        {
            return Err(format!(
                "function {} has several result contracts",
                result.function
            ));
        }
        let signature = function_signature(&signatures, &result.function)?;
        let errors = manifest
            .error_sets
            .iter()
            .find(|errors| errors.name == result.errors)
            .ok_or_else(|| format!("unknown error-set {}", result.errors))?;
        if signature.result != AbiType::Scalar(errors.repr) {
            return Err(format!(
                "function {} result does not match error-set {}",
                result.function, result.errors
            ));
        }
        for (parameter, abi_type) in signature.parameters.iter().enumerate() {
            if matches!(abi_type, AbiType::Pointer { .. })
                && !manifest.pointer_contracts.iter().any(|contract| {
                    contract.function == result.function
                        && usize::from(contract.parameter) == parameter
                })
            {
                return Err(format!(
                    "safe function {} pointer parameter {} has no ownership contract",
                    result.function, parameter
                ));
            }
        }
    }
    for bound in &manifest.bounds {
        let signature = function_signature(&signatures, &bound.function)?;
        let output = manifest.pointer_contracts.iter().find(|contract| {
            contract.function == bound.function
                && contract.parameter == bound.output_parameter
                && contract.kind == PointerContractKind::Out
        });
        let maximum = manifest.pointer_contracts.iter().find(|contract| {
            contract.function == bound.function
                && contract.length_parameter == Some(bound.maximum_parameter)
                && matches!(
                    contract.kind,
                    PointerContractKind::SliceIn | PointerContractKind::SliceOut
                )
        });
        let Some(output) = output else {
            return Err(format!(
                "bound {} does not name an out parameter",
                bound.function
            ));
        };
        if maximum.is_none()
            || resolved_named_type(manifest, &output.type_name)
                != Some(AbiType::Scalar(ScalarType::Usize))
            || signature
                .parameters
                .get(usize::from(bound.maximum_parameter))
                != Some(&AbiType::Scalar(ScalarType::Usize))
        {
            return Err(format!(
                "bound {} is not a usize slice bound",
                bound.function
            ));
        }
    }
    for (index, bound) in manifest.bounds.iter().enumerate() {
        if manifest.bounds[..index].iter().any(|previous| {
            previous.function == bound.function
                && previous.output_parameter == bound.output_parameter
        }) {
            return Err(format!(
                "output {}:{} has several bounds",
                bound.function, bound.output_parameter
            ));
        }
    }
    Ok(())
}

fn function_signature<'a>(
    signatures: &'a [FunctionSignature],
    name: &str,
) -> Result<&'a FunctionSignature, String> {
    signatures
        .iter()
        .find(|signature| signature.name == name)
        .ok_or_else(|| format!("contract references unknown function {name}"))
}

fn validate_pointer_contract(
    manifest: &AbiManifest,
    signatures: &[FunctionSignature],
    contract: &ManifestPointerContract,
) -> Result<(), String> {
    let signature = function_signature(signatures, &contract.function)?;
    let raw = signature
        .parameters
        .get(usize::from(contract.parameter))
        .ok_or_else(|| format!("pointer parameter {} is out of range", contract.parameter))?;
    let AbiType::Pointer { mutable, pointee } = raw else {
        return Err(format!(
            "{} parameter {} is not a pointer",
            contract.function, contract.parameter
        ));
    };
    match contract.kind {
        PointerContractKind::BorrowShared | PointerContractKind::BorrowExclusive => {
            if contract.kind == PointerContractKind::BorrowExclusive && !mutable {
                return Err(format!("{} exclusive borrow is const", contract.function));
            }
            if contract.kind == PointerContractKind::BorrowShared && *mutable {
                return Err(format!("{} shared borrow is mutable", contract.function));
            }
            if pointee != &contract.type_name
                || !manifest
                    .opaque_types
                    .iter()
                    .any(|item| item.name == contract.type_name)
                    && !manifest
                        .structs
                        .iter()
                        .any(|item| item.name == contract.type_name)
            {
                return Err(format!(
                    "{} borrow type {} is not declared or mismatched",
                    contract.function, contract.type_name
                ));
            }
        }
        PointerContractKind::SliceIn | PointerContractKind::SliceOut => {
            if pointee != "u8" || contract.kind == PointerContractKind::SliceOut && !mutable {
                return Err(format!(
                    "{} has invalid byte slice pointer",
                    contract.function
                ));
            }
            let length = contract
                .length_parameter
                .and_then(|index| signature.parameters.get(usize::from(index)));
            if length != Some(&AbiType::Scalar(ScalarType::Usize)) {
                return Err(format!("{} slice length is not usize", contract.function));
            }
        }
        PointerContractKind::Out => {
            if !mutable || !named_type_matches_pointee(manifest, &contract.type_name, pointee) {
                return Err(format!(
                    "{} out type {} does not match pointer",
                    contract.function, contract.type_name
                ));
            }
        }
    }
    Ok(())
}

/// Возвращает pointer-shaped представление для сопоставления named out type с
/// raw `*mut T`: struct/opaque используют своё имя, handle — integer repr.
fn resolved_named_type(manifest: &AbiManifest, name: &str) -> Option<AbiType> {
    if let Ok(scalar) = parse_scalar(name) {
        return Some(AbiType::Scalar(scalar));
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
        return Some(AbiType::Pointer {
            mutable: true,
            pointee: name.into(),
        });
    }
    manifest
        .handles
        .iter()
        .find(|handle| handle.name == name)
        .map(|handle| AbiType::Scalar(handle.repr))
}

fn named_type_matches_pointee(manifest: &AbiManifest, name: &str, pointee: &str) -> bool {
    if let Ok(scalar) = parse_scalar(name) {
        return scalar_name(scalar) == pointee;
    }
    if let Some(handle) = manifest.handles.iter().find(|handle| handle.name == name) {
        return scalar_name(handle.repr) == pointee;
    }
    name == pointee
        && (manifest
            .opaque_types
            .iter()
            .any(|opaque| opaque.name == name)
            || manifest
                .structs
                .iter()
                .any(|structure| structure.name == name))
}

pub const fn scalar_name(scalar: ScalarType) -> &'static str {
    match scalar {
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

fn parse_u16(value: &str, line: usize, name: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| line_error(line, &format!("invalid {name}")))
}

fn parse_alignment(value: &str, line: usize) -> Result<u16, String> {
    parse_nonzero_u16(value, line, "alignment").and_then(|alignment| {
        if alignment.is_power_of_two() && alignment <= 4096 {
            Ok(alignment)
        } else {
            Err(line_error(
                line,
                "alignment must be a power of two up to 4096",
            ))
        }
    })
}

fn parse_u32(value: &str, line: usize, name: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| line_error(line, &format!("invalid {name}")))
}

fn parse_nonzero_u32(value: &str, line: usize, name: &str) -> Result<u32, String> {
    parse_u32(value, line, name).and_then(|value| {
        (value != 0)
            .then_some(value)
            .ok_or_else(|| line_error(line, &format!("{name} must be non-zero")))
    })
}

fn parse_i64(value: &str, line: usize, name: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
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

    #[test]
    fn validates_complete_layout_ownership_and_error_contract() {
        let manifest = parse_manifest(
            r#"
RUNE-ABI 1
package org.rustos.safe
kind library
interface org.rustos.safe/1
opaque context 32 8
struct record 16 8
field record id u64 0
field record flags u32 8
handle object u64 18446744073709551615
error-set io_error i32 0 -2147483648
error io_error FAILED -1
export call call(*mut_context,*const_u8,usize,u64,*mut_record)->i32 function
borrow call 0 exclusive context
slice call 1 2 in utf8
handle-use call 3 object borrow
out call 4 record
result call io_error
"#,
        )
        .unwrap();
        assert_eq!(manifest.opaque_types[0].size, 32);
        assert_eq!(manifest.structs[0].fields[1].offset, 8);
        assert_eq!(manifest.pointer_contracts.len(), 3);
        assert_eq!(manifest.handle_uses.len(), 1);
    }

    #[test]
    fn rejects_overlapping_layout_and_incomplete_pointer_contract() {
        let overlap = r#"
RUNE-ABI 1
package org.rustos.bad-layout
kind library
interface org.rustos.bad-layout/1
struct record 16 8
field record first u64 0
field record second u32 4
"#;
        assert!(parse_manifest(overlap).unwrap_err().contains("overlap"));

        let missing = r#"
RUNE-ABI 1
package org.rustos.bad-pointer
kind library
interface org.rustos.bad-pointer/1
error-set call_error i32 0 -2147483648
export call call(*mut_u8)->i32 function
result call call_error
"#;
        assert!(parse_manifest(missing)
            .unwrap_err()
            .contains("has no ownership contract"));
    }

    #[test]
    fn rejects_const_output_and_error_sentinel_collision() {
        let const_output = r#"
RUNE-ABI 1
package org.rustos.bad-output
kind library
interface org.rustos.bad-output/1
error-set call_error i32 0 -2147483648
export call call(*const_u64)->i32 function
out call 0 u64
result call call_error
"#;
        assert!(parse_manifest(const_output)
            .unwrap_err()
            .contains("does not match pointer"));

        let collision = r#"
RUNE-ABI 1
package org.rustos.bad-error
kind library
interface org.rustos.bad-error/1
error-set call_error i32 0 -1
error call_error COLLISION -1
"#;
        assert!(parse_manifest(collision)
            .unwrap_err()
            .contains("invalid or duplicate"));
    }

    #[test]
    fn rejects_ambiguous_slice_length_and_mutable_shared_borrow() {
        let shared_length = r#"
RUNE-ABI 1
package org.rustos.bad-slices
kind library
interface org.rustos.bad-slices/1
error-set call_error i32 0 -2147483648
export call call(*const_u8,*const_u8,usize)->i32 function
slice call 0 2 in bytes
slice call 1 2 in bytes
result call call_error
"#;
        assert!(parse_manifest(shared_length)
            .unwrap_err()
            .contains("several owners"));

        let mutable_shared = r#"
RUNE-ABI 1
package org.rustos.bad-borrow
kind library
interface org.rustos.bad-borrow/1
opaque context 8 8
error-set call_error i32 0 -2147483648
export call call(*mut_context)->i32 function
borrow call 0 shared context
result call call_error
"#;
        assert!(parse_manifest(mutable_shared)
            .unwrap_err()
            .contains("shared borrow is mutable"));
    }
}

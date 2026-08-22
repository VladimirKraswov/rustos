//! Host-side builder и атомарный activator package registry.
//!
//! Важная граница: ни одна запись не становится `current`, пока проверены не
//! только подпись индекса, но и каждый RUNE object, на который он ссылается.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use ed25519_dalek::{Signer, SigningKey};
use rustos_package_registry::{
    key_id, registry_flags, signature_message, trust_flags, validate_registry_path, Registry,
    TrustedKey, ENTRY_SIZE, FORMAT_VERSION, HEADER_SIZE, MAGIC, MAX_REGISTRY_SIZE, PAYLOAD_OFFSET,
    SIGNATURE_OFFSET, SIGNATURE_SIZE,
};
use rustos_rune_format::{sha256, Container};

const DEVELOPMENT_KEY_DOMAIN: &[u8] = b"RustOS DEVELOPMENT package registry key v1";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn main() {
    if let Err(error) = run() {
        eprintln!("rustos-package: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(usage());
    };
    match command {
        "build" => build_command(&arguments[1..]),
        "verify" => verify_command(&arguments[1..]),
        "activate" => activate_command(&arguments[1..]),
        "development-key-info" => development_key_info(&arguments[1..]),
        "--help" | "-h" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        _ => Err(usage()),
    }
}

fn development_key_info(arguments: &[String]) -> Result<(), String> {
    if !arguments.is_empty() {
        return Err("development-key-info takes no arguments".into());
    }
    let public = development_signing_key().verifying_key().to_bytes();
    println!(
        "public-key={}\nkey-id={}",
        hex(&public),
        hex(&key_id(&public))
    );
    Ok(())
}

#[derive(Default)]
struct Options {
    generation: Option<u64>,
    minimum_generation: u64,
    output: Option<PathBuf>,
    store: Option<PathBuf>,
    development_key: bool,
    secret_key_file: Option<PathBuf>,
    public_key_file: Option<PathBuf>,
    positionals: Vec<String>,
}

fn parse_options(arguments: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0usize;
    while index < arguments.len() {
        let value = &arguments[index];
        let next = |name: &str, index: &mut usize| -> Result<&str, String> {
            *index += 1;
            arguments
                .get(*index)
                .map(String::as_str)
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match value.as_str() {
            "--generation" => {
                options.generation = Some(
                    next("--generation", &mut index)?
                        .parse()
                        .map_err(|_| String::from("invalid generation"))?,
                );
            }
            "--minimum-generation" => {
                options.minimum_generation = next("--minimum-generation", &mut index)?
                    .parse()
                    .map_err(|_| String::from("invalid minimum generation"))?;
            }
            "--output" => options.output = Some(next("--output", &mut index)?.into()),
            "--store" => options.store = Some(next("--store", &mut index)?.into()),
            "--development-key" => options.development_key = true,
            "--secret-key-file" => {
                options.secret_key_file = Some(next("--secret-key-file", &mut index)?.into());
            }
            "--public-key-file" => {
                options.public_key_file = Some(next("--public-key-file", &mut index)?.into());
            }
            _ if value.starts_with('-') => return Err(format!("unknown option {value}")),
            _ => options.positionals.push(value.clone()),
        }
        index += 1;
    }
    Ok(options)
}

fn build_command(arguments: &[String]) -> Result<(), String> {
    let options = parse_options(arguments)?;
    let generation = options
        .generation
        .filter(|value| *value != 0)
        .ok_or_else(|| String::from("build requires non-zero --generation"))?;
    let output = options
        .output
        .as_deref()
        .ok_or_else(|| String::from("build requires --output"))?;
    let (signing, development) = signing_key(&options)?;
    let packages = load_packages(&options.positionals)?;
    let bytes = build_registry(generation, &signing, development, &packages)?;
    atomic_write(output, &bytes)?;
    let public = signing.verifying_key().to_bytes();
    println!(
        "registry={} generation={} entries={} key-id={}",
        output.display(),
        generation,
        packages.len(),
        hex(&key_id(&public))
    );
    Ok(())
}

fn verify_command(arguments: &[String]) -> Result<(), String> {
    let options = parse_options(arguments)?;
    let registry_path = options
        .positionals
        .first()
        .ok_or_else(|| String::from("verify requires registry path"))?;
    let trust = trusted_key(&options)?;
    let bytes = read_bounded(Path::new(registry_path), MAX_REGISTRY_SIZE as u64)?;
    let registry = Registry::verify(&bytes, &[trust], options.minimum_generation)
        .map_err(|error| format!("registry verification failed: {error:?}"))?;
    if options.positionals.len() > 1 {
        let packages = load_packages(&options.positionals[1..])?;
        verify_package_set(&registry, &packages)?;
    }
    println!(
        "verified generation={} entries={} payload={}",
        registry.header().generation,
        registry.header().entry_count,
        hex(&registry.header().payload_hash)
    );
    Ok(())
}

fn activate_command(arguments: &[String]) -> Result<(), String> {
    let options = parse_options(arguments)?;
    let store = options
        .store
        .as_deref()
        .ok_or_else(|| String::from("activate requires --store"))?;
    let registry_path = options
        .positionals
        .first()
        .ok_or_else(|| String::from("activate requires registry path"))?;
    let packages = load_packages(&options.positionals[1..])?;
    let trust = trusted_key(&options)?;
    let bytes = read_bounded(Path::new(registry_path), MAX_REGISTRY_SIZE as u64)?;

    fs::create_dir_all(store.join("objects")).map_err(io_error)?;
    fs::create_dir_all(store.join("indexes")).map_err(io_error)?;
    let current = read_current_registry(store, &trust)?;
    let required = options
        .minimum_generation
        .max(current.map_or(0, |current| current.0));
    let registry = Registry::verify(&bytes, &[trust], required)
        .map_err(|error| format!("registry verification failed: {error:?}"))?;
    if current.is_some_and(|(generation, payload)| {
        registry.header().payload_hash != payload && registry.header().generation <= generation
    }) {
        return Err("registry generation does not advance current package set".into());
    }
    verify_package_set(&registry, &packages)?;

    // Все content-addressed objects фиксируются до смены единственного
    // маленького указателя `current`. Сбой в любой точке сохраняет прежний set.
    for package in &packages {
        let entry = registry
            .find_path(&package.registry_path)
            .ok_or_else(|| format!("{} is absent from registry", package.registry_path))?;
        let object = store
            .join("objects")
            .join(format!("{}.rune", hex(&entry.content_hash)));
        write_content_addressed(&object, &package.bytes)?;
    }
    let index_name = format!("{}.ridx", hex(&registry.header().payload_hash));
    let index_path = store.join("indexes").join(&index_name);
    write_content_addressed(&index_path, &bytes)?;
    let pointer = format!(
        "RUSTOS-REGISTRY 1\n{}\n{}\n",
        registry.header().generation,
        index_name
    );
    atomic_write(&store.join("current"), pointer.as_bytes())?;
    sync_directory(store)?;
    println!(
        "activated generation={} entries={} store={}",
        registry.header().generation,
        registry.header().entry_count,
        store.display()
    );
    Ok(())
}

struct Package {
    registry_path: String,
    bytes: Vec<u8>,
}

fn load_packages(specifications: &[String]) -> Result<Vec<Package>, String> {
    let mut packages = Vec::with_capacity(specifications.len());
    for specification in specifications {
        let (registry_path, host_path) = specification
            .split_once('=')
            .ok_or_else(|| format!("expected /registry/path=host-file: {specification}"))?;
        validate_registry_path(registry_path)
            .map_err(|_| format!("invalid registry path {registry_path}"))?;
        let bytes = read_bounded(Path::new(host_path), 512 * 1024 * 1024)?;
        Container::parse(&bytes)
            .map_err(|error| format!("{host_path}: invalid RUNE: {error:?}"))?;
        packages.push(Package {
            registry_path: registry_path.into(),
            bytes,
        });
    }
    packages.sort_by(|left, right| left.registry_path.cmp(&right.registry_path));
    if packages
        .windows(2)
        .any(|pair| pair[0].registry_path == pair[1].registry_path)
    {
        return Err("duplicate registry path".into());
    }
    Ok(packages)
}

fn build_registry(
    generation: u64,
    signing: &SigningKey,
    development: bool,
    packages: &[Package],
) -> Result<Vec<u8>, String> {
    let entry_count = u32::try_from(packages.len()).map_err(|_| "too many packages")?;
    let strings_size = packages.iter().try_fold(0usize, |total, package| {
        total
            .checked_add(package.registry_path.len())
            .ok_or("registry strings overflow")
    })?;
    let entries_size = packages
        .len()
        .checked_mul(ENTRY_SIZE)
        .ok_or("registry entries overflow")?;
    let total = HEADER_SIZE
        .checked_add(entries_size)
        .and_then(|value| value.checked_add(strings_size))
        .ok_or("registry size overflow")?;
    if total > MAX_REGISTRY_SIZE {
        return Err("registry exceeds the bounded runtime size".into());
    }
    let mut output = vec![0u8; total];
    output[..8].copy_from_slice(&MAGIC);
    put_u16(&mut output, 8, FORMAT_VERSION);
    put_u16(&mut output, 10, HEADER_SIZE as u16);
    put_u16(&mut output, 12, ENTRY_SIZE as u16);
    put_u16(
        &mut output,
        14,
        if development {
            registry_flags::DEVELOPMENT
        } else {
            0
        },
    );
    put_u64(&mut output, 16, generation);
    put_u32(&mut output, 24, entry_count);
    put_u32(&mut output, 28, HEADER_SIZE as u32);
    put_u32(&mut output, 32, (HEADER_SIZE + entries_size) as u32);
    put_u32(&mut output, 36, strings_size as u32);
    output[40..56].copy_from_slice(&key_id(&signing.verifying_key().to_bytes()));

    let mut string_offset = 0usize;
    for (index, package) in packages.iter().enumerate() {
        let container = Container::parse(&package.bytes)
            .map_err(|error| format!("{}: {error:?}", package.registry_path))?;
        let header = container.header();
        let manifest = container
            .manifest()
            .ok_or_else(|| format!("{} has no manifest", package.registry_path))?;
        let offset = HEADER_SIZE + index * ENTRY_SIZE;
        let entry = &mut output[offset..offset + ENTRY_SIZE];
        entry[..16].copy_from_slice(&header.package_id);
        entry[16..32].copy_from_slice(&header.build_id);
        entry[32..64].copy_from_slice(&header.content_hash);
        put_u32(entry, 64, string_offset as u32);
        put_u16(entry, 68, package.registry_path.len() as u16);
        put_u32(entry, 72, manifest.version_major);
        put_u32(entry, 76, manifest.version_minor);
        put_u32(entry, 80, manifest.version_patch);
        put_u16(entry, 84, manifest.artifact_kind);
        put_u16(entry, 86, manifest.runtime_abi_minimum);
        put_u64(entry, 88, header.file_size);
        let start = HEADER_SIZE + entries_size + string_offset;
        output[start..start + package.registry_path.len()]
            .copy_from_slice(package.registry_path.as_bytes());
        string_offset += package.registry_path.len();
    }
    let payload_hash = sha256(&output[PAYLOAD_OFFSET..]);
    output[56..88].copy_from_slice(&payload_hash);
    let signature = signing.sign(&signature_message(&output).map_err(debug_error)?);
    output[SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE_SIZE]
        .copy_from_slice(&signature.to_bytes());
    Ok(output)
}

fn verify_package_set(registry: &Registry<'_>, packages: &[Package]) -> Result<(), String> {
    if packages.len() != registry.header().entry_count as usize {
        return Err(format!(
            "registry contains {} entries, but {} package files were supplied",
            registry.header().entry_count,
            packages.len()
        ));
    }
    for package in packages {
        let container = Container::parse(&package.bytes)
            .map_err(|error| format!("{}: {error:?}", package.registry_path))?;
        registry
            .require_container(&package.registry_path, &container)
            .map_err(|error| format!("{}: {error:?}", package.registry_path))?;
    }
    Ok(())
}

fn signing_key(options: &Options) -> Result<(SigningKey, bool), String> {
    match (options.development_key, options.secret_key_file.as_deref()) {
        (true, None) => Ok((development_signing_key(), true)),
        (false, Some(path)) => Ok((SigningKey::from_bytes(&read_key::<32>(path)?), false)),
        _ => Err("choose exactly one of --development-key or --secret-key-file".into()),
    }
}

fn trusted_key(options: &Options) -> Result<TrustedKey, String> {
    match (options.development_key, options.public_key_file.as_deref()) {
        (true, None) => Ok(TrustedKey::new(
            development_signing_key().verifying_key().to_bytes(),
            trust_flags::ALLOW_DEVELOPMENT,
        )),
        (false, Some(path)) => Ok(TrustedKey::new(
            read_key::<32>(path)?,
            trust_flags::ALLOW_PRODUCTION,
        )),
        _ => Err("choose exactly one of --development-key or --public-key-file".into()),
    }
}

fn development_signing_key() -> SigningKey {
    SigningKey::from_bytes(&sha256(DEVELOPMENT_KEY_DOMAIN))
}

fn read_key<const N: usize>(path: &Path) -> Result<[u8; N], String> {
    let bytes = read_bounded(path, 4096)?;
    if bytes.len() == N {
        return bytes
            .try_into()
            .map_err(|_| format!("{}: invalid key length", path.display()));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{}: key must be raw or hexadecimal", path.display()))?
        .trim();
    decode_hex(text).and_then(|decoded| {
        decoded
            .try_into()
            .map_err(|_| format!("{}: invalid key length", path.display()))
    })
}

fn read_current_registry(
    store: &Path,
    trust: &TrustedKey,
) -> Result<Option<(u64, [u8; 32])>, String> {
    let current = store.join("current");
    let pointer = match File::open(&current) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", current.display())),
    };
    let mut text = String::new();
    pointer
        .take(4097)
        .read_to_string(&mut text)
        .map_err(io_error)?;
    let mut lines = text.lines();
    if lines.next() != Some("RUSTOS-REGISTRY 1") {
        return Err("invalid current registry pointer".into());
    }
    let generation: u64 = lines
        .next()
        .ok_or("missing current generation")?
        .parse()
        .map_err(|_| "invalid current generation")?;
    let name = lines.next().ok_or("missing current index")?;
    if lines.next().is_some()
        || name.contains('/')
        || name.contains('\\')
        || !name.ends_with(".ridx")
    {
        return Err("invalid current registry pointer".into());
    }
    let bytes = read_bounded(&store.join("indexes").join(name), MAX_REGISTRY_SIZE as u64)?;
    let registry = Registry::verify(&bytes, &[*trust], generation)
        .map_err(|error| format!("current registry is corrupt: {error:?}"))?;
    if registry.header().generation != generation {
        return Err("current pointer generation mismatch".into());
    }
    Ok(Some((generation, registry.header().payload_hash)))
}

fn write_content_addressed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => Err(format!("{}: content-addressed collision", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => atomic_write(path, bytes),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{}: non-UTF-8 file name", path.display()))?;
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.tmp-{}-{sequence}", process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    let result = (|| {
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        fs::rename(&temporary, path).map_err(io_error)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let length = file.metadata().map_err(io_error)?.len();
    if length > maximum {
        return Err(format!("{}: file exceeds bounded limit", path.display()));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 != length {
        return Err(format!("{}: file changed while reading", path.display()));
    }
    Ok(bytes)
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

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("hex key has odd length".into());
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok(high << 4 | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("invalid hexadecimal key".into()),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn debug_error(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

fn usage() -> String {
    "usage:\n  rustos-package build --generation N --output FILE (--development-key | --secret-key-file FILE) /path=file.rune...\n  rustos-package verify --minimum-generation N (--development-key | --public-key-file FILE) REGISTRY [/path=file.rune...]\n  rustos-package activate --store DIR --minimum-generation N (--development-key | --public-key-file FILE) REGISTRY /path=file.rune...\n  rustos-package development-key-info\n\n--development-key is deterministic and intended only for local educational builds."
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_key_is_deterministic_but_policy_separated() {
        let first = development_signing_key().verifying_key().to_bytes();
        let second = development_signing_key().verifying_key().to_bytes();
        assert_eq!(first, second);
        assert_eq!(key_id(&first), TrustedKey::new(first, 1).key_id);
    }

    #[test]
    fn key_decoder_accepts_exact_hex_and_rejects_noise() {
        assert_eq!(decode_hex("00ff10").unwrap(), [0, 255, 16]);
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("zz").is_err());
    }
}

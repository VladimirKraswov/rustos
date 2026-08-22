//! Подписанный индекс пакетов RustOS.
//!
//! Registry не является базой данных и ничего не исполняет: parser сначала
//! целиком проверяет bounded layout, SHA-256 payload и Ed25519-подпись, и лишь
//! затем разрешает resolver'у искать RUNE. Закрытый ключ никогда не нужен ОС —
//! в trust store находятся только публичные ключи и их явная policy.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::cmp::Ordering;

use ed25519_dalek::{Signature, VerifyingKey};
use rustos_rune_format::{artifact_kind, sha256, Container};

pub const MAGIC: [u8; 8] = *b"RPKGIDX\0";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 160;
pub const ENTRY_SIZE: usize = 112;
pub const SIGNATURE_OFFSET: usize = 88;
pub const SIGNATURE_SIZE: usize = 64;
pub const PAYLOAD_OFFSET: usize = HEADER_SIZE;
pub const MAX_ENTRIES: u32 = 16_384;
pub const MAX_STRINGS_SIZE: u32 = 4 * 1024 * 1024;
/// Общий предел одного индекса, одинаковый для host activator и ring-3 loader.
pub const MAX_REGISTRY_SIZE: usize = 4 * 1024 * 1024;

/// Public trust anchor локальных образов из этого репозитория. Закрытый
/// development key существует только в host tool; runtime содержит лишь эти
/// публичные bytes и обязан разрешать их отдельной policy.
pub const DEVELOPMENT_PUBLIC_KEY: [u8; 32] = [
    0xed, 0x1d, 0x23, 0xf3, 0x97, 0xc3, 0x77, 0x69, 0x92, 0x9d, 0xe5, 0x45, 0xa3, 0xc9, 0xc2, 0x9a,
    0xa7, 0x6d, 0x79, 0x91, 0x29, 0x33, 0x0f, 0x91, 0x73, 0xb7, 0xd1, 0xac, 0x45, 0x53, 0x32, 0xf7,
];
pub const DEVELOPMENT_KEY_ID: [u8; 16] = [
    0x01, 0x64, 0x82, 0x62, 0x9d, 0x8e, 0x0e, 0x17, 0x72, 0xab, 0x6f, 0x56, 0x64, 0x85, 0x67, 0x35,
];

pub mod registry_flags {
    /// Индекс подписан публичным development key из исходного дерева. Такой
    /// trust anchor удобен для учебной сборки, но production policy его
    /// обязана явно запретить.
    pub const DEVELOPMENT: u16 = 1 << 0;
}

pub mod trust_flags {
    pub const ALLOW_PRODUCTION: u16 = 1 << 0;
    pub const ALLOW_DEVELOPMENT: u16 = 1 << 1;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    Truncated,
    BadMagic,
    UnsupportedVersion,
    InvalidHeader,
    TooManyEntries,
    InvalidPayloadHash,
    InvalidEntry,
    InvalidPath,
    UnsortedEntries,
    UnknownKey,
    KeyPolicyDenied,
    InvalidPublicKey,
    InvalidSignature,
    Downgrade,
    PackageNotRegistered,
    PackageMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub flags: u16,
    pub generation: u64,
    pub entry_count: u32,
    pub strings_size: u32,
    pub key_id: [u8; 16],
    pub payload_hash: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry<'a> {
    pub package_id: [u8; 16],
    pub build_id: [u8; 16],
    pub content_hash: [u8; 32],
    pub path: &'a str,
    pub flags: u16,
    pub version: (u32, u32, u32),
    pub artifact_kind: u16,
    pub abi_version: u16,
    pub file_size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrustedKey {
    pub key_id: [u8; 16],
    pub public_key: [u8; 32],
    pub flags: u16,
}

impl TrustedKey {
    pub fn new(public_key: [u8; 32], flags: u16) -> Self {
        Self {
            key_id: key_id(&public_key),
            public_key,
            flags,
        }
    }
}

pub const DEVELOPMENT_TRUSTED_KEY: TrustedKey = TrustedKey {
    key_id: DEVELOPMENT_KEY_ID,
    public_key: DEVELOPMENT_PUBLIC_KEY,
    flags: trust_flags::ALLOW_DEVELOPMENT,
};

/// Полностью проверенный borrowed view. Создать его в обход [`Registry::verify`]
/// невозможно, поэтому resolver не смешивает trusted и untrusted состояния.
#[derive(Clone, Copy, Debug)]
pub struct Registry<'a> {
    bytes: &'a [u8],
    header: Header,
}

impl<'a> Registry<'a> {
    pub fn verify(
        bytes: &'a [u8],
        trust: &[TrustedKey],
        minimum_generation: u64,
    ) -> Result<Self, RegistryError> {
        let header = parse_header(bytes)?;
        if header.generation < minimum_generation {
            return Err(RegistryError::Downgrade);
        }
        let payload = bytes
            .get(PAYLOAD_OFFSET..)
            .ok_or(RegistryError::Truncated)?;
        if sha256(payload) != header.payload_hash {
            return Err(RegistryError::InvalidPayloadHash);
        }
        let registry = Self { bytes, header };
        registry.validate_entries()?;
        registry.verify_signature(trust)?;
        Ok(registry)
    }

    pub const fn header(&self) -> Header {
        self.header
    }

    pub fn entries(&self) -> Entries<'_, 'a> {
        Entries {
            registry: self,
            index: 0,
        }
    }

    pub fn find_path(&self, path: &str) -> Option<Entry<'a>> {
        let mut low = 0u32;
        let mut high = self.header.entry_count;
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = self.entry(middle)?;
            match entry.path.as_bytes().cmp(path.as_bytes()) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Some(entry),
            }
        }
        None
    }

    /// Сопоставляет уже hash-проверенный RUNE с подписанной записью. Loader
    /// вызывает метод до отображения первой executable page.
    pub fn require_container(
        &self,
        path: &str,
        container: &Container<'_>,
    ) -> Result<Entry<'a>, RegistryError> {
        let entry = self
            .find_path(path)
            .ok_or(RegistryError::PackageNotRegistered)?;
        let header = container.header();
        let manifest = container.manifest().ok_or(RegistryError::PackageMismatch)?;
        if entry.package_id != header.package_id
            || entry.build_id != header.build_id
            || entry.content_hash != header.content_hash
            || entry.file_size != header.file_size
            || entry.version
                != (
                    manifest.version_major,
                    manifest.version_minor,
                    manifest.version_patch,
                )
            || entry.artifact_kind != manifest.artifact_kind
            || entry.abi_version != manifest.runtime_abi_minimum
        {
            return Err(RegistryError::PackageMismatch);
        }
        Ok(entry)
    }

    fn entry(&self, index: u32) -> Option<Entry<'a>> {
        if index >= self.header.entry_count {
            return None;
        }
        let offset =
            HEADER_SIZE.checked_add(usize::try_from(index).ok()?.checked_mul(ENTRY_SIZE)?)?;
        let bytes = self.bytes.get(offset..offset.checked_add(ENTRY_SIZE)?)?;
        let strings_offset = HEADER_SIZE.checked_add(
            usize::try_from(self.header.entry_count)
                .ok()?
                .checked_mul(ENTRY_SIZE)?,
        )?;
        let strings = self.bytes.get(strings_offset..)?;
        let path_offset = usize::try_from(read_u32(bytes, 64)?).ok()?;
        let path_length = usize::from(read_u16(bytes, 68)?);
        let path =
            core::str::from_utf8(strings.get(path_offset..path_offset.checked_add(path_length)?)?)
                .ok()?;
        Some(Entry {
            package_id: read_array(bytes, 0)?,
            build_id: read_array(bytes, 16)?,
            content_hash: read_array(bytes, 32)?,
            path,
            flags: read_u16(bytes, 70)?,
            version: (
                read_u32(bytes, 72)?,
                read_u32(bytes, 76)?,
                read_u32(bytes, 80)?,
            ),
            artifact_kind: read_u16(bytes, 84)?,
            abi_version: read_u16(bytes, 86)?,
            file_size: read_u64(bytes, 88)?,
        })
    }

    fn validate_entries(&self) -> Result<(), RegistryError> {
        let mut previous: Option<&[u8]> = None;
        for index in 0..self.header.entry_count {
            let entry = self.entry(index).ok_or(RegistryError::InvalidEntry)?;
            let offset = HEADER_SIZE + usize::try_from(index).unwrap() * ENTRY_SIZE;
            let raw = self
                .bytes
                .get(offset..offset + ENTRY_SIZE)
                .ok_or(RegistryError::InvalidEntry)?;
            if raw[96..].iter().any(|byte| *byte != 0)
                || entry.flags != 0
                || entry.abi_version == 0
                || entry.file_size < rustos_rune_format::HEADER_SIZE as u64
                || !matches!(
                    entry.artifact_kind,
                    artifact_kind::APPLICATION
                        | artifact_kind::LIBRARY
                        | artifact_kind::SERVICE
                        | artifact_kind::DRIVER
                )
                || entry.package_id.iter().all(|byte| *byte == 0)
                || entry.build_id.iter().all(|byte| *byte == 0)
                || entry.content_hash.iter().all(|byte| *byte == 0)
            {
                return Err(RegistryError::InvalidEntry);
            }
            validate_registry_path(entry.path)?;
            if previous.is_some_and(|value| value >= entry.path.as_bytes()) {
                return Err(RegistryError::UnsortedEntries);
            }
            previous = Some(entry.path.as_bytes());
        }
        Ok(())
    }

    fn verify_signature(&self, trust: &[TrustedKey]) -> Result<(), RegistryError> {
        let trusted = trust
            .iter()
            .find(|candidate| candidate.key_id == self.header.key_id)
            .ok_or(RegistryError::UnknownKey)?;
        let required = if self.header.flags & registry_flags::DEVELOPMENT != 0 {
            trust_flags::ALLOW_DEVELOPMENT
        } else {
            trust_flags::ALLOW_PRODUCTION
        };
        if trusted.flags & required == 0 {
            return Err(RegistryError::KeyPolicyDenied);
        }
        let verifying_key = VerifyingKey::from_bytes(&trusted.public_key)
            .map_err(|_| RegistryError::InvalidPublicKey)?;
        let signature_bytes: [u8; SIGNATURE_SIZE] = self.bytes
            [SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE_SIZE]
            .try_into()
            .map_err(|_| RegistryError::Truncated)?;
        let signature = Signature::from_bytes(&signature_bytes);
        verifying_key
            .verify_strict(&signature_message(self.bytes)?, &signature)
            .map_err(|_| RegistryError::InvalidSignature)
    }
}

pub struct Entries<'registry, 'bytes> {
    registry: &'registry Registry<'bytes>,
    index: u32,
}

impl<'bytes> Iterator for Entries<'_, 'bytes> {
    type Item = Entry<'bytes>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.registry.entry(self.index)?;
        self.index += 1;
        Some(entry)
    }
}

pub fn key_id(public_key: &[u8; 32]) -> [u8; 16] {
    sha256(public_key)[..16].try_into().unwrap()
}

/// Domain-separated statement. Подпись не охватывает сама себя; payload
/// связан с statement полем `payload_hash` в первых 88 байтах header.
pub fn signature_message(bytes: &[u8]) -> Result<[u8; 96], RegistryError> {
    let signed_header = bytes
        .get(..SIGNATURE_OFFSET)
        .ok_or(RegistryError::Truncated)?;
    let mut message = [0u8; 96];
    message[..8].copy_from_slice(b"RPKG-SIG");
    message[8..].copy_from_slice(signed_header);
    Ok(message)
}

pub fn validate_registry_path(path: &str) -> Result<(), RegistryError> {
    if path.len() < 2
        || path.len() > u16::MAX as usize
        || !path.starts_with('/')
        || path.contains('\0')
        || path.contains('\\')
        || path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(RegistryError::InvalidPath);
    }
    Ok(())
}

fn parse_header(bytes: &[u8]) -> Result<Header, RegistryError> {
    let header = bytes.get(..HEADER_SIZE).ok_or(RegistryError::Truncated)?;
    if header[..8] != MAGIC {
        return Err(RegistryError::BadMagic);
    }
    if read_u16(header, 8) != Some(FORMAT_VERSION) {
        return Err(RegistryError::UnsupportedVersion);
    }
    let flags = read_u16(header, 14).ok_or(RegistryError::Truncated)?;
    let entry_count = read_u32(header, 24).ok_or(RegistryError::Truncated)?;
    let entries_offset = read_u32(header, 28).ok_or(RegistryError::Truncated)?;
    let strings_offset = read_u32(header, 32).ok_or(RegistryError::Truncated)?;
    let strings_size = read_u32(header, 36).ok_or(RegistryError::Truncated)?;
    if entry_count > MAX_ENTRIES {
        return Err(RegistryError::TooManyEntries);
    }
    let entries_size = entry_count
        .checked_mul(ENTRY_SIZE as u32)
        .ok_or(RegistryError::InvalidHeader)?;
    let expected_strings = (HEADER_SIZE as u32)
        .checked_add(entries_size)
        .ok_or(RegistryError::InvalidHeader)?;
    let expected_size = expected_strings
        .checked_add(strings_size)
        .ok_or(RegistryError::InvalidHeader)?;
    if read_u16(header, 10) != Some(HEADER_SIZE as u16)
        || read_u16(header, 12) != Some(ENTRY_SIZE as u16)
        || flags & !registry_flags::DEVELOPMENT != 0
        || entries_offset != HEADER_SIZE as u32
        || strings_offset != expected_strings
        || strings_size > MAX_STRINGS_SIZE
        || bytes.len() > MAX_REGISTRY_SIZE
        || usize::try_from(expected_size).ok() != Some(bytes.len())
        || header[152..160].iter().any(|byte| *byte != 0)
    {
        return Err(RegistryError::InvalidHeader);
    }
    Ok(Header {
        flags,
        generation: read_u64(header, 16).ok_or(RegistryError::Truncated)?,
        entry_count,
        strings_size,
        key_id: read_array(header, 40).ok_or(RegistryError::Truncated)?,
        payload_hash: read_array(header, 56).ok_or(RegistryError::Truncated)?,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    extern crate std;

    use ed25519_dalek::{Signer, SigningKey};
    use std::{vec, vec::Vec};

    use super::*;

    fn signed_registry(generation: u64) -> (Vec<u8>, TrustedKey) {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let public = signing.verifying_key().to_bytes();
        let path = b"/apps/demo.rune";
        let mut bytes = vec![0u8; HEADER_SIZE + ENTRY_SIZE + path.len()];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[10..12].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        bytes[12..14].copy_from_slice(&(ENTRY_SIZE as u16).to_le_bytes());
        bytes[16..24].copy_from_slice(&generation.to_le_bytes());
        bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        bytes[32..36].copy_from_slice(&((HEADER_SIZE + ENTRY_SIZE) as u32).to_le_bytes());
        bytes[36..40].copy_from_slice(&(path.len() as u32).to_le_bytes());
        bytes[40..56].copy_from_slice(&key_id(&public));
        let entry = &mut bytes[HEADER_SIZE..HEADER_SIZE + ENTRY_SIZE];
        entry[..16].fill(1);
        entry[16..32].fill(2);
        entry[32..64].fill(3);
        entry[68..70].copy_from_slice(&(path.len() as u16).to_le_bytes());
        entry[72..76].copy_from_slice(&1u32.to_le_bytes());
        entry[84..86].copy_from_slice(&artifact_kind::APPLICATION.to_le_bytes());
        entry[86..88].copy_from_slice(&1u16.to_le_bytes());
        entry[88..96].copy_from_slice(&256u64.to_le_bytes());
        bytes[HEADER_SIZE + ENTRY_SIZE..].copy_from_slice(path);
        let payload_hash = sha256(&bytes[PAYLOAD_OFFSET..]);
        bytes[56..88].copy_from_slice(&payload_hash);
        let signature = signing.sign(&signature_message(&bytes).unwrap()).to_bytes();
        bytes[SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE_SIZE].copy_from_slice(&signature);
        (
            bytes,
            TrustedKey::new(public, trust_flags::ALLOW_PRODUCTION),
        )
    }

    #[test]
    fn verifies_signature_and_binary_searches_path() {
        assert_eq!(key_id(&DEVELOPMENT_PUBLIC_KEY), DEVELOPMENT_KEY_ID);
        let (bytes, trust) = signed_registry(8);
        let registry = Registry::verify(&bytes, &[trust], 8).unwrap();
        assert_eq!(
            registry.find_path("/apps/demo.rune").unwrap().file_size,
            256
        );
        assert!(registry.find_path("/apps/missing.rune").is_none());
    }

    #[test]
    fn rejects_tampering_downgrade_and_wrong_policy() {
        let (mut bytes, trust) = signed_registry(8);
        assert_eq!(
            Registry::verify(&bytes, &[trust], 9).unwrap_err(),
            RegistryError::Downgrade
        );
        bytes[HEADER_SIZE + 4] ^= 1;
        assert_eq!(
            Registry::verify(&bytes, &[trust], 0).unwrap_err(),
            RegistryError::InvalidPayloadHash
        );

        let (mut bytes, trust) = signed_registry(8);
        bytes[14..16].copy_from_slice(&registry_flags::DEVELOPMENT.to_le_bytes());
        let signing = SigningKey::from_bytes(&[7; 32]);
        let signature = signing.sign(&signature_message(&bytes).unwrap()).to_bytes();
        bytes[SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE_SIZE].copy_from_slice(&signature);
        assert_eq!(
            Registry::verify(&bytes, &[trust], 0).unwrap_err(),
            RegistryError::KeyPolicyDenied
        );
    }

    #[test]
    fn runtime_and_host_share_one_total_size_limit() {
        let mut bytes = vec![0u8; MAX_REGISTRY_SIZE + 1];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[10..12].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        bytes[12..14].copy_from_slice(&(ENTRY_SIZE as u16).to_le_bytes());
        bytes[28..32].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        bytes[32..36].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        bytes[36..40]
            .copy_from_slice(&((MAX_REGISTRY_SIZE + 1 - HEADER_SIZE) as u32).to_le_bytes());
        assert_eq!(
            Registry::verify(&bytes, &[], 0).unwrap_err(),
            RegistryError::InvalidHeader
        );
    }
}

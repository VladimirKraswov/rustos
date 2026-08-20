//! RUNE — нативный формат программ и разделяемых библиотек RustOS.
//!
//! Формат намеренно прост для изучения: один заголовок, таблица одинаковых
//! записей и выровненные payload-области. При этом parser проверяет все
//! смещения до того, как loader отобразит хотя бы одну страницу.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ops::Range;

/// Сигнатура включает управляющие байты, поэтому текстовый файл не может быть
/// случайно принят за программу.
pub const MAGIC: [u8; 8] = *b"RUNE\r\n\x1a\n";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 128;
pub const TOC_ENTRY_SIZE: usize = 64;
pub const RELOCATION_SIZE: usize = 24;
pub const IMPORT_SIZE: usize = 48;
pub const EXPORT_SIZE: usize = 56;
pub const DEPENDENCY_SIZE: usize = 48;
pub const CAPABILITY_REQUEST_SIZE: usize = 32;
pub const PAGE_SIZE: u64 = 4096;

/// Смещение hash-поля. При вычислении digest эти 32 байта считаются нулями.
pub const CONTENT_HASH_OFFSET: usize = 80;
pub const CONTENT_HASH_SIZE: usize = 32;

pub mod file_flags {
    pub const APPLICATION: u32 = 1 << 0;
    pub const LIBRARY: u32 = 1 << 1;
    pub const SERVICE: u32 = 1 << 2;
    pub const DRIVER: u32 = 1 << 3;
    pub const REPRODUCIBLE: u32 = 1 << 4;
}

pub mod record_kind {
    pub const SLICE: u16 = 1;
    pub const REGION: u16 = 2;
    pub const RELOCATIONS: u16 = 3;
    pub const IMPORTS: u16 = 4;
    pub const EXPORTS: u16 = 5;
    pub const DEPENDENCIES: u16 = 6;
    pub const TLS: u16 = 7;
    pub const RELRO: u16 = 8;
    pub const CAPABILITIES: u16 = 9;
    pub const STRINGS: u16 = 10;
    pub const DEBUG: u16 = 11;
    pub const SIGNATURE: u16 = 12;
}

pub mod architecture {
    pub const ANY: u16 = 0;
    pub const X86_64: u16 = 1;
    pub const AARCH64: u16 = 2;
}

pub mod region_flags {
    pub const READ: u32 = 1 << 0;
    pub const WRITE: u32 = 1 << 1;
    pub const EXECUTE: u32 = 1 << 2;
    /// Страницы одного sealed library region разрешено разделять процессам.
    pub const SHAREABLE: u32 = 1 << 3;
    /// Диапазон закрывается для записи после применения relocation.
    pub const RELRO: u32 = 1 << 4;
}

pub mod relocation_kind {
    /// В target записывается `load_base + addend`; символ не используется.
    pub const RELATIVE64: u16 = 1;
    /// Абсолютный 64-битный импорт из таблицы interface/name.
    pub const IMPORT64: u16 = 2;
    /// 32-битное PC-relative смещение до импортированного символа.
    pub const IMPORT_PC32: u16 = 3;
    /// Смещение TLS-переменной относительно thread pointer.
    pub const TLS_TPOFF64: u16 = 4;
}

/// Свойства импортируемого символа. Импорт по умолчанию обязателен и
/// разрешается eager: ошибка обнаруживается до передачи управления процессу.
pub mod import_flags {
    pub const WEAK: u32 = 1 << 0;
    pub const FUNCTION: u32 = 1 << 1;
    pub const DATA: u32 = 1 << 2;
    pub const TLS: u32 = 1 << 3;
}

pub mod export_flags {
    pub const FUNCTION: u16 = 1 << 0;
    pub const DATA: u16 = 1 << 1;
    pub const TLS: u16 = 1 << 2;
}

pub mod dependency_flags {
    pub const REQUIRED: u32 = 1 << 0;
    pub const LAZY: u32 = 1 << 1;
    pub const SYSTEM: u32 = 1 << 2;
    pub const SHARE_CODE: u32 = 1 << 3;
}

pub mod capability_flags {
    pub const REQUIRED: u32 = 1 << 0;
    pub const OPTIONAL: u32 = 1 << 1;
    pub const MULTIPLE: u32 = 1 << 2;
}

/// 128-битное имя версионируемого ABI. В отличие от имени файла библиотеки,
/// ID не меняется при переносе пакета в другой каталог.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InterfaceId(pub [u8; 16]);

/// ID экспортируемой функции/переменной внутри интерфейса. Он вычисляется из
/// канонической C ABI signature, поэтому Rust-mangled names не входят в ABI.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SymbolId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Import {
    pub interface: InterfaceId,
    pub symbol: SymbolId,
    pub minimum_abi: u16,
    pub maximum_abi: u16,
    pub flags: u32,
    pub name_offset: u32,
    pub name_length: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Export {
    pub interface: InterfaceId,
    pub symbol: SymbolId,
    pub virtual_address: u64,
    pub abi_version: u16,
    pub flags: u16,
    pub name_offset: u32,
    pub name_length: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dependency {
    pub interface: InterfaceId,
    /// Предпочтительный package; нули означают «любой доверенный provider».
    pub package: [u8; 16],
    pub minimum_abi: u16,
    pub maximum_abi: u16,
    pub flags: u32,
    pub name_offset: u32,
    pub name_length: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityRequest {
    pub service: InterfaceId,
    pub rights: u64,
    pub abi_version: u16,
    pub slot_hint: u16,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    Truncated,
    BadMagic,
    UnsupportedVersion,
    InvalidHeader,
    InvalidTable,
    InvalidRecord,
    InvalidHash,
    MissingSlice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub flags: u32,
    pub file_size: u64,
    pub toc_offset: u64,
    pub toc_count: u32,
    pub strings_index: u32,
    pub manifest_index: u32,
    pub package_id: [u8; 16],
    pub build_id: [u8; 16],
    pub content_hash: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TocEntry {
    pub kind: u16,
    pub architecture: u16,
    pub flags: u32,
    pub offset: u64,
    pub file_size: u64,
    pub virtual_address: u64,
    pub memory_size: u64,
    pub alignment: u64,
    pub name_offset: u32,
    pub name_length: u16,
    pub abi_version: u16,
    pub link: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Relocation {
    pub offset: u64,
    pub addend: i64,
    pub symbol: u32,
    pub kind: u16,
    pub flags: u16,
}

/// Заимствованный, уже структурно и криптографически проверенный контейнер.
#[derive(Clone, Copy)]
pub struct Container<'a> {
    bytes: &'a [u8],
    header: Header,
}

impl<'a> Container<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, FormatError> {
        let header = parse_header(bytes)?;
        if header.file_size != bytes.len() as u64 {
            return Err(FormatError::InvalidHeader);
        }
        let table_size = (header.toc_count as usize)
            .checked_mul(TOC_ENTRY_SIZE)
            .ok_or(FormatError::InvalidTable)?;
        checked_range(bytes.len(), header.toc_offset, table_size as u64)
            .ok_or(FormatError::InvalidTable)?;
        let container = Self { bytes, header };
        for index in 0..header.toc_count as usize {
            let entry = container.entry(index).ok_or(FormatError::InvalidTable)?;
            validate_entry(bytes.len(), entry)?;
        }
        if header.strings_index != u32::MAX
            && container
                .entry(header.strings_index as usize)
                .map(|e| e.kind)
                != Some(record_kind::STRINGS)
        {
            return Err(FormatError::InvalidTable);
        }
        if sha256_with_zeroed_range(bytes, CONTENT_HASH_OFFSET..CONTENT_HASH_OFFSET + 32)
            != header.content_hash
        {
            return Err(FormatError::InvalidHash);
        }
        Ok(container)
    }

    pub const fn header(&self) -> Header {
        self.header
    }

    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn entry(&self, index: usize) -> Option<TocEntry> {
        if index >= self.header.toc_count as usize {
            return None;
        }
        let offset = self.header.toc_offset as usize + index * TOC_ENTRY_SIZE;
        parse_entry(self.bytes.get(offset..offset + TOC_ENTRY_SIZE)?)
    }

    pub fn entries(&self) -> Entries<'_, 'a> {
        Entries {
            container: self,
            index: 0,
        }
    }

    pub fn payload(&self, entry: TocEntry) -> Option<&'a [u8]> {
        let range = checked_range(self.bytes.len(), entry.offset, entry.file_size)?;
        self.bytes.get(range)
    }

    pub fn name(&self, entry: TocEntry) -> Option<&'a str> {
        self.string(entry.name_offset, entry.name_length)
    }

    /// Читает diagnostic name из общей string table. Wire records imports,
    /// exports и dependencies используют те же offsets, что и TOC.
    pub fn string(&self, offset: u32, length: u16) -> Option<&'a str> {
        if length == 0 {
            return Some("");
        }
        let strings = self.entry(self.header.strings_index as usize)?;
        let table = self.payload(strings)?;
        let start = offset as usize;
        let end = start.checked_add(length as usize)?;
        core::str::from_utf8(table.get(start..end)?).ok()
    }

    pub fn slice(&self, architecture: u16) -> Result<TocEntry, FormatError> {
        self.entries()
            .find(|entry| entry.kind == record_kind::SLICE && entry.architecture == architecture)
            .ok_or(FormatError::MissingSlice)
    }
}

pub struct Entries<'container, 'bytes> {
    container: &'container Container<'bytes>,
    index: usize,
}

impl Iterator for Entries<'_, '_> {
    type Item = TocEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.container.entry(self.index)?;
        self.index += 1;
        Some(entry)
    }
}

pub fn parse_relocation(bytes: &[u8]) -> Option<Relocation> {
    Some(Relocation {
        offset: read_u64(bytes, 0)?,
        addend: read_i64(bytes, 8)?,
        symbol: read_u32(bytes, 16)?,
        kind: read_u16(bytes, 20)?,
        flags: read_u16(bytes, 22)?,
    })
}

pub fn parse_import(bytes: &[u8]) -> Option<Import> {
    Some(Import {
        interface: InterfaceId(read_array(bytes, 0)?),
        symbol: SymbolId(read_array(bytes, 16)?),
        minimum_abi: read_u16(bytes, 32)?,
        maximum_abi: read_u16(bytes, 34)?,
        flags: read_u32(bytes, 36)?,
        name_offset: read_u32(bytes, 40)?,
        name_length: read_u16(bytes, 44)?,
    })
}

pub fn parse_export(bytes: &[u8]) -> Option<Export> {
    Some(Export {
        interface: InterfaceId(read_array(bytes, 0)?),
        symbol: SymbolId(read_array(bytes, 16)?),
        virtual_address: read_u64(bytes, 32)?,
        abi_version: read_u16(bytes, 40)?,
        flags: read_u16(bytes, 42)?,
        name_offset: read_u32(bytes, 44)?,
        name_length: read_u16(bytes, 48)?,
    })
}

pub fn parse_dependency(bytes: &[u8]) -> Option<Dependency> {
    Some(Dependency {
        interface: InterfaceId(read_array(bytes, 0)?),
        package: read_array(bytes, 16)?,
        minimum_abi: read_u16(bytes, 32)?,
        maximum_abi: read_u16(bytes, 34)?,
        flags: read_u32(bytes, 36)?,
        name_offset: read_u32(bytes, 40)?,
        name_length: read_u16(bytes, 44)?,
    })
}

pub fn parse_capability_request(bytes: &[u8]) -> Option<CapabilityRequest> {
    Some(CapabilityRequest {
        service: InterfaceId(read_array(bytes, 0)?),
        rights: read_u64(bytes, 16)?,
        abi_version: read_u16(bytes, 24)?,
        slot_hint: read_u16(bytes, 26)?,
        flags: read_u32(bytes, 28)?,
    })
}

/// Детерминированный ID интерфейса. Читаемое имя остаётся в string table для
/// диагностики, но resolver сравнивает все 128 бит ID.
pub fn interface_id(canonical_name: &str) -> InterfaceId {
    InterfaceId(domain_id(b"RUNE/interface/v1\0", canonical_name.as_bytes()))
}

/// Детерминированный symbol ID включает interface ID и каноническую C ABI
/// signature, например `read(u64,*mut u8,u64)->i64`.
pub fn symbol_id(interface: InterfaceId, canonical_signature: &str) -> SymbolId {
    let mut hash = Sha256::new();
    for byte in b"RUNE/symbol/v1\0" {
        hash.update_byte(*byte);
    }
    for byte in interface.0 {
        hash.update_byte(byte);
    }
    for byte in canonical_signature.bytes() {
        hash.update_byte(byte);
    }
    let digest = hash.finish();
    SymbolId(digest[..16].try_into().unwrap())
}

fn domain_id(domain: &[u8], value: &[u8]) -> [u8; 16] {
    let mut hash = Sha256::new();
    for byte in domain.iter().chain(value).copied() {
        hash.update_byte(byte);
    }
    hash.finish()[..16].try_into().unwrap()
}

pub fn sha256_with_zeroed_range(bytes: &[u8], zeroed: Range<usize>) -> [u8; 32] {
    let mut state = Sha256::new();
    for (index, byte) in bytes.iter().copied().enumerate() {
        state.update_byte(if zeroed.contains(&index) { 0 } else { byte });
    }
    state.finish()
}

fn parse_header(bytes: &[u8]) -> Result<Header, FormatError> {
    let bytes = bytes.get(..HEADER_SIZE).ok_or(FormatError::Truncated)?;
    if bytes[..8] != MAGIC {
        return Err(FormatError::BadMagic);
    }
    if read_u16(bytes, 8) != Some(FORMAT_VERSION) {
        return Err(FormatError::UnsupportedVersion);
    }
    if read_u16(bytes, 10) != Some(HEADER_SIZE as u16)
        || read_u32(bytes, 36) != Some(TOC_ENTRY_SIZE as u32)
        || bytes[112..128].iter().any(|byte| *byte != 0)
    {
        return Err(FormatError::InvalidHeader);
    }
    Ok(Header {
        flags: read_u32(bytes, 12).ok_or(FormatError::Truncated)?,
        file_size: read_u64(bytes, 16).ok_or(FormatError::Truncated)?,
        toc_offset: read_u64(bytes, 24).ok_or(FormatError::Truncated)?,
        toc_count: read_u32(bytes, 32).ok_or(FormatError::Truncated)?,
        strings_index: read_u32(bytes, 40).ok_or(FormatError::Truncated)?,
        manifest_index: read_u32(bytes, 44).ok_or(FormatError::Truncated)?,
        package_id: bytes[48..64]
            .try_into()
            .ok()
            .ok_or(FormatError::Truncated)?,
        build_id: bytes[64..80]
            .try_into()
            .ok()
            .ok_or(FormatError::Truncated)?,
        content_hash: bytes[80..112]
            .try_into()
            .ok()
            .ok_or(FormatError::Truncated)?,
    })
}

fn parse_entry(bytes: &[u8]) -> Option<TocEntry> {
    Some(TocEntry {
        kind: read_u16(bytes, 0)?,
        architecture: read_u16(bytes, 2)?,
        flags: read_u32(bytes, 4)?,
        offset: read_u64(bytes, 8)?,
        file_size: read_u64(bytes, 16)?,
        virtual_address: read_u64(bytes, 24)?,
        memory_size: read_u64(bytes, 32)?,
        alignment: read_u64(bytes, 40)?,
        name_offset: read_u32(bytes, 48)?,
        name_length: read_u16(bytes, 52)?,
        abi_version: read_u16(bytes, 54)?,
        link: read_u32(bytes, 56)?,
    })
}

fn validate_entry(file_len: usize, entry: TocEntry) -> Result<(), FormatError> {
    if entry.kind == 0
        || entry.alignment != 0 && !entry.alignment.is_power_of_two()
        || checked_range(file_len, entry.offset, entry.file_size).is_none()
        || entry.file_size > entry.memory_size
        || entry.flags & region_flags::WRITE != 0 && entry.flags & region_flags::EXECUTE != 0
    {
        return Err(FormatError::InvalidRecord);
    }
    Ok(())
}

fn checked_range(file_len: usize, offset: u64, length: u64) -> Option<Range<usize>> {
    let start = usize::try_from(offset).ok()?;
    let length = usize::try_from(length).ok()?;
    let end = start.checked_add(length)?;
    (end <= file_len).then_some(start..end)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn read_i64(bytes: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

/// Небольшая самостоятельная SHA-256 нужна kernel loader'у до запуска
/// crypto-service. Код не выделяет память и одинаков для host/no_std.
struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    total_len: u64,
}

impl Sha256 {
    const fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            block_len: 0,
            total_len: 0,
        }
    }

    fn update_byte(&mut self, byte: u8) {
        self.block[self.block_len] = byte;
        self.block_len += 1;
        self.total_len += 1;
        if self.block_len == 64 {
            self.compress();
            self.block_len = 0;
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.block[self.block_len] = 0x80;
        self.block_len += 1;
        if self.block_len > 56 {
            self.block[self.block_len..].fill(0);
            self.compress();
            self.block_len = 0;
        }
        self.block[self.block_len..56].fill(0);
        self.block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        self.compress();
        let mut output = [0u8; 32];
        for (chunk, word) in output.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    fn compress(&mut self) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0u32; 64];
        for (index, chunk) in self.block.as_chunks::<4>().0.iter().enumerate() {
            words[index] = u32::from_be_bytes(*chunk);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        let digest = sha256_with_zeroed_range(b"abc", 3..3);
        assert_eq!(
            digest,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn interface_and_symbol_ids_are_stable_and_domain_separated() {
        let vfs = interface_id("org.rustos.vfs/1");
        let same = interface_id("org.rustos.vfs/1");
        let ui = interface_id("org.rustos.ui/1");
        assert_eq!(vfs, same);
        assert_ne!(vfs, ui);
        assert_ne!(
            symbol_id(vfs, "read(u64,*mut u8,u64)->i64"),
            symbol_id(vfs, "write(u64,*const u8,u64)->i64")
        );
    }
}

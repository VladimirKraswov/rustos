//! Строгий bounded parser стандартного ELF64 `ET_DYN`.

use core::str;

const ET_DYN: u16 = 3;
#[cfg(target_arch = "x86_64")]
const ELF_MACHINE: u16 = 0x3e;
#[cfg(target_arch = "aarch64")]
const ELF_MACHINE: u16 = 0xb7;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;
const PT_GNU_RELRO: u32 = 0x6474_e552;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_STRSZ: i64 = 10;
const DT_SYMENT: i64 = 11;
const DT_SONAME: i64 = 14;
const DT_PLTREL: i64 = 20;
const DT_JMPREL: i64 = 23;
const DT_PLTRELSZ: i64 = 2;
const DT_GNU_HASH: i64 = 0x6fff_fef5;

pub(crate) const MAX_LOAD_SEGMENTS: usize = 8;
pub(crate) const MAX_NEEDED: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    InvalidHeader,
    WrongMachine,
    InvalidProgramHeader,
    InvalidDynamicTable,
    InvalidString,
    InvalidSymbolTable,
    InvalidRelocationTable,
    WritableExecutableSegment,
    TooManySegments,
    TooManyDependencies,
    MissingDynamicTable,
}

/// ELF `p_flags` без утечки числовых magic constants в loader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgramFlags(pub u32);

impl ProgramFlags {
    pub const EXECUTE: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const READ: Self = Self(4);

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Segment {
    pub offset: u64,
    pub virtual_address: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub flags: ProgramFlags,
}

const EMPTY_SEGMENT: Segment = Segment {
    offset: 0,
    virtual_address: 0,
    file_size: 0,
    memory_size: 0,
    flags: ProgramFlags(0),
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct TlsSegment {
    pub offset: u64,
    pub virtual_address: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub alignment: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RelroSegment {
    pub virtual_address: u64,
    pub memory_size: u64,
}

#[derive(Clone, Copy, Debug)]
struct DynamicInfo {
    strtab: u64,
    strsz: u64,
    symtab: u64,
    syment: u64,
    hash: u64,
    gnu_hash: u64,
    rela: u64,
    relasz: u64,
    relaent: u64,
    jmprel: u64,
    pltrelsz: u64,
    pltrel: u64,
    soname: Option<u32>,
    needed: [u32; MAX_NEEDED],
    needed_count: usize,
}

const EMPTY_DYNAMIC: DynamicInfo = DynamicInfo {
    strtab: 0,
    strsz: 0,
    symtab: 0,
    syment: 0,
    hash: 0,
    gnu_hash: 0,
    rela: 0,
    relasz: 0,
    relaent: 0,
    jmprel: 0,
    pltrelsz: 0,
    pltrel: 0,
    soname: None,
    needed: [0; MAX_NEEDED],
    needed_count: 0,
};

/// Одна запись `.dynsym`.
#[derive(Clone, Copy, Debug)]
pub struct Symbol {
    pub name_offset: u32,
    pub info: u8,
    pub visibility: u8,
    pub section: u16,
    pub value: u64,
    pub size: u64,
}

impl Symbol {
    pub const fn binding(self) -> u8 {
        self.info >> 4
    }

    pub const fn kind(self) -> u8 {
        self.info & 0x0f
    }

    pub const fn is_defined(self) -> bool {
        self.section != 0
    }
}

/// Одна ELF64 RELA relocation.
#[derive(Clone, Copy, Debug)]
pub struct Relocation {
    pub offset: u64,
    pub symbol: u32,
    pub kind: u32,
    pub addend: i64,
}

/// Проверенное представление ELF, ссылающееся на неизменяемый file image.
#[derive(Clone, Copy)]
pub struct ElfView<'a> {
    image: &'a [u8],
    entry: u64,
    segments: [Segment; MAX_LOAD_SEGMENTS],
    segment_count: usize,
    dynamic: DynamicInfo,
    tls: Option<TlsSegment>,
    relro: Option<RelroSegment>,
    symbol_count: u32,
}

impl<'a> ElfView<'a> {
    pub fn parse(image: &'a [u8]) -> Result<Self, ElfError> {
        if image.get(0..4) != Some(b"\x7fELF")
            || image.get(4) != Some(&2)
            || image.get(5) != Some(&1)
            || image.get(6) != Some(&1)
            || read_u16(image, 16) != Some(ET_DYN)
        {
            return Err(ElfError::InvalidHeader);
        }
        if read_u16(image, 18) != Some(ELF_MACHINE) {
            return Err(ElfError::WrongMachine);
        }
        let entry = read_u64(image, 24).ok_or(ElfError::InvalidHeader)?;
        let phoff = usize::try_from(read_u64(image, 32).ok_or(ElfError::InvalidHeader)?)
            .map_err(|_| ElfError::InvalidHeader)?;
        let phentsize = usize::from(read_u16(image, 54).ok_or(ElfError::InvalidHeader)?);
        let phnum = usize::from(read_u16(image, 56).ok_or(ElfError::InvalidHeader)?);
        if phentsize < 56 || phnum == 0 {
            return Err(ElfError::InvalidHeader);
        }

        let mut view = Self {
            image,
            entry,
            segments: [EMPTY_SEGMENT; MAX_LOAD_SEGMENTS],
            segment_count: 0,
            dynamic: EMPTY_DYNAMIC,
            tls: None,
            relro: None,
            symbol_count: 0,
        };
        let mut dynamic_file_range = None;
        for index in 0..phnum {
            let header = phoff
                .checked_add(
                    index
                        .checked_mul(phentsize)
                        .ok_or(ElfError::InvalidHeader)?,
                )
                .ok_or(ElfError::InvalidHeader)?;
            let program = image
                .get(
                    header
                        ..header
                            .checked_add(56)
                            .ok_or(ElfError::InvalidProgramHeader)?,
                )
                .ok_or(ElfError::InvalidProgramHeader)?;
            let kind = read_u32(program, 0).ok_or(ElfError::InvalidProgramHeader)?;
            let flags = ProgramFlags(read_u32(program, 4).ok_or(ElfError::InvalidProgramHeader)?);
            let offset = read_u64(program, 8).ok_or(ElfError::InvalidProgramHeader)?;
            let virtual_address = read_u64(program, 16).ok_or(ElfError::InvalidProgramHeader)?;
            let file_size = read_u64(program, 32).ok_or(ElfError::InvalidProgramHeader)?;
            let memory_size = read_u64(program, 40).ok_or(ElfError::InvalidProgramHeader)?;
            let alignment = read_u64(program, 48).ok_or(ElfError::InvalidProgramHeader)?;
            let file_end = offset
                .checked_add(file_size)
                .ok_or(ElfError::InvalidProgramHeader)?;
            if file_end > image.len() as u64 || file_size > memory_size {
                return Err(ElfError::InvalidProgramHeader);
            }
            match kind {
                PT_LOAD => {
                    if flags.contains(ProgramFlags::WRITE) && flags.contains(ProgramFlags::EXECUTE)
                    {
                        return Err(ElfError::WritableExecutableSegment);
                    }
                    if alignment > 1
                        && (!alignment.is_power_of_two()
                            || offset % alignment != virtual_address % alignment)
                    {
                        return Err(ElfError::InvalidProgramHeader);
                    }
                    if view.segment_count == MAX_LOAD_SEGMENTS {
                        return Err(ElfError::TooManySegments);
                    }
                    view.segments[view.segment_count] = Segment {
                        offset,
                        virtual_address,
                        file_size,
                        memory_size,
                        flags,
                    };
                    view.segment_count += 1;
                }
                PT_DYNAMIC => dynamic_file_range = Some((offset, file_size)),
                PT_TLS => {
                    view.tls = Some(TlsSegment {
                        offset,
                        virtual_address,
                        file_size,
                        memory_size,
                        alignment: alignment.max(1),
                    })
                }
                PT_GNU_RELRO => {
                    view.relro = Some(RelroSegment {
                        virtual_address,
                        memory_size,
                    })
                }
                _ => {}
            }
        }
        if view.segment_count == 0 {
            return Err(ElfError::InvalidProgramHeader);
        }
        view.reject_overlapping_load_pages()?;
        let (dynamic_offset, dynamic_size) =
            dynamic_file_range.ok_or(ElfError::MissingDynamicTable)?;
        view.parse_dynamic(dynamic_offset, dynamic_size)?;
        view.symbol_count = view.read_symbol_count()?;
        Ok(view)
    }

    pub const fn entry(self) -> u64 {
        self.entry
    }

    pub(crate) fn image(self) -> &'a [u8] {
        self.image
    }

    pub(crate) fn segments(&self) -> &[Segment] {
        &self.segments[..self.segment_count]
    }

    pub(crate) const fn tls(self) -> Option<TlsSegment> {
        self.tls
    }

    pub(crate) const fn relro(self) -> Option<RelroSegment> {
        self.relro
    }

    pub fn needed_count(self) -> usize {
        self.dynamic.needed_count
    }

    pub fn needed(self, index: usize) -> Result<&'a str, ElfError> {
        let offset = *self
            .dynamic
            .needed
            .get(index)
            .filter(|_| index < self.dynamic.needed_count)
            .ok_or(ElfError::InvalidString)?;
        self.dynamic_string(offset)
    }

    pub fn soname(self) -> Result<Option<&'a str>, ElfError> {
        self.dynamic
            .soname
            .map(|offset| self.dynamic_string(offset))
            .transpose()
    }

    pub fn symbol_count(self) -> u32 {
        self.symbol_count
    }

    pub fn symbol(self, index: u32) -> Result<Symbol, ElfError> {
        if index >= self.symbol_count || self.dynamic.syment != 24 {
            return Err(ElfError::InvalidSymbolTable);
        }
        let table = self.virtual_to_file(self.dynamic.symtab)?;
        let offset = table
            .checked_add(u64::from(index) * 24)
            .ok_or(ElfError::InvalidSymbolTable)?;
        let offset = usize::try_from(offset).map_err(|_| ElfError::InvalidSymbolTable)?;
        Ok(Symbol {
            name_offset: read_u32(self.image, offset).ok_or(ElfError::InvalidSymbolTable)?,
            info: *self
                .image
                .get(offset + 4)
                .ok_or(ElfError::InvalidSymbolTable)?,
            visibility: *self
                .image
                .get(offset + 5)
                .ok_or(ElfError::InvalidSymbolTable)?,
            section: read_u16(self.image, offset + 6).ok_or(ElfError::InvalidSymbolTable)?,
            value: read_u64(self.image, offset + 8).ok_or(ElfError::InvalidSymbolTable)?,
            size: read_u64(self.image, offset + 16).ok_or(ElfError::InvalidSymbolTable)?,
        })
    }

    pub fn symbol_name(self, symbol: Symbol) -> Result<&'a str, ElfError> {
        self.dynamic_string(symbol.name_offset)
    }

    pub fn relocation_count(self) -> Result<usize, ElfError> {
        self.validate_relocation_tables()?;
        let normal = usize::try_from(self.dynamic.relasz / 24)
            .map_err(|_| ElfError::InvalidRelocationTable)?;
        let plt = usize::try_from(self.dynamic.pltrelsz / 24)
            .map_err(|_| ElfError::InvalidRelocationTable)?;
        Ok(normal + plt)
    }

    pub fn relocation(self, index: usize) -> Result<Relocation, ElfError> {
        self.validate_relocation_tables()?;
        let normal = usize::try_from(self.dynamic.relasz / 24)
            .map_err(|_| ElfError::InvalidRelocationTable)?;
        let (table, local_index) = if index < normal {
            (self.dynamic.rela, index)
        } else {
            (self.dynamic.jmprel, index - normal)
        };
        if index >= self.relocation_count()? {
            return Err(ElfError::InvalidRelocationTable);
        }
        let offset = self
            .virtual_to_file(table)?
            .checked_add(local_index as u64 * 24)
            .ok_or(ElfError::InvalidRelocationTable)?;
        let offset = usize::try_from(offset).map_err(|_| ElfError::InvalidRelocationTable)?;
        let info = read_u64(self.image, offset + 8).ok_or(ElfError::InvalidRelocationTable)?;
        Ok(Relocation {
            offset: read_u64(self.image, offset).ok_or(ElfError::InvalidRelocationTable)?,
            symbol: (info >> 32) as u32,
            kind: info as u32,
            addend: read_i64(self.image, offset + 16).ok_or(ElfError::InvalidRelocationTable)?,
        })
    }

    pub(crate) fn minimum_page(self, page_size: u64) -> u64 {
        self.segments()[0..self.segment_count]
            .iter()
            .map(|segment| align_down(segment.virtual_address, page_size))
            .min()
            .unwrap_or(0)
    }

    pub(crate) fn maximum_page(self, page_size: u64) -> Result<u64, ElfError> {
        self.segments()[0..self.segment_count]
            .iter()
            .map(|segment| {
                align_up(
                    segment
                        .virtual_address
                        .checked_add(segment.memory_size)
                        .ok_or(ElfError::InvalidProgramHeader)?,
                    page_size,
                )
                .ok_or(ElfError::InvalidProgramHeader)
            })
            .try_fold(0, |highest, value| value.map(|value| highest.max(value)))
    }

    pub(crate) fn address_is_writable(self, address: u64, bytes: u64) -> bool {
        let Some(end) = address.checked_add(bytes) else {
            return false;
        };
        self.segments().iter().any(|segment| {
            segment.flags.contains(ProgramFlags::WRITE)
                && address >= segment.virtual_address
                && end <= segment.virtual_address.saturating_add(segment.memory_size)
        })
    }

    pub(crate) fn address_is_executable(self, address: u64) -> bool {
        self.segments().iter().any(|segment| {
            segment.flags.contains(ProgramFlags::EXECUTE)
                && address >= segment.virtual_address
                && address < segment.virtual_address.saturating_add(segment.memory_size)
        })
    }

    fn parse_dynamic(&mut self, offset: u64, size: u64) -> Result<(), ElfError> {
        let end = offset
            .checked_add(size)
            .ok_or(ElfError::InvalidDynamicTable)?;
        let mut cursor = offset;
        while cursor + 16 <= end {
            let position = usize::try_from(cursor).map_err(|_| ElfError::InvalidDynamicTable)?;
            let tag = read_i64(self.image, position).ok_or(ElfError::InvalidDynamicTable)?;
            let value = read_u64(self.image, position + 8).ok_or(ElfError::InvalidDynamicTable)?;
            match tag {
                DT_NULL => break,
                DT_NEEDED => {
                    if self.dynamic.needed_count == MAX_NEEDED {
                        return Err(ElfError::TooManyDependencies);
                    }
                    self.dynamic.needed[self.dynamic.needed_count] =
                        u32::try_from(value).map_err(|_| ElfError::InvalidString)?;
                    self.dynamic.needed_count += 1;
                }
                DT_HASH => self.dynamic.hash = value,
                DT_GNU_HASH => self.dynamic.gnu_hash = value,
                DT_STRTAB => self.dynamic.strtab = value,
                DT_STRSZ => self.dynamic.strsz = value,
                DT_SYMTAB => self.dynamic.symtab = value,
                DT_SYMENT => self.dynamic.syment = value,
                DT_RELA => self.dynamic.rela = value,
                DT_RELASZ => self.dynamic.relasz = value,
                DT_RELAENT => self.dynamic.relaent = value,
                DT_JMPREL => self.dynamic.jmprel = value,
                DT_PLTRELSZ => self.dynamic.pltrelsz = value,
                DT_PLTREL => self.dynamic.pltrel = value,
                DT_SONAME => {
                    self.dynamic.soname =
                        Some(u32::try_from(value).map_err(|_| ElfError::InvalidString)?)
                }
                _ => {}
            }
            cursor += 16;
        }
        if self.dynamic.strtab == 0
            || self.dynamic.strsz == 0
            || self.dynamic.symtab == 0
            || self.dynamic.syment != 24
            || (self.dynamic.hash == 0 && self.dynamic.gnu_hash == 0)
        {
            return Err(ElfError::InvalidDynamicTable);
        }
        Ok(())
    }

    fn dynamic_string(self, offset: u32) -> Result<&'a str, ElfError> {
        if u64::from(offset) >= self.dynamic.strsz {
            return Err(ElfError::InvalidString);
        }
        let start = self
            .virtual_to_file(self.dynamic.strtab)?
            .checked_add(u64::from(offset))
            .ok_or(ElfError::InvalidString)?;
        let limit = self
            .virtual_to_file(self.dynamic.strtab)?
            .checked_add(self.dynamic.strsz)
            .ok_or(ElfError::InvalidString)?;
        let start = usize::try_from(start).map_err(|_| ElfError::InvalidString)?;
        let limit = usize::try_from(limit).map_err(|_| ElfError::InvalidString)?;
        let bytes = self
            .image
            .get(start..limit)
            .ok_or(ElfError::InvalidString)?;
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(ElfError::InvalidString)?;
        str::from_utf8(&bytes[..end]).map_err(|_| ElfError::InvalidString)
    }

    fn read_symbol_count(self) -> Result<u32, ElfError> {
        if self.dynamic.hash != 0 {
            let offset = usize::try_from(self.virtual_to_file(self.dynamic.hash)?)
                .map_err(|_| ElfError::InvalidSymbolTable)?;
            return read_u32(self.image, offset + 4).ok_or(ElfError::InvalidSymbolTable);
        }
        self.gnu_hash_symbol_count()
    }

    fn gnu_hash_symbol_count(self) -> Result<u32, ElfError> {
        let base = usize::try_from(self.virtual_to_file(self.dynamic.gnu_hash)?)
            .map_err(|_| ElfError::InvalidSymbolTable)?;
        let buckets = read_u32(self.image, base).ok_or(ElfError::InvalidSymbolTable)?;
        let symbol_offset = read_u32(self.image, base + 4).ok_or(ElfError::InvalidSymbolTable)?;
        let bloom_words = read_u32(self.image, base + 8).ok_or(ElfError::InvalidSymbolTable)?;
        let bucket_base = base
            .checked_add(16)
            .and_then(|value| value.checked_add(bloom_words as usize * 8))
            .ok_or(ElfError::InvalidSymbolTable)?;
        let chain_base = bucket_base
            .checked_add(buckets as usize * 4)
            .ok_or(ElfError::InvalidSymbolTable)?;
        let mut highest = symbol_offset;
        for bucket in 0..buckets as usize {
            let mut symbol = read_u32(self.image, bucket_base + bucket * 4)
                .ok_or(ElfError::InvalidSymbolTable)?;
            if symbol < symbol_offset {
                continue;
            }
            loop {
                let chain = usize::try_from(symbol - symbol_offset)
                    .ok()
                    .and_then(|index| read_u32(self.image, chain_base + index * 4))
                    .ok_or(ElfError::InvalidSymbolTable)?;
                highest = highest.max(symbol + 1);
                symbol = symbol.checked_add(1).ok_or(ElfError::InvalidSymbolTable)?;
                if chain & 1 != 0 {
                    break;
                }
            }
        }
        Ok(highest)
    }

    fn validate_relocation_tables(self) -> Result<(), ElfError> {
        if self.dynamic.relasz != 0
            && (self.dynamic.rela == 0
                || self.dynamic.relaent != 24
                || !self.dynamic.relasz.is_multiple_of(24))
        {
            return Err(ElfError::InvalidRelocationTable);
        }
        if self.dynamic.pltrelsz != 0
            && (self.dynamic.jmprel == 0
                || self.dynamic.pltrel != DT_RELA as u64
                || !self.dynamic.pltrelsz.is_multiple_of(24))
        {
            return Err(ElfError::InvalidRelocationTable);
        }
        Ok(())
    }

    fn virtual_to_file(self, address: u64) -> Result<u64, ElfError> {
        self.segments()
            .iter()
            .find(|segment| {
                address >= segment.virtual_address
                    && address < segment.virtual_address.saturating_add(segment.file_size)
            })
            .and_then(|segment| {
                segment
                    .offset
                    .checked_add(address - segment.virtual_address)
            })
            .ok_or(ElfError::InvalidDynamicTable)
    }

    fn reject_overlapping_load_pages(self) -> Result<(), ElfError> {
        const PAGE: u64 = 4096;
        for left in 0..self.segment_count {
            let left_start = align_down(self.segments[left].virtual_address, PAGE);
            let left_end = align_up(
                self.segments[left]
                    .virtual_address
                    .checked_add(self.segments[left].memory_size)
                    .ok_or(ElfError::InvalidProgramHeader)?,
                PAGE,
            )
            .ok_or(ElfError::InvalidProgramHeader)?;
            for right in left + 1..self.segment_count {
                let right_start = align_down(self.segments[right].virtual_address, PAGE);
                let right_end = align_up(
                    self.segments[right]
                        .virtual_address
                        .checked_add(self.segments[right].memory_size)
                        .ok_or(ElfError::InvalidProgramHeader)?,
                    PAGE,
                )
                .ok_or(ElfError::InvalidProgramHeader)?;
                if left_start < right_end && right_start < left_end {
                    return Err(ElfError::InvalidProgramHeader);
                }
            }
        }
        Ok(())
    }
}

pub(crate) const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

pub(crate) fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    Some(u16::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    Some(u64::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn read_i64(bytes: &[u8], offset: usize) -> Option<i64> {
    let end = offset.checked_add(8)?;
    Some(i64::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

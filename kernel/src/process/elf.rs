//! Проверяющий ELF64 PIE loader пользовательского процесса.

use rustos_abi::PAGE_SIZE;

use crate::memory::{AddressSpace, UserPageFlags};

use super::LoadedImage;

const USER_IMAGE_BASE: u64 = 0x0000_4000_0000_0000;
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const USER_STACK_PAGES: u64 = 16;
const MAX_LOAD_SEGMENTS: usize = 16;

const ET_DYN: u16 = 3;
#[cfg(target_arch = "x86_64")]
const ELF_MACHINE: u16 = 0x3e; // EM_X86_64
#[cfg(target_arch = "aarch64")]
const ELF_MACHINE: u16 = 0xb7; // EM_AARCH64
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
#[cfg(target_arch = "x86_64")]
const RELATIVE_RELOCATION: u32 = 8; // R_X86_64_RELATIVE
#[cfg(target_arch = "aarch64")]
const RELATIVE_RELOCATION: u32 = 1027; // R_AARCH64_RELATIVE

#[derive(Clone, Copy)]
struct LoadSegment {
    offset: u64,
    virtual_address: u64,
    file_size: u64,
    memory_size: u64,
    flags: u32,
}

const EMPTY_SEGMENT: LoadSegment = LoadSegment {
    offset: 0,
    virtual_address: 0,
    file_size: 0,
    memory_size: 0,
    flags: 0,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    InvalidHeader,
    InvalidSegment,
    UnsupportedRelocation,
    WritableExecutableSegment,
    AddressSpace,
}

pub fn load(space: &mut AddressSpace, image: &[u8]) -> Result<LoadedImage, ElfError> {
    if image.get(0..4) != Some(b"\x7fELF")
        || image.get(4) != Some(&2)
        || image.get(5) != Some(&1)
        || read_u16(image, 16) != Some(ET_DYN)
        || read_u16(image, 18) != Some(ELF_MACHINE)
    {
        return Err(ElfError::InvalidHeader);
    }
    let entry = read_u64(image, 24).ok_or(ElfError::InvalidHeader)?;
    let phoff = read_u64(image, 32).ok_or(ElfError::InvalidHeader)? as usize;
    let phentsize = read_u16(image, 54).ok_or(ElfError::InvalidHeader)? as usize;
    let phnum = read_u16(image, 56).ok_or(ElfError::InvalidHeader)? as usize;
    if phentsize < 56 {
        return Err(ElfError::InvalidHeader);
    }

    let mut segments = [EMPTY_SEGMENT; MAX_LOAD_SEGMENTS];
    let mut segment_count = 0usize;
    let mut dynamic = None;
    for index in 0..phnum {
        let header = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or(ElfError::InvalidHeader)?,
            )
            .ok_or(ElfError::InvalidHeader)?;
        let kind = read_u32(image, header).ok_or(ElfError::InvalidHeader)?;
        if kind == PT_LOAD {
            if segment_count == MAX_LOAD_SEGMENTS {
                return Err(ElfError::InvalidSegment);
            }
            let segment = LoadSegment {
                flags: read_u32(image, header + 4).ok_or(ElfError::InvalidSegment)?,
                offset: read_u64(image, header + 8).ok_or(ElfError::InvalidSegment)?,
                virtual_address: read_u64(image, header + 16).ok_or(ElfError::InvalidSegment)?,
                file_size: read_u64(image, header + 32).ok_or(ElfError::InvalidSegment)?,
                memory_size: read_u64(image, header + 40).ok_or(ElfError::InvalidSegment)?,
            };
            validate_segment(image, segment)?;
            segments[segment_count] = segment;
            segment_count += 1;
        } else if kind == PT_DYNAMIC {
            dynamic = Some((
                read_u64(image, header + 8).ok_or(ElfError::InvalidSegment)?,
                read_u64(image, header + 32).ok_or(ElfError::InvalidSegment)?,
            ));
        }
    }
    if segment_count == 0 {
        return Err(ElfError::InvalidSegment);
    }
    let segments = &segments[..segment_count];
    let min_page = segments
        .iter()
        .map(|segment| align_down(segment.virtual_address, PAGE_SIZE))
        .min()
        .ok_or(ElfError::InvalidSegment)?;
    let load_bias = USER_IMAGE_BASE
        .checked_sub(min_page)
        .ok_or(ElfError::InvalidSegment)?;

    for segment in segments {
        let start = align_down(
            load_bias
                .checked_add(segment.virtual_address)
                .ok_or(ElfError::InvalidSegment)?,
            PAGE_SIZE,
        );
        let end = align_up(
            load_bias
                .checked_add(segment.virtual_address)
                .and_then(|value| value.checked_add(segment.memory_size))
                .ok_or(ElfError::InvalidSegment)?,
            PAGE_SIZE,
        )
        .ok_or(ElfError::InvalidSegment)?;
        let flags = UserPageFlags {
            writable: segment.flags & PF_W != 0,
            executable: segment.flags & PF_X != 0,
        };
        let mut page = start;
        while page < end {
            space
                .map_page(page, flags)
                .map_err(|_| ElfError::AddressSpace)?;
            page += PAGE_SIZE;
        }
    }

    for segment in segments {
        let start = segment.offset as usize;
        let end = start + segment.file_size as usize;
        space
            .copy_into_user(load_bias + segment.virtual_address, &image[start..end])
            .map_err(|_| ElfError::AddressSpace)?;
    }
    if let Some((offset, size)) = dynamic {
        apply_relocations(space, image, segments, load_bias, offset, size)?;
    }

    let stack_bottom = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    for page in 0..USER_STACK_PAGES {
        space
            .map_page(stack_bottom + page * PAGE_SIZE, UserPageFlags::READ_WRITE)
            .map_err(|_| ElfError::AddressSpace)?;
    }
    let entry = load_bias
        .checked_add(entry)
        .ok_or(ElfError::InvalidSegment)?;
    if !space.is_executable(entry) {
        return Err(ElfError::InvalidSegment);
    }
    Ok(LoadedImage {
        entry,
        stack_pointer: crate::arch::initial_user_stack(USER_STACK_TOP),
        thread_pointer: 0,
    })
}

fn validate_segment(image: &[u8], segment: LoadSegment) -> Result<(), ElfError> {
    if segment.file_size > segment.memory_size
        || segment.flags & (PF_W | PF_X) == (PF_W | PF_X)
        || (segment.offset & (PAGE_SIZE - 1)) != (segment.virtual_address & (PAGE_SIZE - 1))
    {
        return Err(if segment.flags & (PF_W | PF_X) == (PF_W | PF_X) {
            ElfError::WritableExecutableSegment
        } else {
            ElfError::InvalidSegment
        });
    }
    let end = segment
        .offset
        .checked_add(segment.file_size)
        .ok_or(ElfError::InvalidSegment)?;
    if end > image.len() as u64 {
        return Err(ElfError::InvalidSegment);
    }
    Ok(())
}

fn apply_relocations(
    space: &AddressSpace,
    image: &[u8],
    segments: &[LoadSegment],
    load_bias: u64,
    dynamic_offset: u64,
    dynamic_size: u64,
) -> Result<(), ElfError> {
    let dynamic_end = dynamic_offset
        .checked_add(dynamic_size)
        .ok_or(ElfError::InvalidSegment)?;
    if dynamic_end > image.len() as u64 {
        return Err(ElfError::InvalidSegment);
    }
    let mut rela_address = 0u64;
    let mut rela_size = 0u64;
    let mut cursor = dynamic_offset;
    while cursor + 16 <= dynamic_end {
        let tag = read_i64(image, cursor as usize).ok_or(ElfError::InvalidSegment)?;
        let value = read_u64(image, cursor as usize + 8).ok_or(ElfError::InvalidSegment)?;
        match tag {
            DT_NULL => break,
            DT_RELA => rela_address = value,
            DT_RELASZ => rela_size = value,
            _ => {}
        }
        cursor += 16;
    }
    if rela_size == 0 {
        return Ok(());
    }
    if !rela_size.is_multiple_of(24) {
        return Err(ElfError::InvalidSegment);
    }
    let rela_offset = virtual_to_file_offset(segments, rela_address)?;
    for index in 0..rela_size / 24 {
        let offset = rela_offset + index * 24;
        let target = read_u64(image, offset as usize).ok_or(ElfError::InvalidSegment)?;
        let info = read_u64(image, offset as usize + 8).ok_or(ElfError::InvalidSegment)?;
        let addend = read_i64(image, offset as usize + 16).ok_or(ElfError::InvalidSegment)?;
        if info as u32 != RELATIVE_RELOCATION {
            return Err(ElfError::UnsupportedRelocation);
        }
        let value = (load_bias as i128)
            .checked_add(addend as i128)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(ElfError::InvalidSegment)?;
        space
            .copy_into_user(load_bias + target, &value.to_le_bytes())
            .map_err(|_| ElfError::AddressSpace)?;
    }
    Ok(())
}

fn virtual_to_file_offset(segments: &[LoadSegment], address: u64) -> Result<u64, ElfError> {
    segments
        .iter()
        .find(|segment| {
            address >= segment.virtual_address
                && address < segment.virtual_address.saturating_add(segment.file_size)
        })
        .map(|segment| segment.offset + address - segment.virtual_address)
        .ok_or(ElfError::InvalidSegment)
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

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

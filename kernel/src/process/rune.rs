//! Loader нативного RUNE v1.
//!
//! В отличие от ELF compatibility loader здесь нет program-header heuristics:
//! page regions, entry RVA, relocation и RELRO уже нормализованы packer'ом.
//! Проверка hash и всей TOC происходит до первого изменения address space.

use rustos_abi::PAGE_SIZE;
use rustos_rune_format::{
    architecture, parse_relocation, record_kind, region_flags, relocation_kind, Container,
    FormatError, TocEntry, RELOCATION_SIZE,
};

use crate::memory::{AddressSpace, UserPageFlags};

use super::{LoadedImage, TlsTemplate};

const USER_IMAGE_BASE: u64 = 0x0000_4000_0000_0000;
const USER_IMAGE_LIMIT: u64 = 0x0000_0001_0000_0000;
const USER_TLS_BASE: u64 = 0x0000_3fff_fffe_0000;
const USER_TLS_LIMIT: u64 = 64 * 1024;
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const USER_STACK_PAGES: u64 = 16;
const MAX_REGIONS: usize = 32;

#[cfg(target_arch = "x86_64")]
const CURRENT_ARCHITECTURE: u16 = architecture::X86_64;
#[cfg(target_arch = "aarch64")]
const CURRENT_ARCHITECTURE: u16 = architecture::AARCH64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuneError {
    Format(FormatError),
    InvalidRegion,
    UnsupportedImports,
    UnsupportedRelocation,
    InvalidRelocation,
    AddressSpace,
}

pub fn load(space: &mut AddressSpace, image: &'static [u8]) -> Result<LoadedImage, RuneError> {
    let container = Container::parse(image).map_err(RuneError::Format)?;
    let slice = container
        .slice(CURRENT_ARCHITECTURE)
        .map_err(RuneError::Format)?;
    validate_container(&container, slice)?;

    for region in regions(&container) {
        map_region(space, region)?;
    }
    for region in regions(&container) {
        let payload = container.payload(region).ok_or(RuneError::InvalidRegion)?;
        space
            .copy_into_user(USER_IMAGE_BASE + region.virtual_address, payload)
            .map_err(|_| RuneError::AddressSpace)?;
    }
    apply_relocations(space, &container)?;
    apply_relro(space, &container)?;
    let (thread_pointer, tls_template) = initialize_tls(space, &container)?;

    let stack_bottom = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    for page in 0..USER_STACK_PAGES {
        space
            .map_page(stack_bottom + page * PAGE_SIZE, UserPageFlags::READ_WRITE)
            .map_err(|_| RuneError::AddressSpace)?;
    }
    let entry = USER_IMAGE_BASE
        .checked_add(slice.virtual_address)
        .ok_or(RuneError::InvalidRegion)?;
    if !space.is_executable(entry) {
        return Err(RuneError::InvalidRegion);
    }
    Ok(LoadedImage {
        entry,
        stack_pointer: crate::arch::initial_user_stack(USER_STACK_TOP),
        thread_pointer,
        tls_template,
    })
}

fn initialize_tls(
    space: &mut AddressSpace,
    container: &Container<'static>,
) -> Result<(u64, Option<TlsTemplate>), RuneError> {
    let mut templates = container.entries().filter(|entry| {
        entry.kind == record_kind::TLS && entry.architecture == CURRENT_ARCHITECTURE
    });
    let Some(template) = templates.next() else {
        return Ok((0, None));
    };
    if templates.next().is_some()
        || template.memory_size > USER_TLS_LIMIT - 16
        || template.alignment == 0
        || template.alignment > PAGE_SIZE
    {
        return Err(RuneError::InvalidRegion);
    }
    let tls_size = align_up(template.memory_size, template.alignment)?;
    let storage_size = align_up(
        tls_size.checked_add(16).ok_or(RuneError::InvalidRegion)?,
        PAGE_SIZE,
    )?;
    if storage_size > USER_TLS_LIMIT {
        return Err(RuneError::InvalidRegion);
    }
    for offset in (0..storage_size).step_by(PAGE_SIZE as usize) {
        space
            .map_page(USER_TLS_BASE + offset, UserPageFlags::READ_WRITE)
            .map_err(|_| RuneError::AddressSpace)?;
    }

    #[cfg(target_arch = "x86_64")]
    let thread_pointer = USER_TLS_BASE + tls_size;
    #[cfg(target_arch = "aarch64")]
    let thread_pointer = USER_TLS_BASE;

    let payload = container
        .payload(template)
        .ok_or(RuneError::InvalidRegion)?;
    #[cfg(target_arch = "x86_64")]
    let template_address = thread_pointer
        .checked_sub(template.memory_size)
        .ok_or(RuneError::InvalidRegion)?;
    #[cfg(target_arch = "aarch64")]
    let template_address = thread_pointer + 16;
    space
        .copy_into_user(template_address, payload)
        .map_err(|_| RuneError::AddressSpace)?;
    // AMD64 variant-II TCB: %fs:0 хранит self pointer. AArch64 резервирует
    // первые 16 байт TCB перед variant-I TLS template.
    space
        .copy_into_user(thread_pointer, &thread_pointer.to_le_bytes())
        .map_err(|_| RuneError::AddressSpace)?;
    Ok((
        thread_pointer,
        Some(TlsTemplate {
            bytes: payload,
            memory_size: template.memory_size,
            alignment: template.alignment,
        }),
    ))
}

fn validate_container(container: &Container<'_>, _slice: TocEntry) -> Result<(), RuneError> {
    let mut region_count = 0usize;
    for entry in container.entries() {
        if entry.architecture != architecture::ANY && entry.architecture != CURRENT_ARCHITECTURE {
            continue;
        }
        match entry.kind {
            record_kind::REGION => {
                region_count += 1;
                if region_count > MAX_REGIONS
                    || entry.memory_size == 0
                    || entry.alignment < PAGE_SIZE
                    || entry.virtual_address >= USER_IMAGE_LIMIT
                    || entry
                        .virtual_address
                        .checked_add(entry.memory_size)
                        .filter(|end| *end <= USER_IMAGE_LIMIT)
                        .is_none()
                    || entry.flags & region_flags::WRITE != 0
                        && entry.flags & region_flags::EXECUTE != 0
                {
                    return Err(RuneError::InvalidRegion);
                }
            }
            record_kind::IMPORTS | record_kind::DEPENDENCIES => {
                if entry.file_size != 0 {
                    return Err(RuneError::UnsupportedImports);
                }
            }
            _ => {}
        }
    }
    if region_count == 0 {
        return Err(RuneError::InvalidRegion);
    }
    // Page ranges не могут пересекаться: иначе порядок TOC влиял бы на права.
    for (left_index, left) in regions(container).enumerate() {
        let left_start = align_down(left.virtual_address, PAGE_SIZE);
        let left_end = align_up(
            left.virtual_address
                .checked_add(left.memory_size)
                .ok_or(RuneError::InvalidRegion)?,
            PAGE_SIZE,
        )?;
        for right in regions(container).skip(left_index + 1) {
            let right_start = align_down(right.virtual_address, PAGE_SIZE);
            let right_end = align_up(
                right
                    .virtual_address
                    .checked_add(right.memory_size)
                    .ok_or(RuneError::InvalidRegion)?,
                PAGE_SIZE,
            )?;
            if left_start < right_end && right_start < left_end {
                return Err(RuneError::InvalidRegion);
            }
        }
    }
    Ok(())
}

fn regions<'a>(container: &'a Container<'_>) -> impl Iterator<Item = TocEntry> + 'a {
    container.entries().filter(|entry| {
        entry.kind == record_kind::REGION && entry.architecture == CURRENT_ARCHITECTURE
    })
}

fn map_region(space: &mut AddressSpace, region: TocEntry) -> Result<(), RuneError> {
    let start = USER_IMAGE_BASE
        .checked_add(align_down(region.virtual_address, PAGE_SIZE))
        .ok_or(RuneError::InvalidRegion)?;
    let end_rva = align_up(
        region
            .virtual_address
            .checked_add(region.memory_size)
            .ok_or(RuneError::InvalidRegion)?,
        PAGE_SIZE,
    )?;
    let end = USER_IMAGE_BASE
        .checked_add(end_rva)
        .ok_or(RuneError::InvalidRegion)?;
    let flags = UserPageFlags {
        writable: region.flags & region_flags::WRITE != 0,
        executable: region.flags & region_flags::EXECUTE != 0,
    };
    let mut page = start;
    while page < end {
        space
            .map_page(page, flags)
            .map_err(|_| RuneError::AddressSpace)?;
        page += PAGE_SIZE;
    }
    Ok(())
}

fn apply_relocations(space: &AddressSpace, container: &Container<'_>) -> Result<(), RuneError> {
    for table in container.entries().filter(|entry| {
        entry.kind == record_kind::RELOCATIONS && entry.architecture == CURRENT_ARCHITECTURE
    }) {
        let bytes = container
            .payload(table)
            .ok_or(RuneError::InvalidRelocation)?;
        if !bytes.len().is_multiple_of(RELOCATION_SIZE) {
            return Err(RuneError::InvalidRelocation);
        }
        for raw in bytes.chunks_exact(RELOCATION_SIZE) {
            let relocation = parse_relocation(raw).ok_or(RuneError::InvalidRelocation)?;
            if relocation.kind != relocation_kind::RELATIVE64 || relocation.symbol != 0 {
                return Err(RuneError::UnsupportedRelocation);
            }
            let target = USER_IMAGE_BASE
                .checked_add(relocation.offset)
                .ok_or(RuneError::InvalidRelocation)?;
            if !writable_target(container, relocation.offset, 8) {
                return Err(RuneError::InvalidRelocation);
            }
            let value = (USER_IMAGE_BASE as i128)
                .checked_add(relocation.addend as i128)
                .and_then(|value| u64::try_from(value).ok())
                .ok_or(RuneError::InvalidRelocation)?;
            space
                .copy_into_user(target, &value.to_le_bytes())
                .map_err(|_| RuneError::AddressSpace)?;
        }
    }
    Ok(())
}

fn writable_target(container: &Container<'_>, offset: u64, length: u64) -> bool {
    regions(container).any(|region| {
        region.flags & region_flags::WRITE != 0
            && offset >= region.virtual_address
            && offset
                .checked_add(length)
                .is_some_and(|end| end <= region.virtual_address.saturating_add(region.memory_size))
    })
}

fn apply_relro(space: &mut AddressSpace, container: &Container<'_>) -> Result<(), RuneError> {
    for relro in container.entries().filter(|entry| {
        entry.kind == record_kind::RELRO && entry.architecture == CURRENT_ARCHITECTURE
    }) {
        if relro.memory_size == 0 {
            continue;
        }
        let start = USER_IMAGE_BASE
            .checked_add(align_down(relro.virtual_address, PAGE_SIZE))
            .ok_or(RuneError::InvalidRegion)?;
        let end = USER_IMAGE_BASE
            .checked_add(align_up(
                relro
                    .virtual_address
                    .checked_add(relro.memory_size)
                    .ok_or(RuneError::InvalidRegion)?,
                PAGE_SIZE,
            )?)
            .ok_or(RuneError::InvalidRegion)?;
        let mut page = start;
        while page < end {
            space
                .protect_page(
                    page,
                    UserPageFlags {
                        writable: false,
                        executable: false,
                    },
                )
                .map_err(|_| RuneError::AddressSpace)?;
            page += PAGE_SIZE;
        }
    }
    Ok(())
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Result<u64, RuneError> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(RuneError::InvalidRegion)
}

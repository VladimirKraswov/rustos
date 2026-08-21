//! Kernel objects графической памяти.
//!
//! Здесь нет оконной или display policy: объект только владеет физическими
//! кадрами, неизменяемым descriptor'ом и двумя независимыми reference counts.
//! Mapping не становится висячим при закрытии capability, а capability не
//! освобождает память, пока кадры ещё отображены в процессе.

use rustos_abi::{
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain},
    memory::VmFlags,
    syscall::status,
    PAGE_SIZE,
};

use crate::memory::{self, FrameBlock};

/// Текущий bootstrap budget поддерживает несколько full-HD/4K staging
/// buffers, но не позволяет одному клиенту поглотить всю RAM машины 128 MiB.
pub(super) const MAX_GRAPHICS_BUFFERS: usize = 6;
pub(super) const MAX_GRAPHICS_BUFFER_PAGES: usize = 8 * 1024;
const MAX_GRAPHICS_TOTAL_PAGES: usize = 24 * 1024;
const MAX_GRAPHICS_EXTENTS: usize = 256;

const EMPTY_FRAME: FrameBlock = FrameBlock { phys: 0, frames: 0 };

#[derive(Clone, Copy)]
struct GraphicsBufferObject {
    generation: u8,
    used: bool,
    descriptor: GraphicsBufferDesc,
    extents: [FrameBlock; MAX_GRAPHICS_EXTENTS],
    extent_count: usize,
    pages: usize,
    capability_refs: usize,
    mapping_refs: usize,
    maximum_flags: VmFlags,
}

impl GraphicsBufferObject {
    const EMPTY: Self = Self {
        generation: 1,
        used: false,
        // Descriptor пустого slot никогда не пересекает ABI. Zero-value
        // нужен только для const-инициализации статической таблицы.
        // SAFETY: descriptor пустого slot не читается до `used=true`; все
        // входящие descriptors полностью перезаписывают это значение.
        descriptor: unsafe { core::mem::zeroed() },
        extents: [EMPTY_FRAME; MAX_GRAPHICS_EXTENTS],
        extent_count: 0,
        pages: 0,
        capability_refs: 0,
        mapping_refs: 0,
        maximum_flags: VmFlags(0),
    };
}

pub(super) struct GraphicsBufferPool {
    objects: [GraphicsBufferObject; MAX_GRAPHICS_BUFFERS],
    allocated_pages: usize,
}

impl GraphicsBufferPool {
    pub(super) const fn new() -> Self {
        Self {
            objects: [GraphicsBufferObject::EMPTY; MAX_GRAPHICS_BUFFERS],
            allocated_pages: 0,
        }
    }

    /// Создаёт system-memory buffer. Device-local/protected backing появится
    /// через отдельный driver allocator и не маскируется обычной RAM.
    pub(super) fn create(&mut self, descriptor: GraphicsBufferDesc) -> Result<u16, i64> {
        descriptor
            .validate()
            .map_err(|_| status::INVALID_ARGUMENT)?;
        if !descriptor.memory_domains.contains(MemoryDomain::SYSTEM)
            || !descriptor
                .memory_domains
                .contains(MemoryDomain::HOST_VISIBLE)
            || descriptor
                .memory_domains
                .contains(MemoryDomain::DEVICE_LOCAL)
            || descriptor.memory_domains.contains(MemoryDomain::PROTECTED)
            || !descriptor.usage.contains(BufferUsage::CPU_READ)
            || descriptor.modifier != rustos_abi::graphics_buffer::modifier::LINEAR
        {
            return Err(status::NOT_SUPPORTED);
        }
        let pages_u64 = descriptor.byte_size.div_ceil(PAGE_SIZE);
        let pages = usize::try_from(pages_u64).map_err(|_| status::LIMIT_REACHED)?;
        if pages == 0
            || pages > MAX_GRAPHICS_BUFFER_PAGES
            || self
                .allocated_pages
                .checked_add(pages)
                .is_none_or(|total| total > MAX_GRAPHICS_TOTAL_PAGES)
        {
            return Err(status::LIMIT_REACHED);
        }
        let index = self
            .objects
            .iter()
            .position(|object| !object.used)
            .ok_or(status::LIMIT_REACHED)?;
        let generation = self.objects[index].generation;
        let object = &mut self.objects[index];
        object.used = true;
        object.descriptor = descriptor;
        object.pages = 0;
        object.extent_count = 0;
        object.capability_refs = 0;
        object.mapping_refs = 0;
        let mut remaining = pages as u64;
        while remaining != 0 {
            if object.extent_count == MAX_GRAPHICS_EXTENTS {
                Self::rollback_unpublished(object, generation);
                return Err(status::OUT_OF_MEMORY);
            }
            // Сначала просим крупный contiguous extent — это ускоряет mapping
            // и DMA import. При фрагментации двоично уменьшаем запрос вплоть
            // до одной страницы, сохраняя scatter/gather semantics.
            let mut extent_pages = remaining;
            let block = loop {
                match memory::allocate(extent_pages, 1) {
                    Ok(block) => break Some(block),
                    Err(_) if extent_pages > 1 => extent_pages = extent_pages.div_ceil(2),
                    Err(_) => break None,
                }
            };
            match block {
                Some(block) => {
                    // SAFETY: allocator вернул identity-mapped page в
                    // исключительное владение ещё не опубликованного object.
                    unsafe {
                        (block.phys as *mut u8)
                            .write_bytes(0, block.frames as usize * PAGE_SIZE as usize)
                    };
                    object.extents[object.extent_count] = block;
                    object.extent_count += 1;
                    object.pages += block.frames as usize;
                    remaining -= block.frames;
                }
                None => {
                    Self::rollback_unpublished(object, generation);
                    return Err(status::OUT_OF_MEMORY);
                }
            }
        }
        let maximum_flags = if descriptor.usage.contains(BufferUsage::CPU_WRITE) {
            VmFlags::READ.union(VmFlags::WRITE)
        } else {
            VmFlags::READ
        };
        object.capability_refs = 1;
        object.maximum_flags = maximum_flags;
        self.allocated_pages += pages;
        Ok(make_id(index, generation))
    }

    pub(super) fn descriptor(&self, id: u16) -> Result<GraphicsBufferDesc, i64> {
        Ok(self.get(id)?.descriptor)
    }

    pub(super) fn pages_and_flags(&self, id: u16) -> Result<(usize, VmFlags), i64> {
        let object = self.get(id)?;
        Ok((object.pages, object.maximum_flags))
    }

    pub(super) fn physical_page(&self, id: u16, page: usize) -> Result<u64, i64> {
        let object = self.get(id)?;
        if page >= object.pages {
            return Err(status::INVALID_ARGUMENT);
        }
        let mut base_page = 0usize;
        for extent in object.extents.iter().take(object.extent_count) {
            let extent_pages = extent.frames as usize;
            if page < base_page + extent_pages {
                return Ok(extent.phys + (page - base_page) as u64 * PAGE_SIZE);
            }
            base_page += extent_pages;
        }
        Err(status::BAD_HANDLE)
    }

    pub(super) fn retain_capability(&mut self, id: u16) -> Result<(), i64> {
        let object = self.get_mut(id)?;
        object.capability_refs = object
            .capability_refs
            .checked_add(1)
            .ok_or(status::LIMIT_REACHED)?;
        Ok(())
    }

    pub(super) fn release_capability(&mut self, id: u16) {
        let index = id_index(id);
        let Ok(object) = self.get_mut(id) else { return };
        object.capability_refs = object.capability_refs.saturating_sub(1);
        self.destroy_if_unused(index);
    }

    pub(super) fn retain_mapping(&mut self, id: u16) -> Result<(), i64> {
        let object = self.get_mut(id)?;
        object.mapping_refs = object
            .mapping_refs
            .checked_add(1)
            .ok_or(status::LIMIT_REACHED)?;
        Ok(())
    }

    pub(super) fn release_mappings(&mut self, id: u16, count: usize) {
        let index = id_index(id);
        let Ok(object) = self.get_mut(id) else { return };
        object.mapping_refs = object.mapping_refs.saturating_sub(count);
        self.destroy_if_unused(index);
    }

    pub(super) fn generation_at(&self, index: usize) -> u8 {
        self.objects[index].generation
    }

    pub(super) fn cleanup(&mut self) {
        for index in 0..MAX_GRAPHICS_BUFFERS {
            if self.objects[index].used {
                self.objects[index].capability_refs = 0;
                self.objects[index].mapping_refs = 0;
                self.destroy_if_unused(index);
            }
        }
    }

    fn get(&self, id: u16) -> Result<&GraphicsBufferObject, i64> {
        let object = self.objects.get(id_index(id)).ok_or(status::BAD_HANDLE)?;
        if !object.used || object.generation != id_generation(id) {
            return Err(status::BAD_HANDLE);
        }
        Ok(object)
    }

    fn get_mut(&mut self, id: u16) -> Result<&mut GraphicsBufferObject, i64> {
        let object = self
            .objects
            .get_mut(id_index(id))
            .ok_or(status::BAD_HANDLE)?;
        if !object.used || object.generation != id_generation(id) {
            return Err(status::BAD_HANDLE);
        }
        Ok(object)
    }

    fn destroy_if_unused(&mut self, index: usize) {
        let object = &mut self.objects[index];
        if !object.used || object.capability_refs != 0 || object.mapping_refs != 0 {
            return;
        }
        for extent in object.extents.iter().take(object.extent_count) {
            let _ = memory::free(*extent);
        }
        self.allocated_pages = self.allocated_pages.saturating_sub(object.pages);
        let generation = next_generation(object.generation);
        *object = GraphicsBufferObject::EMPTY;
        object.generation = generation;
    }

    fn rollback_unpublished(object: &mut GraphicsBufferObject, generation: u8) {
        for extent in object.extents.iter().take(object.extent_count) {
            let _ = memory::free(*extent);
        }
        *object = GraphicsBufferObject::EMPTY;
        object.generation = generation;
    }
}

pub(super) const fn make_id(index: usize, generation: u8) -> u16 {
    ((generation as u16) << 8) | index as u16
}

const fn id_index(id: u16) -> usize {
    (id & 0xff) as usize
}

const fn id_generation(id: u16) -> u8 {
    (id >> 8) as u8
}

const fn next_generation(generation: u8) -> u8 {
    let next = generation.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

//! Системный allocator `std` поверх анонимных VM mappings.
//!
//! На этом этапе одна allocation занимает целое число страниц. Это простая и
//! корректная база; позднее user-space `alloc.dll` добавит slab/size classes,
//! не меняя API стандартной библиотеки.

use crate::alloc::Layout;
use crate::ptr;
use crate::sys::pal::syscall3;

const PAGE_SIZE: usize = 4096;
const VM_MAP: u64 = 12;
const VM_UNMAP: u64 = 13;

#[repr(C)]
struct VmMapRequest {
    version: u32,
    reserved: u32,
    address: u64,
    length: u64,
    flags: u64,
}

pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    let size = match layout.size().max(1).checked_add(PAGE_SIZE - 1) {
        Some(value) => value & !(PAGE_SIZE - 1),
        None => return ptr::null_mut(),
    };
    // vm_map возвращает page-aligned адрес. Редкие over-aligned allocations
    // будут добавлены вместе с частичным unmap; молча нарушать Layout нельзя.
    if layout.align() > PAGE_SIZE {
        return ptr::null_mut();
    }
    let request = VmMapRequest {
        version: 1,
        reserved: 0,
        address: 0,
        length: size as u64,
        flags: 3, // READ | WRITE, W^X проверяет kernel.
    };
    let result = unsafe { syscall3(VM_MAP, ptr::from_ref(&request).addr() as u64, 0, 0) };
    if result > 0 {
        result as *mut u8
    } else {
        ptr::null_mut()
    }
}

pub unsafe fn dealloc(pointer: *mut u8, layout: Layout) {
    let size = layout.size().max(1).saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let _ = unsafe { syscall3(VM_UNMAP, pointer.addr() as u64, size as u64, 0) };
}

pub unsafe fn realloc(pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    unsafe { super::realloc_fallback(pointer, layout, new_size) }
}

pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // ABI гарантирует zero-filled private pages.
    unsafe { alloc(layout) }
}

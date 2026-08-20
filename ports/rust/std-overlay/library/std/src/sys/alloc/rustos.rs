//! Системный allocator `std` поверх VM микроядра.
//!
//! Малые объекты обслуживаются page slabs восьми size classes, поэтому Vec,
//! BTreeMap и rustc AST не расходуют отдельный физический кадр на каждый Box.
//! Большие и over-aligned объекты получают собственный mapping с маленьким
//! header перед выровненным указателем. Внешний ABI остаётся обычным Rust
//! GlobalAlloc, а allocator не зависит от libc или отдельного процесса.

use crate::alloc::Layout;
use crate::cell::UnsafeCell;
use crate::ptr;
use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sys::pal::syscall3;

const PAGE_SIZE: usize = 4096;
const VM_MAP: u64 = 12;
const VM_UNMAP: u64 = 13;
const THREAD_YIELD: u64 = 0;
const CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

#[repr(C)]
struct VmMapRequest {
    version: u32,
    reserved: u32,
    address: u64,
    length: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LargeHeader {
    mapping: *mut u8,
    length: usize,
}

struct AllocatorState {
    free: [*mut u8; CLASSES.len()],
}

struct SharedState(UnsafeCell<AllocatorState>);
unsafe impl Sync for SharedState {}

static LOCK: AtomicBool = AtomicBool::new(false);
static STATE: SharedState = SharedState(UnsafeCell::new(AllocatorState {
    free: [ptr::null_mut(); CLASSES.len()],
}));

struct Guard;

impl Guard {
    fn lock() -> Self {
        while LOCK
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            unsafe { syscall3(THREAD_YIELD, 0, 0, 0) };
        }
        Self
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    if let Some((class_index, class_size)) = size_class(layout) {
        let _guard = Guard::lock();
        let state = unsafe { &mut *STATE.0.get() };
        if state.free[class_index].is_null() {
            let page = map_pages(PAGE_SIZE);
            if page.is_null() {
                return ptr::null_mut();
            }
            // Каждый свободный block хранит указатель следующего прямо в себе.
            // Первый block возвращаем caller'у, остальные включаем в freelist.
            for offset in (class_size..PAGE_SIZE).step_by(class_size) {
                let block = unsafe { page.add(offset) };
                unsafe { block.cast::<*mut u8>().write(state.free[class_index]) };
                state.free[class_index] = block;
            }
            return page;
        }
        let block = state.free[class_index];
        state.free[class_index] = unsafe { block.cast::<*mut u8>().read() };
        return block;
    }
    large_alloc(layout)
}

pub unsafe fn dealloc(pointer: *mut u8, layout: Layout) {
    if pointer.is_null() {
        return;
    }
    if let Some((class_index, _)) = size_class(layout) {
        let _guard = Guard::lock();
        let state = unsafe { &mut *STATE.0.get() };
        unsafe { pointer.cast::<*mut u8>().write(state.free[class_index]) };
        state.free[class_index] = pointer;
        return;
    }
    let header = unsafe {
        pointer
            .sub(core::mem::size_of::<LargeHeader>())
            .cast::<LargeHeader>()
            .read()
    };
    let _ = unsafe {
        syscall3(
            VM_UNMAP,
            header.mapping.addr() as u64,
            header.length as u64,
            0,
        )
    };
}

pub unsafe fn realloc(pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    unsafe { super::realloc_fallback(pointer, layout, new_size) }
}

pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    let pointer = unsafe { alloc(layout) };
    if !pointer.is_null() {
        // Slab blocks могут использоваться повторно; zero-filled гарантия VM
        // действует лишь при первом выделении страницы.
        unsafe { pointer.write_bytes(0, layout.size()) };
    }
    pointer
}

fn size_class(layout: Layout) -> Option<(usize, usize)> {
    let required = layout.size().max(1).max(layout.align());
    CLASSES
        .iter()
        .copied()
        .enumerate()
        .find(|(_, class)| *class >= required)
}

fn large_alloc(layout: Layout) -> *mut u8 {
    let alignment = layout.align().max(core::mem::align_of::<LargeHeader>());
    let total = match layout
        .size()
        .max(1)
        .checked_add(alignment)
        .and_then(|value| value.checked_add(core::mem::size_of::<LargeHeader>()))
        .and_then(|value| align_up(value, PAGE_SIZE))
    {
        Some(total) => total,
        None => return ptr::null_mut(),
    };
    let mapping = map_pages(total);
    if mapping.is_null() {
        return mapping;
    }
    let start = mapping.addr() + core::mem::size_of::<LargeHeader>();
    let Some(aligned) = align_up(start, alignment) else {
        let _ = unsafe { syscall3(VM_UNMAP, mapping.addr() as u64, total as u64, 0) };
        return ptr::null_mut();
    };
    let pointer = aligned as *mut u8;
    let header = LargeHeader {
        mapping,
        length: total,
    };
    unsafe {
        pointer
            .sub(core::mem::size_of::<LargeHeader>())
            .cast::<LargeHeader>()
            .write(header)
    };
    pointer
}

fn map_pages(length: usize) -> *mut u8 {
    let request = VmMapRequest {
        version: 1,
        reserved: 0,
        address: 0,
        length: length as u64,
        flags: 3,
    };
    let result = unsafe { syscall3(VM_MAP, ptr::from_ref(&request).addr() as u64, 0, 0) };
    if result > 0 {
        result as *mut u8
    } else {
        ptr::null_mut()
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return None;
    }
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

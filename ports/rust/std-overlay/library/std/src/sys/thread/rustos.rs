//! Native threads RustOS поверх process/thread/vm ABI микроядра.

use crate::ffi::CStr;
use crate::num::NonZero;
use crate::thread::ThreadInit;
use crate::time::Duration;
use crate::{cmp, io, ptr};

const PAGE_SIZE: usize = 4096;
const MIN_STACK_SIZE: usize = 64 * 1024;
pub const DEFAULT_MIN_STACK_SIZE: usize = 256 * 1024;

const PROCESS_ABI_VERSION: u32 = 2;
const PRIORITY_INTERACTIVE: u8 = 3;
const SYSCALL_THREAD_YIELD: u64 = 0;
const SYSCALL_THREAD_CREATE: u64 = 8;
const SYSCALL_THREAD_EXIT: u64 = 9;
const SYSCALL_THREAD_JOIN: u64 = 10;
const SYSCALL_VM_MAP: u64 = 12;
const SYSCALL_VM_UNMAP: u64 = 13;
const SYSCALL_CLOCK_MONOTONIC: u64 = 18;
const SYSCALL_THREAD_DETACH: u64 = 26;
const STATUS_OK: i64 = 0;

#[repr(C)]
struct VmMapRequest {
    version: u32,
    reserved: u32,
    address: u64,
    length: u64,
    flags: u64,
}

#[repr(C)]
struct ThreadCreateRequest {
    version: u32,
    flags: u32,
    entry: u64,
    stack_pointer: u64,
    argument: u64,
    thread_pointer: u64,
    reclaim_address: u64,
    reclaim_length: u64,
    priority: u8,
    reserved: [u8; 7],
}

#[repr(C)]
struct ThreadCreateResult {
    thread: u32,
    reserved: u32,
    tid: u64,
}

#[repr(C)]
struct ExitReason {
    status: i32,
    exception: u16,
    flags: u16,
    fault_address: u64,
}

pub struct Thread {
    handle: u32,
    joined: bool,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

impl Thread {
    // unsafe: см. safety contract `thread::Builder::spawn_unchecked`.
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        let stack_size = align_up(cmp::max(stack, MIN_STACK_SIZE), PAGE_SIZE)
            .ok_or_else(|| io::const_error!(io::ErrorKind::OutOfMemory, "thread stack overflow"))?;
        let (template, file_size, memory_size, tls_alignment, variant) =
            crate::sys::pal::rustos_tls_template().ok_or_else(|| {
                io::const_error!(io::ErrorKind::InvalidData, "invalid RustOS TLS template")
            })?;
        let tls_size = align_up(memory_size, tls_alignment.max(1))
            .and_then(|size| size.checked_add(16))
            .and_then(|size| align_up(size, PAGE_SIZE))
            .ok_or_else(|| io::const_error!(io::ErrorKind::OutOfMemory, "thread TLS overflow"))?;
        let mapping_size = tls_size.checked_add(stack_size).ok_or_else(|| {
            io::const_error!(io::ErrorKind::OutOfMemory, "thread mapping overflow")
        })?;
        let mapping = map_rw(mapping_size)?;

        let thread_pointer = match variant {
            2 => mapping + align_up(memory_size, tls_alignment.max(1)).unwrap_or(memory_size),
            1 => mapping,
            0 => 0,
            _ => {
                unmap(mapping, mapping_size);
                return Err(io::const_error!(
                    io::ErrorKind::InvalidData,
                    "unknown TLS variant"
                ));
            }
        };
        if variant == 2 {
            let destination = (thread_pointer - memory_size) as *mut u8;
            if file_size != 0 {
                // SAFETY: template находится в immutable startup block, а
                // destination — в только что созданном RW mapping.
                unsafe { ptr::copy_nonoverlapping(template, destination, file_size) };
            }
            // AMD64 variant-II TCB: %fs:0 содержит self pointer.
            unsafe { (thread_pointer as *mut usize).write(thread_pointer) };
        } else if variant == 1 {
            if file_size != 0 {
                unsafe {
                    ptr::copy_nonoverlapping(template, (thread_pointer + 16) as *mut u8, file_size)
                };
            }
            unsafe { (thread_pointer as *mut usize).write(thread_pointer) };
        }

        let data = Box::into_raw(init);
        let stack_top = mapping + mapping_size;
        #[cfg(target_arch = "x86_64")]
        let stack_pointer = stack_top - 8;
        #[cfg(target_arch = "aarch64")]
        let stack_pointer = stack_top;
        let request = ThreadCreateRequest {
            version: PROCESS_ABI_VERSION,
            flags: 0,
            entry: thread_entry as *const () as u64,
            stack_pointer: stack_pointer as u64,
            argument: data.addr() as u64,
            thread_pointer: thread_pointer as u64,
            reclaim_address: mapping as u64,
            reclaim_length: mapping_size as u64,
            priority: PRIORITY_INTERACTIVE,
            reserved: [0; 7],
        };
        let mut result = ThreadCreateResult {
            thread: 0,
            reserved: 0,
            tid: 0,
        };
        let status = unsafe {
            crate::sys::pal::syscall3(
                SYSCALL_THREAD_CREATE,
                ptr::from_ref(&request).addr() as u64,
                ptr::from_mut(&mut result).addr() as u64,
                0,
            )
        };
        if status != STATUS_OK {
            // Kernel не принял ownership reclaim-диапазона.
            unsafe { drop(Box::from_raw(data)) };
            unmap(mapping, mapping_size);
            return Err(io::const_error!(
                io::ErrorKind::ResourceBusy,
                "thread_create failed"
            ));
        }
        Ok(Thread {
            handle: result.thread,
            joined: false,
        })
    }

    pub fn join(mut self) {
        let mut reason = ExitReason {
            status: 0,
            exception: 0,
            flags: 0,
            fault_address: 0,
        };
        let status = unsafe {
            crate::sys::pal::syscall3(
                SYSCALL_THREAD_JOIN,
                self.handle as u64,
                ptr::from_mut(&mut reason).addr() as u64,
                0,
            )
        };
        if status != STATUS_OK || reason.exception != 0 {
            crate::sys::pal::abort_internal();
        }
        self.joined = true;
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        if !self.joined {
            // Последний detach-handle даёт kernel право reclaim после exit;
            // текущий стек никогда не удаляется из-под работающего потока.
            let _ = unsafe {
                crate::sys::pal::syscall3(SYSCALL_THREAD_DETACH, self.handle as u64, 0, 0)
            };
        }
    }
}

extern "C" fn thread_entry(data: u64) -> ! {
    // SAFETY: указатель получен из Box::into_raw непосредственно перед
    // успешным thread_create и принадлежит ровно этому новому потоку.
    let init = unsafe { Box::from_raw(data as *mut ThreadInit) };
    let rust_start = init.init();
    rust_start();
    unsafe { crate::sys::thread_local::destructors::run() };
    crate::rt::thread_cleanup();
    unsafe { crate::sys::pal::syscall3(SYSCALL_THREAD_EXIT, 0, 0, 0) };
    crate::sys::pal::abort_internal()
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    // AP-ядра пока parked: честно сообщаем число CPU, реально исполняющих
    // user threads, чтобы rayon/rustc не создавали бесполезные workers.
    Ok(NonZero::new(1).unwrap())
}

pub fn current_os_id() -> Option<u64> {
    None
}

pub fn yield_now() {
    unsafe { crate::sys::pal::syscall3(SYSCALL_THREAD_YIELD, 0, 0, 0) };
}

pub fn set_name(_name: &CStr) {}

pub fn sleep(duration: Duration) {
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    let start = monotonic_ns();
    let deadline = start.saturating_add(nanos);
    while monotonic_ns() < deadline {
        yield_now();
    }
}

fn monotonic_ns() -> u64 {
    unsafe { crate::sys::pal::syscall3(SYSCALL_CLOCK_MONOTONIC, 0, 0, 0).max(0) as u64 }
}

fn map_rw(length: usize) -> io::Result<usize> {
    let request = VmMapRequest {
        version: 1,
        reserved: 0,
        address: 0,
        length: length as u64,
        flags: 3,
    };
    let result = unsafe {
        crate::sys::pal::syscall3(SYSCALL_VM_MAP, ptr::from_ref(&request).addr() as u64, 0, 0)
    };
    if result > 0 {
        Ok(result as usize)
    } else {
        Err(io::const_error!(
            io::ErrorKind::OutOfMemory,
            "cannot map thread stack/TLS"
        ))
    }
}

fn unmap(address: usize, length: usize) {
    unsafe { crate::sys::pal::syscall3(SYSCALL_VM_UNMAP, address as u64, length as u64, 0) };
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return None;
    }
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

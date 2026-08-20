//! Дочерняя сторона ring-3 ABI-теста: проверяет args/env bootstrap block и
//! shared-memory capability, переданный атомарно вместе с process_spawn.

#![no_std]
#![no_main]

use core::{panic::PanicInfo, slice};

use rustos_runtime::{
    handle_close, process_exit, process_start_info, shared_memory_map, syscall, Handle,
    SharedMemoryMap, VmFlags,
};

const SHARED_SLOT: Handle = Handle(3);
const PAGE_SIZE: u64 = 4096;

#[no_mangle]
pub extern "C" fn _start(start_address: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(201);
    }
    let Some(info) = (unsafe { process_start_info(start_address) }) else {
        process_exit(202);
    };
    let arguments = unsafe {
        slice::from_raw_parts(
            info.arguments_address as *const u8,
            info.arguments_length as usize,
        )
    };
    let environment = unsafe {
        slice::from_raw_parts(
            info.environment_address as *const u8,
            info.environment_length as usize,
        )
    };
    let shared_mode = arguments == b"abi-child\0shared-test\0";
    let spin_mode = arguments == b"abi-child\0spin\0";
    if info.argument_count != 2
        || (!shared_mode && !spin_mode)
        || info.environment_count != 1
        || environment != b"RUSTOS_TEST=1\0"
    {
        process_exit(203);
    }
    if spin_mode {
        loop {
            core::hint::spin_loop();
        }
    }
    let request = SharedMemoryMap {
        version: rustos_abi::memory::MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        offset: 0,
        length: PAGE_SIZE,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };
    let mapping = shared_memory_map(SHARED_SLOT, &request);
    if mapping <= 0 {
        process_exit(204);
    }
    let word = mapping as *mut u64;
    if unsafe { word.read_volatile() } != 0x5255_5354_4f53_0001 {
        process_exit(205);
    }
    unsafe { word.write_volatile(0x5255_5354_4f53_0002) };
    // Mapping намеренно оставляем до process reap: kernel обязан снять его
    // reference независимо от дисциплины пользовательской программы.
    if handle_close(SHARED_SLOT) != syscall::status::OK {
        process_exit(206);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(209)
}

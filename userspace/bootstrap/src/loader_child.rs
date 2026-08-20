//! Второй процесс отображает тот же sealed RX object и исполняет общую
//! физическую страницу DLL. Writable data/GOT ему не передаются.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use rustos_abi::{memory::MEMORY_ABI_VERSION, syscall};
use rustos_runtime::{
    process_exit, process_start_info, shared_memory_map, Handle, SharedMemoryMap, VmFlags,
};

const SHARED_CODE_SLOT: Handle = Handle(5);

#[no_mangle]
pub extern "C" fn _start(start_info: u64, abi_version: u64, _unused: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(190);
    }
    let Some(info) = (unsafe { process_start_info(start_info) }) else {
        process_exit(191);
    };
    if info.argument_count != 1 || info.arguments_length == 0 {
        process_exit(192);
    }
    let arguments = unsafe {
        core::slice::from_raw_parts(
            info.arguments_address as *const u8,
            info.arguments_length as usize,
        )
    };
    let Some((offset, length)) = parse_mapping(arguments) else {
        process_exit(193);
    };
    let request = SharedMemoryMap {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        offset: 0,
        length,
        flags: VmFlags::READ.union(VmFlags::EXECUTE),
    };
    let mapping = shared_memory_map(SHARED_CODE_SLOT, &request);
    if mapping <= 0 || offset >= length {
        process_exit(194);
    }
    let entry = (mapping as u64 + offset) as usize;
    let function: extern "C" fn() -> u64 = unsafe { core::mem::transmute(entry) };
    if function() != 41 {
        process_exit(195);
    }
    process_exit(0)
}

fn parse_mapping(bytes: &[u8]) -> Option<(u64, u64)> {
    let bytes = bytes.strip_suffix(&[0])?;
    let separator = bytes.iter().position(|byte| *byte == b':')?;
    Some((
        parse_decimal(&bytes[..separator])?,
        parse_decimal(&bytes[separator + 1..])?,
    ))
}

fn parse_decimal(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0u64, |value, byte| {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        value.checked_mul(10)?.checked_add(u64::from(digit))
    })
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(199)
}

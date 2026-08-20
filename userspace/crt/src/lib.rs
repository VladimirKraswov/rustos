//! Startup runtime обычной Rust-программы.
//!
//! Компилятор Rust уже генерирует функцию `main(argc, argv)`, которая входит
//! в upstream `std::rt::lang_start`. Но freestanding RUNE-процесс не получает
//! Unix CRT от libc. Этот маленький crate является нашим собственным CRT:
//! проверяет `ProcessStartInfo`, строит привычный `argv` и после возврата из
//! `main` завершает процесс через capability-safe syscall ABI.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::{ptr, slice, str};

use rustos_abi::process::{ProcessStartInfo, PROCESS_ABI_VERSION};

/// Защитный предел startup runtime. Kernel отдельно ограничивает общий размер
/// таблицы аргументов; предел числа указателей не даёт расходовать большой
/// стек даже при повреждённом заголовке.
const MAX_ARGUMENTS: usize = 256;
const MAX_STARTUP_BYTES: u64 = 64 * 1024;

unsafe extern "C" {
    /// Этот символ создаёт rustc для обычной программы с `fn main()`.
    #[link_name = "main"]
    fn rust_main(argc: isize, argv: *const *const u8) -> isize;
    /// Upstream std хранит активный startup block для env/capabilities/TLS.
    /// Обычно это фиксированный kernel mapping; user-space RUNE loader может
    /// передать эквивалентный immutable block перед входом в target.
    fn __rustos_std_set_start_info(address: u64) -> i32;
}

/// Первая инструкция RUNE-процесса с обычным `fn main()`.
///
/// `-u_start` в RustOS SDK заставляет linker извлечь этот объект из rlib. Сам
/// пользователь поэтому пишет обычный переносимый `fn main`, без собственного
/// assembly stub или `#![no_main]`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(start_address: u64, abi_version: u64, _reserved: u64) -> ! {
    let Some(info) = validate_start_info(start_address, abi_version) else {
        rustos_runtime::process_exit(126);
    };
    if unsafe { __rustos_std_set_start_info(start_address) } != 0 {
        rustos_runtime::process_exit(124);
    }
    let mut argv = [ptr::null(); MAX_ARGUMENTS + 1];
    let Some(argc) = build_argv(info, &mut argv) else {
        rustos_runtime::process_exit(125);
    };

    // SAFETY: rustc генерирует `main` с этим ABI. Все указатели argv живут в
    // read-only startup mapping до уничтожения address space, а сам массив —
    // на стеке `_start` на всё время выполнения `rust_main`.
    let status = unsafe { rust_main(argc as isize, argv.as_ptr()) };
    rustos_runtime::process_exit(status as i32)
}

fn validate_start_info(address: u64, abi_version: u64) -> Option<&'static ProcessStartInfo> {
    if abi_version != rustos_abi::syscall::ABI_VERSION
        || !(0x1_0000..0x0000_8000_0000_0000).contains(&address)
        || !address.is_multiple_of(core::mem::align_of::<ProcessStartInfo>() as u64)
    {
        return None;
    }
    // SAFETY: точный адрес зарезервирован process ABI и до входа в ring 3
    // отображается kernel'ом read-only. Поля проверяются до использования.
    let info = unsafe { &*(address as *const ProcessStartInfo) };
    (info.version == PROCESS_ABI_VERSION
        && info.size as usize >= core::mem::size_of::<ProcessStartInfo>()
        && info.argument_count as usize <= MAX_ARGUMENTS)
        .then_some(info)
}

fn build_argv(info: &ProcessStartInfo, argv: &mut [*const u8]) -> Option<usize> {
    let bytes = checked_table(
        info as *const ProcessStartInfo as u64,
        info.arguments_address,
        info.arguments_length,
        info.argument_count,
    )?;
    if bytes.is_empty() {
        return (info.argument_count == 0).then_some(0);
    }

    let mut count = 0usize;
    let mut offset = 0usize;
    while offset < bytes.len() {
        if count >= info.argument_count as usize {
            return None;
        }
        let tail = &bytes[offset..];
        let length = tail.iter().position(|byte| *byte == 0)?;
        str::from_utf8(&tail[..length]).ok()?;
        argv[count] = info.arguments_address.checked_add(offset as u64)? as *const u8;
        count += 1;
        offset = offset.checked_add(length + 1)?;
    }
    (count == info.argument_count as usize).then_some(count)
}

fn checked_table(
    info_address: u64,
    address: u64,
    length: u32,
    count: u32,
) -> Option<&'static [u8]> {
    if length == 0 {
        return (count == 0).then_some(&[]);
    }
    let start_info_end =
        info_address.checked_add(core::mem::size_of::<ProcessStartInfo>() as u64)?;
    let end = address.checked_add(length as u64)?;
    // ABI размещает args/env вслед за заголовком. Верхний предел ограничивает
    // разыменование небольшим kernel-created startup mapping даже если header
    // повреждён. Текущие таблицы занимают не более двух страниц.
    if address < start_info_end || end > info_address.checked_add(MAX_STARTUP_BYTES)? {
        return None;
    }
    // SAFETY: диапазон выше полностью находится в kernel-created read-only
    // startup mapping и проверен на переполнение.
    Some(unsafe { slice::from_raw_parts(address as *const u8, length as usize) })
}

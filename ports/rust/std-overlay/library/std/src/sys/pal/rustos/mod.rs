//! Минимальная граница между upstream `std` и syscall ABI RustOS.
//!
//! Здесь намеренно нет зависимости от `rustos-runtime`: стандартная
//! библиотека является самым нижним user-space слоем и должна собираться
//! внутри собственного sysroot без циклических Cargo-зависимостей.

use core::sync::atomic::{AtomicIsize, AtomicPtr, AtomicU64, Ordering};

use crate::io;

const PROCESS_ABI_VERSION: u32 = 2;
const PROCESS_START_INFO_ADDRESS: usize = 0x0000_3fff_ffff_0000;
pub const STARTUP_ROLE_VFS: u16 = 2;
pub const STARTUP_ROLE_VFS_REPLY: u16 = 3;

/// Локальная копия публичного ABI-заголовка. `std` намеренно не зависит от
/// Cargo crate `rustos-abi`, иначе возник бы цикл при построении sysroot.
#[repr(C)]
struct ProcessStartInfo {
    version: u32,
    size: u32,
    pid: u64,
    tid: u64,
    page_size: u64,
    monotonic_hz: u64,
    arguments_address: u64,
    arguments_length: u32,
    argument_count: u32,
    environment_address: u64,
    environment_length: u32,
    environment_count: u32,
    capabilities_address: u64,
    capability_count: u32,
    reserved: u32,
    tls_template_address: u64,
    tls_file_size: u64,
    tls_memory_size: u64,
    tls_alignment: u32,
    tls_variant: u16,
    tls_reserved: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StartupCapability {
    role: u16,
    flags: u16,
    handle: u32,
    rights: u64,
}

static ARGUMENT_COUNT: AtomicIsize = AtomicIsize::new(0);
static ARGUMENT_VECTOR: AtomicPtr<*const u8> = AtomicPtr::new(core::ptr::null_mut());
static START_INFO_ADDRESS: AtomicU64 = AtomicU64::new(PROCESS_START_INFO_ADDRESS as u64);

/// Переключает std на проверенный startup block. Нормальный kernel startup и
/// user-space `rune-runner` используют один ABI; handle values не меняются.
#[unsafe(no_mangle)]
pub extern "C" fn __rustos_std_set_start_info(address: u64) -> i32 {
    if address < 0x1_0000
        || address >= 0x0000_8000_0000_0000
        || !address.is_multiple_of(core::mem::align_of::<ProcessStartInfo>() as u64)
    {
        return -1;
    }
    let info = unsafe { &*(address as *const ProcessStartInfo) };
    if info.version != PROCESS_ABI_VERSION
        || (info.size as usize) < core::mem::size_of::<ProcessStartInfo>()
        || info.reserved != 0
        || info.tls_reserved != 0
    {
        return -1;
    }
    START_INFO_ADDRESS.store(address, Ordering::Release);
    0
}

// SAFETY: вызывается один раз runtime'ом до пользовательского `main`.
pub unsafe fn init(argc: isize, argv: *const *const u8, _sigpipe: u8) {
    // Release/Acquire позволяет `std::env::args` безопасно вызываться из
    // созданных позднее потоков. Сами строки лежат в неизменяемом startup
    // mapping на протяжении всей жизни процесса.
    ARGUMENT_VECTOR.store(argv.cast_mut(), Ordering::Release);
    ARGUMENT_COUNT.store(argc.max(0), Ordering::Release);

    // std::fs подключается один раз по типизированным startup roles. Номера
    // slots остаются приватной деталью process manager/supervisor.
    if let (Some(server), Some(reply)) = (
        rustos_startup_handle(STARTUP_ROLE_VFS),
        rustos_startup_handle(STARTUP_ROLE_VFS_REPLY),
    ) {
        let status = crate::sys::fs::rustos::__rustos_std_vfs_init(server, reply);
        if status != 0 {
            abort_internal();
        }
    }
}

// SAFETY: вызывается один раз при штатном завершении runtime.
pub unsafe fn cleanup() {}

/// Снимок argv, сохранённый `std::rt` до вызова пользовательского main.
pub fn rustos_arguments() -> (isize, *const *const u8) {
    let count = ARGUMENT_COUNT.load(Ordering::Acquire);
    let vector = ARGUMENT_VECTOR.load(Ordering::Acquire).cast_const();
    (count, vector)
}

/// Возвращает NUL-separated environment из versioned ProcessStartInfo.
///
/// Указатель никогда не выдаётся приложению напрямую: `sys::env` немедленно
/// копирует пары в process-local storage, где `set_var/remove_var` безопасно
/// меняют только состояние текущего процесса.
pub fn rustos_environment() -> Option<(*const u8, usize, usize)> {
    let info = rustos_start_info()?;
    if info.environment_length > 64 * 1024 {
        return None;
    }
    if info.environment_length == 0 {
        return (info.environment_count == 0).then_some((core::ptr::null(), 0, 0));
    }
    let end = info
        .environment_address
        .checked_add(info.environment_length as u64)?;
    let base = START_INFO_ADDRESS.load(Ordering::Acquire);
    if info.environment_address < base || end > base.checked_add(64 * 1024)? {
        return None;
    }
    Some((
        info.environment_address as *const u8,
        info.environment_length as usize,
        info.environment_count as usize,
    ))
}

fn rustos_start_info() -> Option<&'static ProcessStartInfo> {
    // SAFETY: адрес зарезервирован process ABI и является read-only mapping до
    // запуска CRT. Полный header size и версия проверяются перед полями tail.
    let address = START_INFO_ADDRESS.load(Ordering::Acquire);
    let info = unsafe { &*(address as *const ProcessStartInfo) };
    (info.version == PROCESS_ABI_VERSION
        && info.size as usize >= core::mem::size_of::<ProcessStartInfo>())
    .then_some(info)
}

pub fn rustos_startup_handle(role: u16) -> Option<u32> {
    let info = rustos_start_info()?;
    if role == 0
        || info.capability_count > 8
        || info.capabilities_address == 0
        || !info.capabilities_address.is_multiple_of(8)
    {
        return None;
    }
    // SAFETY: kernel хранит bounded массив в том же read-only startup block.
    let capabilities = unsafe {
        core::slice::from_raw_parts(
            info.capabilities_address as *const StartupCapability,
            info.capability_count as usize,
        )
    };
    capabilities
        .iter()
        .find(|capability| capability.role == role && capability.flags == 0)
        .map(|capability| capability.handle)
}

/// Один элемент startup capability namespace для наследования `Command`.
pub fn rustos_startup_capability(index: usize) -> Option<(u16, u32, u64)> {
    let info = rustos_start_info()?;
    if index >= info.capability_count as usize || info.capability_count > 8 {
        return None;
    }
    let capability = unsafe {
        (info.capabilities_address as *const StartupCapability)
            .add(index)
            .read()
    };
    (capability.role != 0 && capability.flags == 0).then_some((
        capability.role,
        capability.handle,
        capability.rights,
    ))
}

pub fn rustos_process_id() -> u64 {
    rustos_start_info().map(|info| info.pid).unwrap_or(0)
}

/// Метаданные immutable TLS template для `std::thread`.
pub fn rustos_tls_template() -> Option<(*const u8, usize, usize, usize, u16)> {
    let info = rustos_start_info()?;
    if info.tls_variant == 0 {
        return Some((core::ptr::null(), 0, 0, 1, 0));
    }
    if info.tls_template_address == 0
        || info.tls_file_size > info.tls_memory_size
        || info.tls_memory_size > 1024 * 1024
        || info.tls_alignment == 0
        || !info.tls_alignment.is_power_of_two()
        || info.tls_alignment > 4096
        || !matches!(info.tls_variant, 1 | 2)
    {
        return None;
    }
    Some((
        info.tls_template_address as *const u8,
        info.tls_file_size as usize,
        info.tls_memory_size as usize,
        info.tls_alignment as usize,
        info.tls_variant,
    ))
}

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}
pub fn unsupported_err() -> io::Error {
    io::Error::UNSUPPORTED_PLATFORM
}

pub fn abort_internal() -> ! {
    core::intrinsics::abort();
}

/// Общий машинно-независимый вход в микроядро. Номера и аргументы совпадают
/// с `rustos-abi`; различается только инструкция ISA.
#[inline]
pub unsafe fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    #[cfg(target_arch = "x86_64")]
    {
        let result: i64;
        unsafe {
            core::arch::asm!(
                "int 0x80",
                inlateout("rax") number as i64 => result,
                in("rdi") arg0,
                in("rsi") arg1,
                in("rdx") arg2,
                options(nostack),
            );
        }
        result
    }
    #[cfg(target_arch = "aarch64")]
    {
        let mut result = arg0 as i64;
        unsafe {
            core::arch::asm!(
                "svc #0",
                in("x8") number,
                inlateout("x0") result,
                in("x1") arg1,
                in("x2") arg2,
                options(nostack),
            );
        }
        result
    }
}

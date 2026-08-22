//! Настоящий ELF64 `ET_DYN` образ `vfs-1.dll`.
//!
//! Реализация живёт в типобезопасном `rustos-vfs-client`; этот crate задаёт
//! только стабильные unmangled C symbols и собственный panic boundary.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::panic::PanicInfo;
use rustos_abi::vfs::DirectoryEntry;
use rustos_vfs::{self as client, VfsClient};

/// Создаёт клиентское соединение с `vfsd`.
///
/// # Safety
/// `output` должен указывать на выровненную writable память для `VfsClient`.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_connect(
    output: *mut VfsClient,
    server: u32,
    reply: u32,
) -> i32 {
    unsafe { client::rustos_vfs_connect(output, server, reply) }
}

/// Открывает объект по UTF-8 пути.
///
/// # Safety
/// `state`, `path[0..length]` и `output` должны быть валидны; `state` нельзя
/// одновременно использовать из другого потока.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_open(
    state: *mut VfsClient,
    path: *const u8,
    length: usize,
    flags: u32,
    output: *mut u64,
) -> i32 {
    unsafe { client::rustos_vfs_open(state, path, length, flags, output) }
}

/// Закрывает server-side file description.
///
/// # Safety
/// `state` должен указывать на ранее инициализированный `VfsClient`.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_close(state: *mut VfsClient, object: u64) -> i32 {
    unsafe { client::rustos_vfs_close(state, object) }
}

/// Читает данные, автоматически разбивая запрос на shared-memory chunks.
///
/// # Safety
/// `state`, `output[0..length]` и `processed` должны быть валидны и не
/// пересекаться; клиент нельзя одновременно использовать другим потоком.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_read(
    state: *mut VfsClient,
    object: u64,
    output: *mut u8,
    length: usize,
    processed: *mut usize,
) -> i32 {
    unsafe { client::rustos_vfs_read(state, object, output, length, processed) }
}

/// Потоково записывает весь входной буфер.
///
/// # Safety
/// `state`, `input[0..length]` и `processed` должны быть валидны; клиент
/// нельзя одновременно использовать другим потоком.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_write(
    state: *mut VfsClient,
    object: u64,
    input: *const u8,
    length: usize,
    processed: *mut usize,
) -> i32 {
    unsafe { client::rustos_vfs_write(state, object, input, length, processed) }
}

/// Изменяет текущую позицию открытого объекта.
///
/// # Safety
/// `state` и `position` должны указывать на валидную память.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_seek(
    state: *mut VfsClient,
    object: u64,
    offset: i64,
    whence: u32,
    position: *mut u64,
) -> i32 {
    unsafe { client::rustos_vfs_seek(state, object, offset, whence, position) }
}

/// Читает следующую запись каталога.
///
/// # Safety
/// `state` и `entry` должны указывать на валидную writable память.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_readdir(
    state: *mut VfsClient,
    directory: u64,
    entry: *mut DirectoryEntry,
    present: *mut u8,
) -> i32 {
    unsafe { client::rustos_vfs_readdir(state, directory, entry, present) }
}

/// Создаёт каталог.
///
/// # Safety
/// `state` и UTF-8 `path[0..length]` должны быть валидны.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_mkdir(
    state: *mut VfsClient,
    path: *const u8,
    length: usize,
) -> i32 {
    unsafe { client::rustos_vfs_mkdir(state, path, length) }
}

/// Удаляет файл или пустой каталог.
///
/// # Safety
/// `state` и UTF-8 `path[0..length]` должны быть валидны.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_unlink(
    state: *mut VfsClient,
    path: *const u8,
    length: usize,
) -> i32 {
    unsafe { client::rustos_vfs_unlink(state, path, length) }
}

/// Атомарно переименовывает объект в пределах mounted filesystem.
///
/// # Safety
/// `state` и оба UTF-8 path slice должны быть валидны на переданные длины.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_rename(
    state: *mut VfsClient,
    old_path: *const u8,
    old_length: usize,
    new_path: *const u8,
    new_length: usize,
) -> i32 {
    unsafe { client::rustos_vfs_rename(state, old_path, old_length, new_path, new_length) }
}

/// Фиксирует данные и metadata на устройстве.
///
/// # Safety
/// `state` должен указывать на валидный и эксклюзивно используемый client.
#[no_mangle]
pub unsafe extern "C" fn rustos_vfs_sync(state: *mut VfsClient) -> i32 {
    unsafe { client::rustos_vfs_sync(state) }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

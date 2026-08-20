//! Клиентская системная библиотека `vfs-1.dll`.
//!
//! Приложение работает с обычными `open/read/write/seek/readdir` и не знает
//! ни формат IPC message, ни правила передачи shared-memory capabilities.
//! Один `VfsClient` допускает один синхронный вызов за раз; для параллельных
//! потоков создаются независимые clients/reply endpoints.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::{mem::size_of, ptr, slice};
use rustos_abi::{
    ipc::flags as ipc_flags,
    memory::MEMORY_ABI_VERSION,
    vfs::{
        self, DirectoryEntry, IoRequest, OpenRequest, PathRequest, RenameRequest, Reply,
        SeekRequest, VfsObject,
    },
};
use rustos_runtime::{
    handle_close, ipc_receive, ipc_send, shared_memory_create, shared_memory_map, syscall, Handle,
    Message, Rights, SharedMemoryCreate, SharedMemoryMap, VmFlags,
};

const BUFFER_PAGES: u64 = 16;
const BUFFER_BYTES: usize = BUFFER_PAGES as usize * 4096;

/// Состояние соединения с `vfsd`. Handles принадлежат текущему процессу.
#[repr(C)]
pub struct VfsClient {
    server: Handle,
    reply: Handle,
    shared: Handle,
    buffer: *mut u8,
    request_id: u64,
}

impl VfsClient {
    /// Создаёт и отображает reusable shared window. `reply` должен иметь
    /// RECEIVE и передаваемое SEND право, `server` — SEND.
    pub fn connect(server: Handle, reply: Handle) -> Result<Self, i32> {
        let create = SharedMemoryCreate {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            length: BUFFER_BYTES as u64,
            flags: VmFlags::READ.union(VmFlags::WRITE),
        };
        let shared = shared_memory_create(&create);
        if shared < 0 {
            return Err(shared as i32);
        }
        let shared = Handle(shared as u32);
        let mapping = SharedMemoryMap {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            address: 0,
            offset: 0,
            length: BUFFER_BYTES as u64,
            flags: VmFlags::READ.union(VmFlags::WRITE),
        };
        let address = shared_memory_map(shared, &mapping);
        if address < 0 {
            let _ = handle_close(shared);
            return Err(address as i32);
        }
        Ok(Self {
            server,
            reply,
            shared,
            buffer: address as *mut u8,
            request_id: 1,
        })
    }

    pub const fn buffer_capacity(&self) -> usize {
        BUFFER_BYTES
    }

    pub fn open(&mut self, path: &str, flags: u32) -> Result<VfsObject, i32> {
        self.put_path(0, path)?;
        let request = OpenRequest {
            directory: VfsObject::ROOT,
            path_offset: 0,
            path_length: path.len() as u32,
            open_flags: flags,
            reserved: 0,
        };
        let reply = self.call(vfs::opcode::OPEN, &request, true)?;
        check(reply)?;
        Ok(reply.object)
    }

    pub fn close(&mut self, file: VfsObject) -> Result<(), i32> {
        let reply = self.call(vfs::opcode::CLOSE, &file, false)?;
        check(reply).map(|_| ())
    }

    /// Читает до `output.len()` байт, разбивая запрос на shared-memory chunks.
    pub fn read(&mut self, file: VfsObject, output: &mut [u8]) -> Result<usize, i32> {
        let mut done = 0usize;
        while done < output.len() {
            let chunk = (output.len() - done).min(BUFFER_BYTES);
            let request = IoRequest {
                file,
                buffer_offset: 0,
                length: chunk as u64,
                file_offset: u64::MAX,
            };
            let reply = self.call(vfs::opcode::READ, &request, true)?;
            check(reply)?;
            let count = usize::try_from(reply.value).map_err(|_| vfs::status::PROTOCOL)?;
            if count > chunk {
                return Err(vfs::status::PROTOCOL);
            }
            unsafe { ptr::copy_nonoverlapping(self.buffer, output[done..].as_mut_ptr(), count) };
            done += count;
            if count < chunk {
                break;
            }
        }
        Ok(done)
    }

    /// Потоково записывает весь input; успешный возврат означает, что все
    /// chunks приняты `vfsd`, но durability требует отдельного `sync`.
    pub fn write(&mut self, file: VfsObject, input: &[u8]) -> Result<usize, i32> {
        let mut done = 0usize;
        while done < input.len() {
            let chunk = (input.len() - done).min(BUFFER_BYTES);
            unsafe { ptr::copy_nonoverlapping(input[done..].as_ptr(), self.buffer, chunk) };
            let request = IoRequest {
                file,
                buffer_offset: 0,
                length: chunk as u64,
                file_offset: u64::MAX,
            };
            let reply = self.call(vfs::opcode::WRITE, &request, true)?;
            check(reply)?;
            let count = usize::try_from(reply.value).map_err(|_| vfs::status::PROTOCOL)?;
            if count == 0 || count > chunk {
                return Err(vfs::status::PROTOCOL);
            }
            done += count;
        }
        Ok(done)
    }

    pub fn seek(&mut self, file: VfsObject, offset: i64, whence: u32) -> Result<u64, i32> {
        let reply = self.call(
            vfs::opcode::SEEK,
            &SeekRequest {
                file,
                offset,
                whence,
                reserved: 0,
            },
            false,
        )?;
        check(reply)?;
        Ok(reply.value)
    }

    pub fn read_dir(&mut self, directory: VfsObject) -> Result<Option<DirectoryEntry>, i32> {
        let request = IoRequest {
            file: directory,
            buffer_offset: 0,
            length: size_of::<DirectoryEntry>() as u64,
            file_offset: u64::MAX,
        };
        let reply = self.call(vfs::opcode::READ_DIR, &request, true)?;
        check(reply)?;
        if reply.value == 0 {
            return Ok(None);
        }
        if reply.value != 1 {
            return Err(vfs::status::PROTOCOL);
        }
        Ok(Some(unsafe {
            ptr::read_unaligned(self.buffer.cast::<DirectoryEntry>())
        }))
    }

    pub fn make_dir(&mut self, path: &str) -> Result<(), i32> {
        self.path_call(vfs::opcode::MAKE_DIR, path)
    }

    pub fn unlink(&mut self, path: &str) -> Result<(), i32> {
        self.path_call(vfs::opcode::UNLINK, path)
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), i32> {
        if old.len() + new.len() > BUFFER_BYTES {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        self.put_path(0, old)?;
        self.put_path(old.len(), new)?;
        let request = RenameRequest {
            old_directory: VfsObject::ROOT,
            new_directory: VfsObject::ROOT,
            old_offset: 0,
            new_offset: old.len() as u64,
            old_length: old.len() as u32,
            new_length: new.len() as u32,
            flags: 0,
            reserved: 0,
        };
        let reply = self.call(vfs::opcode::RENAME, &request, true)?;
        check(reply).map(|_| ())
    }

    pub fn sync(&mut self) -> Result<(), i32> {
        let reply = self.call(vfs::opcode::SYNC, &0u64, false)?;
        check(reply).map(|_| ())
    }

    pub fn shutdown_service(&mut self) -> Result<(), i32> {
        let reply = self.call(vfs::opcode::SHUTDOWN, &0u64, false)?;
        check(reply).map(|_| ())
    }

    fn path_call(&mut self, opcode: u16, path: &str) -> Result<(), i32> {
        self.put_path(0, path)?;
        let request = PathRequest {
            directory: VfsObject::ROOT,
            path_offset: 0,
            path_length: path.len() as u32,
            flags: 0,
        };
        let reply = self.call(opcode, &request, true)?;
        check(reply).map(|_| ())
    }

    fn put_path(&mut self, offset: usize, path: &str) -> Result<(), i32> {
        let end = offset
            .checked_add(path.len())
            .ok_or(vfs::status::INVALID_ARGUMENT)?;
        if path.is_empty() || end > BUFFER_BYTES || path.as_bytes().contains(&0) {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        unsafe { ptr::copy_nonoverlapping(path.as_ptr(), self.buffer.add(offset), path.len()) };
        Ok(())
    }

    fn call<T>(&mut self, opcode: u16, request: &T, shared: bool) -> Result<Reply, i32> {
        if size_of::<T>() > rustos_abi::ipc::IPC_INLINE_BYTES {
            return Err(vfs::status::PROTOCOL);
        }
        let request_id = self.request_id;
        self.request_id = self.request_id.wrapping_add(1).max(1);
        let mut message = Message::EMPTY;
        message.header.abi_version = rustos_abi::ipc::IPC_ABI_VERSION;
        message.header.opcode = opcode;
        message.header.request_id = request_id;
        message.header.payload_len = size_of::<T>() as u32;
        message.header.handle_count = if shared { 2 } else { 1 };
        let bytes =
            unsafe { slice::from_raw_parts((request as *const T).cast::<u8>(), size_of::<T>()) };
        message.payload[..bytes.len()].copy_from_slice(bytes);
        message.handles[0].handle = self.reply;
        message.handles[0].rights = Rights::SEND;
        if shared {
            message.handles[1].handle = self.shared;
            message.handles[1].rights = Rights::READ.union(Rights::WRITE).union(Rights::MAP);
        }
        let result = ipc_send(self.server, &message);
        if result != syscall::status::OK {
            return Err(result as i32);
        }
        let mut response = Message::EMPTY;
        let result = ipc_receive(self.reply, &mut response);
        if result != syscall::status::OK {
            return Err(result as i32);
        }
        if response.header.abi_version != rustos_abi::ipc::IPC_ABI_VERSION
            || response.header.flags & ipc_flags::REPLY == 0
            || response.header.opcode != opcode
            || response.header.request_id != request_id
            || response.header.payload_len as usize != size_of::<Reply>()
        {
            return Err(vfs::status::PROTOCOL);
        }
        Ok(unsafe { ptr::read_unaligned(response.payload.as_ptr().cast::<Reply>()) })
    }
}

fn check(reply: Reply) -> Result<Reply, i32> {
    if reply.status == vfs::status::OK {
        Ok(reply)
    } else {
        Err(reply.status)
    }
}

// Экспортируемый C ABI — будущая таблица символов `vfs-1.dll`. Rust API выше
// остаётся типобезопасным, а C/другие языки получают те же операции без знания
// IPC protocol. `VfsClient` занимает фиксированные 32 байта в ABI v1.
const _: () = assert!(core::mem::size_of::<VfsClient>() == 32);

/// Создаёт клиент в памяти `output`.
///
/// # Safety
/// `output` должен быть выровнен и доступен для записи одного [`VfsClient`].
pub unsafe extern "C" fn rustos_vfs_connect(
    output: *mut VfsClient,
    server: u32,
    reply: u32,
) -> i32 {
    if output.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    match VfsClient::connect(Handle(server), Handle(reply)) {
        Ok(client) => {
            unsafe { output.write(client) };
            vfs::status::OK
        }
        Err(error) => error,
    }
}

/// C ABI `open`.
///
/// # Safety
/// Все указатели должны быть валидны на указанную длину; `client` не может
/// одновременно использоваться другим потоком.
pub unsafe extern "C" fn rustos_vfs_open(
    client: *mut VfsClient,
    path: *const u8,
    length: usize,
    flags: u32,
    output: *mut u64,
) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    if path.is_null() || output.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    let path = unsafe { slice::from_raw_parts(path, length) };
    let Ok(path) = core::str::from_utf8(path) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    match client.open(path, flags) {
        Ok(object) => {
            unsafe { output.write(object.0) };
            vfs::status::OK
        }
        Err(error) => error,
    }
}

/// C ABI `close`.
///
/// # Safety
/// `client` должен указывать на инициализированный объект.
pub unsafe extern "C" fn rustos_vfs_close(client: *mut VfsClient, object: u64) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    client
        .close(VfsObject(object))
        .map(|_| 0)
        .unwrap_or_else(|error| error)
}

/// C ABI потокового `read`.
///
/// # Safety
/// `output` доступен для записи `length` байт, `processed` — одного `usize`.
pub unsafe extern "C" fn rustos_vfs_read(
    client: *mut VfsClient,
    object: u64,
    output: *mut u8,
    length: usize,
    processed: *mut usize,
) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    if output.is_null() || processed.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    match client.read(VfsObject(object), unsafe {
        slice::from_raw_parts_mut(output, length)
    }) {
        Ok(count) => {
            unsafe { processed.write(count) };
            vfs::status::OK
        }
        Err(error) => error,
    }
}

/// C ABI потокового `write`.
///
/// # Safety
/// `input` доступен для чтения `length` байт, `processed` — для записи.
pub unsafe extern "C" fn rustos_vfs_write(
    client: *mut VfsClient,
    object: u64,
    input: *const u8,
    length: usize,
    processed: *mut usize,
) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    if input.is_null() || processed.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    match client.write(VfsObject(object), unsafe {
        slice::from_raw_parts(input, length)
    }) {
        Ok(count) => {
            unsafe { processed.write(count) };
            vfs::status::OK
        }
        Err(error) => error,
    }
}

/// C ABI `seek`.
///
/// # Safety
/// `client` и `position` должны быть валидными указателями.
pub unsafe extern "C" fn rustos_vfs_seek(
    client: *mut VfsClient,
    object: u64,
    offset: i64,
    whence: u32,
    position: *mut u64,
) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    if position.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    match client.seek(VfsObject(object), offset, whence) {
        Ok(value) => {
            unsafe { position.write(value) };
            vfs::status::OK
        }
        Err(error) => error,
    }
}

/// C ABI `readdir`: 1 — запись, 0 — EOF, отрицательное значение — ошибка.
///
/// # Safety
/// `entry` доступен для записи [`DirectoryEntry`].
pub unsafe extern "C" fn rustos_vfs_readdir(
    client: *mut VfsClient,
    directory: u64,
    entry: *mut DirectoryEntry,
) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    if entry.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    match client.read_dir(VfsObject(directory)) {
        Ok(Some(value)) => {
            unsafe { entry.write(value) };
            1
        }
        Ok(None) => 0,
        Err(error) => error,
    }
}

unsafe fn path_operation(
    client: *mut VfsClient,
    path: *const u8,
    length: usize,
    operation: impl FnOnce(&mut VfsClient, &str) -> Result<(), i32>,
) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    if path.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    let bytes = unsafe { slice::from_raw_parts(path, length) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    operation(client, path)
        .map(|_| 0)
        .unwrap_or_else(|error| error)
}

/// C ABI `mkdir`.
///
/// # Safety
/// Указатели подчиняются контракту [`rustos_vfs_open`].
pub unsafe extern "C" fn rustos_vfs_mkdir(
    client: *mut VfsClient,
    path: *const u8,
    length: usize,
) -> i32 {
    unsafe { path_operation(client, path, length, VfsClient::make_dir) }
}

/// C ABI `unlink` для файлов и пустых каталогов.
///
/// # Safety
/// Указатели подчиняются контракту [`rustos_vfs_open`].
pub unsafe extern "C" fn rustos_vfs_unlink(
    client: *mut VfsClient,
    path: *const u8,
    length: usize,
) -> i32 {
    unsafe { path_operation(client, path, length, VfsClient::unlink) }
}

/// C ABI атомарного `rename`.
///
/// # Safety
/// Оба пути доступны для чтения на указанные длины; `client` уникально
/// заимствован на время операции.
pub unsafe extern "C" fn rustos_vfs_rename(
    client: *mut VfsClient,
    old_path: *const u8,
    old_length: usize,
    new_path: *const u8,
    new_length: usize,
) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    if old_path.is_null() || new_path.is_null() {
        return vfs::status::INVALID_ARGUMENT;
    }
    let old = unsafe { slice::from_raw_parts(old_path, old_length) };
    let new = unsafe { slice::from_raw_parts(new_path, new_length) };
    let (Ok(old), Ok(new)) = (core::str::from_utf8(old), core::str::from_utf8(new)) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    client
        .rename(old, new)
        .map(|_| 0)
        .unwrap_or_else(|error| error)
}

/// C ABI `sync`.
///
/// # Safety
/// `client` должен указывать на инициализированный объект.
pub unsafe extern "C" fn rustos_vfs_sync(client: *mut VfsClient) -> i32 {
    let Some(client) = (unsafe { client.as_mut() }) else {
        return vfs::status::INVALID_ARGUMENT;
    };
    client.sync().map(|_| 0).unwrap_or_else(|error| error)
}

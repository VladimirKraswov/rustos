//! Реализация `std::fs` через изолированный сервис `vfsd`.
//!
//! Для программ API остаётся привычным Rust/POSIX-подобным, но `std` не
//! содержит драйвер и не разбирает файловую систему. Каждый вызов становится
//! коротким capability RPC, а пути и данные передаются через повторно
//! используемое окно shared memory. Это сохраняет микроядерную изоляцию и
//! одновременно уменьшает объём изменений при переносе Linux-программ.

use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::hash::{Hash, Hasher};
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::path::{Path, PathBuf};
use crate::ptr;
use crate::slice;
use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sys::time::SystemTime;
use crate::sys::{unsupported, unsupported_err};

pub use crate::sys::fs::common::Dir;
#[path = "unsupported.rs"]
mod unsupported_fs;
pub use unsupported_fs::{FileTimes, link, readlink, set_times, set_times_nofollow, symlink};

const PAGE_SIZE: usize = 4096;
const BUFFER_PAGES: usize = 16;
const BUFFER_BYTES: usize = PAGE_SIZE * BUFFER_PAGES;

const SYSCALL_YIELD: u64 = 0;
const SYSCALL_IPC_SEND: u64 = 3;
const SYSCALL_IPC_RECEIVE: u64 = 4;
const SYSCALL_SHARED_CREATE: u64 = 15;
const SYSCALL_SHARED_MAP: u64 = 16;
const SYSCALL_HANDLE_CLOSE: u64 = 17;

const MEMORY_ABI_VERSION: u32 = 1;
const VM_READ: u64 = 1;
const VM_WRITE: u64 = 2;

const IPC_ABI_VERSION: u16 = 1;
const IPC_REPLY: u32 = 1;
const RIGHT_READ: u64 = 1;
const RIGHT_WRITE: u64 = 2;
const RIGHT_MAP: u64 = 1 << 3;
const RIGHT_SEND: u64 = 1 << 5;

const OP_OPEN: u16 = 1;
const OP_CLOSE: u16 = 2;
const OP_READ: u16 = 3;
const OP_WRITE: u16 = 4;
const OP_STAT: u16 = 5;
const OP_READ_DIR: u16 = 6;
const OP_MAKE_DIR: u16 = 7;
const OP_UNLINK: u16 = 8;
const OP_RENAME: u16 = 9;
const OP_SYNC: u16 = 10;
const OP_SEEK: u16 = 11;
const OP_RESIZE: u16 = 12;
const OP_SHUTDOWN: u16 = 0xff;

const STATUS_OK: i32 = 0;
const STATUS_INVALID_ARGUMENT: i32 = -100;
const STATUS_NOT_FOUND: i32 = -101;
const STATUS_ALREADY_EXISTS: i32 = -102;
const STATUS_NOT_DIRECTORY: i32 = -103;
const STATUS_IS_DIRECTORY: i32 = -104;
const STATUS_NOT_EMPTY: i32 = -105;
const STATUS_READ_ONLY: i32 = -106;

const KIND_FILE: u32 = 1;
const KIND_DIRECTORY: u32 = 2;

const OPEN_READ: u32 = 1;
const OPEN_WRITE: u32 = 1 << 1;
const OPEN_CREATE: u32 = 1 << 2;
const OPEN_EXCLUSIVE: u32 = 1 << 3;
const OPEN_TRUNCATE: u32 = 1 << 4;
const OPEN_APPEND: u32 = 1 << 5;
const OPEN_DIRECTORY: u32 = 1 << 6;

const SEEK_START: u32 = 0;
const SEEK_CURRENT: u32 = 1;
const SEEK_END: u32 = 2;
const ROOT_OBJECT: u64 = 0;
const INVALID_OBJECT: u64 = u64::MAX;

#[repr(C)]
struct SharedMemoryCreate {
    version: u32,
    reserved: u32,
    length: u64,
    flags: u64,
}

#[repr(C)]
struct SharedMemoryMap {
    version: u32,
    reserved: u32,
    address: u64,
    offset: u64,
    length: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MessageHeader {
    abi_version: u16,
    opcode: u16,
    flags: u32,
    request_id: u64,
    sender_pid: u64,
    payload_len: u32,
    handle_count: u16,
    reserved: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TransferredHandle {
    handle: u32,
    reserved: u32,
    rights: u64,
}

#[repr(C)]
struct Message {
    header: MessageHeader,
    payload: [u8; 64],
    handles: [TransferredHandle; 4],
}

impl Message {
    const EMPTY: Self = Self {
        header: MessageHeader {
            abi_version: IPC_ABI_VERSION,
            opcode: 0,
            flags: 0,
            request_id: 0,
            sender_pid: 0,
            payload_len: 0,
            handle_count: 0,
            reserved: 0,
        },
        payload: [0; 64],
        handles: [TransferredHandle {
            handle: 0,
            reserved: 0,
            rights: 0,
        }; 4],
    };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PathRequest {
    directory: u64,
    path_offset: u64,
    path_length: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OpenRequest {
    directory: u64,
    path_offset: u64,
    path_length: u32,
    open_flags: u32,
    reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IoRequest {
    file: u64,
    buffer_offset: u64,
    length: u64,
    file_offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SeekRequest {
    file: u64,
    offset: i64,
    whence: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResizeRequest {
    file: u64,
    length: u64,
    reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RenameRequest {
    old_directory: u64,
    new_directory: u64,
    old_offset: u64,
    new_offset: u64,
    old_length: u32,
    new_length: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Reply {
    status: i32,
    object_kind: u32,
    object: u64,
    value: u64,
    auxiliary: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawDirectoryEntry {
    object: u64,
    size: u64,
    kind: u32,
    name_length: u16,
    reserved: u16,
    name: [u8; 232],
}

struct ClientState {
    initialized: bool,
    server: u32,
    reply: u32,
    shared: u32,
    buffer: *mut u8,
    request_id: u64,
}

struct GlobalState(crate::cell::UnsafeCell<ClientState>);

// SAFETY: доступ к UnsafeCell сериализован глобальным spin lock ниже.
unsafe impl Sync for GlobalState {}

static LOCK: AtomicBool = AtomicBool::new(false);
static STATE: GlobalState = GlobalState(crate::cell::UnsafeCell::new(ClientState {
    initialized: false,
    server: 0,
    reply: 0,
    shared: 0,
    buffer: ptr::null_mut(),
    request_id: 1,
}));

struct StateGuard;

impl StateGuard {
    fn lock() -> Self {
        while LOCK
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Не сжигаем quantum, если несколько потоков одновременно вошли
            // в std::fs. Настоящее futex wait заменит этот цикл позднее.
            unsafe { crate::sys::pal::syscall3(SYSCALL_YIELD, 0, 0, 0) };
        }
        Self
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

fn with_state<T>(operation: impl FnOnce(&mut ClientState) -> Result<T, i32>) -> Result<T, i32> {
    let _guard = StateGuard::lock();
    // SAFETY: StateGuard предоставляет исключительный доступ.
    operation(unsafe { &mut *STATE.0.get() })
}

/// Подключает `std::fs` к capabilities, переданным process manager'ом.
///
/// Runtime следующего этапа вызовет эту функцию сам из ProcessStartInfo.
/// Пока явный вызов делает ring-3 smoke, что позволяет тестировать весь путь
/// без фиктивных глобальных handle'ов.
#[unsafe(no_mangle)]
pub extern "C" fn __rustos_std_vfs_init(server: u32, reply: u32) -> i32 {
    with_state(|state| {
        if state.initialized {
            return if state.server == server && state.reply == reply {
                Ok(STATUS_OK)
            } else {
                Err(STATUS_INVALID_ARGUMENT)
            };
        }
        if server == 0 || reply == 0 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let create = SharedMemoryCreate {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            length: BUFFER_BYTES as u64,
            flags: VM_READ | VM_WRITE,
        };
        let shared = unsafe {
            crate::sys::pal::syscall3(SYSCALL_SHARED_CREATE, ptr::from_ref(&create) as u64, 0, 0)
        };
        if shared < 0 {
            return Err(shared as i32);
        }
        let mapping = SharedMemoryMap {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            address: 0,
            offset: 0,
            length: BUFFER_BYTES as u64,
            flags: VM_READ | VM_WRITE,
        };
        let address = unsafe {
            crate::sys::pal::syscall3(
                SYSCALL_SHARED_MAP,
                shared as u64,
                ptr::from_ref(&mapping) as u64,
                0,
            )
        };
        if address < 0 {
            unsafe { crate::sys::pal::syscall3(SYSCALL_HANDLE_CLOSE, shared as u64, 0, 0) };
            return Err(address as i32);
        }
        state.initialized = true;
        state.server = server;
        state.reply = reply;
        state.shared = shared as u32;
        state.buffer = address as *mut u8;
        state.request_id = 1;
        Ok(STATUS_OK)
    })
    .unwrap_or_else(|status| status)
}

/// Тестовый lifecycle hook: просит `vfsd` синхронизировать диск и завершиться.
#[unsafe(no_mangle)]
pub extern "C" fn __rustos_std_vfs_shutdown() -> i32 {
    with_state(|state| {
        let result = call(state, OP_SHUTDOWN, &0u64, false).and_then(check_reply);
        state.initialized = false;
        result.map(|_| STATUS_OK)
    })
    .unwrap_or_else(|status| status)
}

fn call<T: Copy>(
    state: &mut ClientState,
    opcode: u16,
    request: &T,
    shared: bool,
) -> Result<Reply, i32> {
    if !state.initialized || crate::mem::size_of::<T>() > 64 {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    let request_id = state.request_id;
    state.request_id = state.request_id.wrapping_add(1).max(1);
    let mut message = Message::EMPTY;
    message.header.opcode = opcode;
    message.header.request_id = request_id;
    message.header.payload_len = crate::mem::size_of::<T>() as u32;
    message.header.handle_count = if shared { 2 } else { 1 };
    let bytes = unsafe {
        slice::from_raw_parts(
            ptr::from_ref(request).cast::<u8>(),
            crate::mem::size_of::<T>(),
        )
    };
    message.payload[..bytes.len()].copy_from_slice(bytes);
    message.handles[0] = TransferredHandle {
        handle: state.reply,
        reserved: 0,
        rights: RIGHT_SEND,
    };
    if shared {
        message.handles[1] = TransferredHandle {
            handle: state.shared,
            reserved: 0,
            rights: RIGHT_READ | RIGHT_WRITE | RIGHT_MAP,
        };
    }
    let status = unsafe {
        crate::sys::pal::syscall3(
            SYSCALL_IPC_SEND,
            state.server as u64,
            ptr::from_ref(&message) as u64,
            0,
        )
    };
    if status != 0 {
        return Err(status as i32);
    }
    let mut response = Message::EMPTY;
    let status = unsafe {
        crate::sys::pal::syscall3(
            SYSCALL_IPC_RECEIVE,
            state.reply as u64,
            ptr::from_mut(&mut response) as u64,
            0,
        )
    };
    if status != 0 {
        return Err(status as i32);
    }
    if response.header.abi_version != IPC_ABI_VERSION
        || response.header.flags & IPC_REPLY == 0
        || response.header.opcode != opcode
        || response.header.request_id != request_id
        || response.header.payload_len as usize != crate::mem::size_of::<Reply>()
    {
        return Err(-111);
    }
    Ok(unsafe { ptr::read_unaligned(response.payload.as_ptr().cast::<Reply>()) })
}

fn check_reply(reply: Reply) -> Result<Reply, i32> {
    if reply.status == STATUS_OK {
        Ok(reply)
    } else {
        Err(reply.status)
    }
}

fn put_path(state: &ClientState, offset: usize, path: &Path) -> Result<usize, i32> {
    let absolute =
        crate::sys::paths::rustos::absolute(path).map_err(|_| STATUS_INVALID_ARGUMENT)?;
    let path = absolute.to_str().ok_or(STATUS_INVALID_ARGUMENT)?.as_bytes();
    let end = offset
        .checked_add(path.len())
        .ok_or(STATUS_INVALID_ARGUMENT)?;
    if path.is_empty() || path.contains(&0) || end > BUFFER_BYTES {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    unsafe { ptr::copy_nonoverlapping(path.as_ptr(), state.buffer.add(offset), path.len()) };
    Ok(path.len())
}

fn open_rpc(path: &Path, flags: u32) -> Result<Reply, i32> {
    with_state(|state| {
        let length = put_path(state, 0, path)?;
        check_reply(call(
            state,
            OP_OPEN,
            &OpenRequest {
                directory: ROOT_OBJECT,
                path_offset: 0,
                path_length: length as u32,
                open_flags: flags,
                reserved: 0,
            },
            true,
        )?)
    })
}

fn path_rpc(opcode: u16, path: &Path) -> Result<Reply, i32> {
    with_state(|state| {
        let length = put_path(state, 0, path)?;
        check_reply(call(
            state,
            opcode,
            &PathRequest {
                directory: ROOT_OBJECT,
                path_offset: 0,
                path_length: length as u32,
                flags: 0,
            },
            true,
        )?)
    })
}

fn close_rpc(object: u64) -> Result<(), i32> {
    with_state(|state| check_reply(call(state, OP_CLOSE, &object, false)?).map(|_| ()))
}

fn read_rpc(object: u64, buffer: &mut [u8]) -> Result<usize, i32> {
    with_state(|state| {
        let mut done = 0usize;
        while done < buffer.len() {
            let chunk = (buffer.len() - done).min(BUFFER_BYTES);
            let reply = check_reply(call(
                state,
                OP_READ,
                &IoRequest {
                    file: object,
                    buffer_offset: 0,
                    length: chunk as u64,
                    file_offset: u64::MAX,
                },
                true,
            )?)?;
            let count = usize::try_from(reply.value).map_err(|_| -111)?;
            if count > chunk {
                return Err(-111);
            }
            unsafe { ptr::copy_nonoverlapping(state.buffer, buffer[done..].as_mut_ptr(), count) };
            done += count;
            if count < chunk {
                break;
            }
        }
        Ok(done)
    })
}

fn write_rpc(object: u64, buffer: &[u8]) -> Result<usize, i32> {
    with_state(|state| {
        let mut done = 0usize;
        while done < buffer.len() {
            let chunk = (buffer.len() - done).min(BUFFER_BYTES);
            unsafe { ptr::copy_nonoverlapping(buffer[done..].as_ptr(), state.buffer, chunk) };
            let reply = check_reply(call(
                state,
                OP_WRITE,
                &IoRequest {
                    file: object,
                    buffer_offset: 0,
                    length: chunk as u64,
                    file_offset: u64::MAX,
                },
                true,
            )?)?;
            let count = usize::try_from(reply.value).map_err(|_| -111)?;
            if count == 0 || count > chunk {
                return Err(-111);
            }
            done += count;
        }
        Ok(done)
    })
}

fn seek_rpc(object: u64, offset: i64, whence: u32) -> Result<u64, i32> {
    with_state(|state| {
        check_reply(call(
            state,
            OP_SEEK,
            &SeekRequest {
                file: object,
                offset,
                whence,
                reserved: 0,
            },
            false,
        )?)
        .map(|reply| reply.value)
    })
}

fn resize_rpc(object: u64, length: u64) -> Result<(), i32> {
    with_state(|state| {
        check_reply(call(
            state,
            OP_RESIZE,
            &ResizeRequest {
                file: object,
                length,
                reserved: 0,
            },
            false,
        )?)
        .map(|_| ())
    })
}

fn sync_rpc() -> Result<(), i32> {
    with_state(|state| check_reply(call(state, OP_SYNC, &0u64, false)?).map(|_| ()))
}

fn vfs_error(status: i32) -> io::Error {
    match status {
        STATUS_NOT_FOUND => io::const_error!(io::ErrorKind::NotFound, "VFS object not found"),
        STATUS_ALREADY_EXISTS => {
            io::const_error!(io::ErrorKind::AlreadyExists, "VFS object already exists")
        }
        STATUS_NOT_DIRECTORY => {
            io::const_error!(
                io::ErrorKind::NotADirectory,
                "VFS object is not a directory"
            )
        }
        STATUS_IS_DIRECTORY => {
            io::const_error!(io::ErrorKind::IsADirectory, "VFS object is a directory")
        }
        STATUS_NOT_EMPTY => {
            io::const_error!(
                io::ErrorKind::DirectoryNotEmpty,
                "VFS directory is not empty"
            )
        }
        STATUS_READ_ONLY => {
            io::const_error!(io::ErrorKind::PermissionDenied, "VFS object is read-only")
        }
        STATUS_INVALID_ARGUMENT => {
            io::const_error!(io::ErrorKind::InvalidInput, "invalid VFS argument")
        }
        _ => io::const_error!(io::ErrorKind::Other, "RustOS VFS request failed"),
    }
}

fn io_result<T>(result: Result<T, i32>) -> io::Result<T> {
    result.map_err(vfs_error)
}

pub struct File {
    object: u64,
    path: PathBuf,
    options: OpenOptions,
}

#[derive(Clone)]
pub struct FileAttr {
    size: u64,
    kind: u32,
}

pub struct ReadDir {
    object: u64,
    base: PathBuf,
    finished: bool,
}

pub struct DirEntry {
    path: PathBuf,
    attr: FileAttr,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct FilePermissions {
    readonly: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileType(u32);

#[derive(Debug)]
pub struct DirBuilder;

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn perm(&self) -> FilePermissions {
        FilePermissions { readonly: false }
    }
    pub fn file_type(&self) -> FileType {
        FileType(self.kind)
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        unsupported()
    }
    pub fn accessed(&self) -> io::Result<SystemTime> {
        unsupported()
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        unsupported()
    }
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

impl fmt::Debug for FilePermissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilePermissions")
            .field("readonly", &self.readonly)
            .finish()
    }
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.0 == KIND_DIRECTORY
    }
    pub fn is_file(&self) -> bool {
        self.0 == KIND_FILE
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
}

impl fmt::Debug for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_dir() { "Directory" } else { "File" })
    }
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadDir")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let result = with_state(|state| {
            let reply = check_reply(call(
                state,
                OP_READ_DIR,
                &IoRequest {
                    file: self.object,
                    buffer_offset: 0,
                    length: crate::mem::size_of::<RawDirectoryEntry>() as u64,
                    file_offset: u64::MAX,
                },
                true,
            )?)?;
            if reply.value == 0 {
                return Ok(None);
            }
            if reply.value != 1 {
                return Err(-111);
            }
            Ok(Some(unsafe {
                ptr::read_unaligned(state.buffer.cast::<RawDirectoryEntry>())
            }))
        });
        match result {
            Ok(None) => {
                self.finished = true;
                None
            }
            Ok(Some(raw)) => {
                let name_len = usize::from(raw.name_length);
                if name_len > raw.name.len() {
                    return Some(Err(vfs_error(-111)));
                }
                let Ok(name) = crate::str::from_utf8(&raw.name[..name_len]) else {
                    return Some(Err(io::const_error!(
                        io::ErrorKind::InvalidData,
                        "VFS returned a non-UTF-8 name",
                    )));
                };
                Some(Ok(DirEntry {
                    path: self.base.join(name),
                    attr: FileAttr {
                        size: raw.size,
                        kind: raw.kind,
                    },
                }))
            }
            Err(status) => Some(Err(vfs_error(status))),
        }
    }
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        let _ = close_rpc(self.object);
    }
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
    pub fn file_name(&self) -> OsString {
        self.path.file_name().unwrap_or_default().into()
    }
    pub fn metadata(&self) -> io::Result<FileAttr> {
        Ok(self.attr.clone())
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.attr.file_type())
    }
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }
    pub fn read(&mut self, value: bool) {
        self.read = value;
    }
    pub fn write(&mut self, value: bool) {
        self.write = value;
    }
    pub fn append(&mut self, value: bool) {
        self.append = value;
    }
    pub fn truncate(&mut self, value: bool) {
        self.truncate = value;
    }
    pub fn create(&mut self, value: bool) {
        self.create = value;
    }
    pub fn create_new(&mut self, value: bool) {
        self.create_new = value;
    }

    fn flags(&self) -> io::Result<u32> {
        if !self.read && !self.write && !self.append {
            return Err(vfs_error(STATUS_INVALID_ARGUMENT));
        }
        if (self.truncate || self.create || self.create_new) && !self.write && !self.append {
            return Err(vfs_error(STATUS_INVALID_ARGUMENT));
        }
        let mut flags = 0;
        if self.read {
            flags |= OPEN_READ;
        }
        if self.write || self.append {
            flags |= OPEN_WRITE;
        }
        if self.append {
            flags |= OPEN_APPEND;
        }
        if self.create || self.create_new {
            flags |= OPEN_CREATE;
        }
        if self.create_new {
            flags |= OPEN_EXCLUSIVE;
        }
        if self.truncate {
            flags |= OPEN_TRUNCATE;
        }
        Ok(flags)
    }
}

impl File {
    pub fn open(path: &Path, options: &OpenOptions) -> io::Result<Self> {
        let absolute = crate::sys::paths::rustos::absolute(path)?;
        let reply = io_result(open_rpc(&absolute, options.flags()?))?;
        if reply.object == INVALID_OBJECT || reply.object_kind == KIND_DIRECTORY {
            if reply.object != INVALID_OBJECT {
                let _ = close_rpc(reply.object);
            }
            return Err(vfs_error(STATUS_IS_DIRECTORY));
        }
        Ok(Self {
            object: reply.object,
            path: absolute,
            options: options.clone(),
        })
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        stat(&self.path)
    }
    pub fn fsync(&self) -> io::Result<()> {
        io_result(sync_rpc())
    }
    pub fn datasync(&self) -> io::Result<()> {
        self.fsync()
    }
    pub fn lock(&self) -> io::Result<()> {
        unsupported()
    }
    pub fn lock_shared(&self) -> io::Result<()> {
        unsupported()
    }
    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(unsupported_err()))
    }
    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(unsupported_err()))
    }
    pub fn unlock(&self) -> io::Result<()> {
        unsupported()
    }
    pub fn truncate(&self, size: u64) -> io::Result<()> {
        io_result(resize_rpc(self.object, size))
    }
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        io_result(read_rpc(self.object, buffer))
    }
    pub fn read_vectored(&self, buffers: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        crate::io::default_read_vectored(|buffer| self.read(buffer), buffers)
    }
    pub fn is_read_vectored(&self) -> bool {
        false
    }
    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        crate::io::default_read_buf(|buffer| self.read(buffer), cursor)
    }
    pub fn write(&self, buffer: &[u8]) -> io::Result<usize> {
        io_result(write_rpc(self.object, buffer))
    }
    pub fn write_vectored(&self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|buffer| self.write(buffer), buffers)
    }
    pub fn is_write_vectored(&self) -> bool {
        false
    }
    pub fn flush(&self) -> io::Result<()> {
        self.fsync()
    }
    pub fn seek(&self, position: SeekFrom) -> io::Result<u64> {
        let (offset, whence) = match position {
            SeekFrom::Start(value) => (
                i64::try_from(value).map_err(|_| vfs_error(STATUS_INVALID_ARGUMENT))?,
                SEEK_START,
            ),
            SeekFrom::Current(value) => (value, SEEK_CURRENT),
            SeekFrom::End(value) => (value, SEEK_END),
        };
        io_result(seek_rpc(self.object, offset, whence))
    }
    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|attr| attr.size()))
    }
    pub fn tell(&self) -> io::Result<u64> {
        io_result(seek_rpc(self.object, 0, SEEK_CURRENT))
    }
    pub fn duplicate(&self) -> io::Result<Self> {
        let position = self.tell()?;
        let duplicate = Self::open(&self.path, &self.options)?;
        duplicate.seek(SeekFrom::Start(position))?;
        Ok(duplicate)
    }
    pub fn set_permissions(&self, permissions: FilePermissions) -> io::Result<()> {
        set_perm(&self.path, permissions)
    }
    pub fn set_times(&self, times: FileTimes) -> io::Result<()> {
        set_times(&self.path, times)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = close_rpc(self.object);
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File")
            .field("object", &self.object)
            .field("path", &self.path)
            .finish()
    }
}

impl DirBuilder {
    pub fn new() -> Self {
        Self
    }
    pub fn mkdir(&self, path: &Path) -> io::Result<()> {
        io_result(path_rpc(OP_MAKE_DIR, path)).map(|_| ())
    }
}

pub fn readdir(path: &Path) -> io::Result<ReadDir> {
    let absolute = crate::sys::paths::rustos::absolute(path)?;
    let reply = io_result(open_rpc(&absolute, OPEN_READ | OPEN_DIRECTORY))?;
    if reply.object == INVALID_OBJECT || reply.object_kind != KIND_DIRECTORY {
        if reply.object != INVALID_OBJECT {
            let _ = close_rpc(reply.object);
        }
        return Err(vfs_error(STATUS_NOT_DIRECTORY));
    }
    Ok(ReadDir {
        object: reply.object,
        base: absolute,
        finished: false,
    })
}

pub fn unlink(path: &Path) -> io::Result<()> {
    if stat(path)?.file_type().is_dir() {
        return Err(vfs_error(STATUS_IS_DIRECTORY));
    }
    io_result(path_rpc(OP_UNLINK, path)).map(|_| ())
}
pub fn rmdir(path: &Path) -> io::Result<()> {
    if !stat(path)?.file_type().is_dir() {
        return Err(vfs_error(STATUS_NOT_DIRECTORY));
    }
    io_result(path_rpc(OP_UNLINK, path)).map(|_| ())
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    io_result(with_state(|state| {
        let old_len = put_path(state, 0, old)?;
        let new_len = put_path(state, old_len, new)?;
        check_reply(call(
            state,
            OP_RENAME,
            &RenameRequest {
                old_directory: ROOT_OBJECT,
                new_directory: ROOT_OBJECT,
                old_offset: 0,
                new_offset: old_len as u64,
                old_length: old_len as u32,
                new_length: new_len as u32,
                flags: 0,
                reserved: 0,
            },
            true,
        )?)
        .map(|_| ())
    }))
}

pub fn stat(path: &Path) -> io::Result<FileAttr> {
    io_result(path_rpc(OP_STAT, path)).map(|reply| FileAttr {
        size: reply.value,
        kind: reply.object_kind,
    })
}

pub fn lstat(path: &Path) -> io::Result<FileAttr> {
    stat(path)
}
pub fn exists(path: &Path) -> io::Result<bool> {
    crate::sys::fs::common::exists(path)
}
pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    crate::sys::fs::common::remove_dir_all(path)
}
pub fn copy(from: &Path, to: &Path) -> io::Result<u64> {
    crate::sys::fs::common::copy(from, to)
}

pub fn set_perm(_path: &Path, _permissions: FilePermissions) -> io::Result<()> {
    Ok(())
}
pub fn set_perm_nofollow(path: &Path, permissions: FilePermissions) -> io::Result<()> {
    set_perm(path, permissions)
}

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let absolute = crate::sys::paths::rustos::absolute(path)?;
    stat(&absolute)?;
    Ok(absolute)
}

const _: () = assert!(crate::mem::size_of::<MessageHeader>() == 32);
const _: () = assert!(crate::mem::size_of::<TransferredHandle>() == 16);
const _: () = assert!(crate::mem::size_of::<Message>() == 160);
const _: () = assert!(crate::mem::size_of::<Reply>() == 32);
const _: () = assert!(crate::mem::size_of::<RawDirectoryEntry>() == 256);

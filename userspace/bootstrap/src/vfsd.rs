//! Изолированный ring-3 VFS server для VaraniaFS.
//!
//! `vfsd` единолично владеет raw block capability. Ядро видит только IPC и
//! shared-memory transfer windows; ошибка filesystem parser завершает этот
//! процесс, но не микроядро и не другие приложения.

#![no_std]
#![no_main]

use core::{mem::size_of, panic::PanicInfo, ptr, slice};
use rustos_abi::{
    block::{BlockIoRequest, BLOCK_ABI_VERSION},
    ipc::{flags as ipc_flags, IPC_ABI_VERSION},
    memory::MEMORY_ABI_VERSION,
    vfs::{
        self, DirectoryEntry, IoRequest, OpenRequest, PathRequest, RenameRequest, Reply,
        ResizeRequest, SeekRequest, VfsObject,
    },
};
use rustos_runtime::{
    block_flush, block_get_size, block_read, block_write, handle_close, ipc_receive, ipc_send,
    process_exit, shared_memory_map, syscall, vm_unmap, Handle, Message, SharedMemoryMap, VmFlags,
};
use varaniafs::{
    file,
    format::{self, Block, Error as StorageError, InodeKind, Superblock},
    namespace::{self, NamespaceError},
    tree::{BlockDevice, Transaction, TransactionWorkspace},
    BLOCK_SIZE,
};

const SHARED_BYTES: usize = 16 * BLOCK_SIZE;
const MAX_OPEN_FILES: usize = 32;
const MAX_PATH_BYTES: usize = 192;
const ALL_OPEN_FLAGS: u32 = vfs::open_flags::READ
    | vfs::open_flags::WRITE
    | vfs::open_flags::CREATE
    | vfs::open_flags::EXCLUSIVE
    | vfs::open_flags::TRUNCATE
    | vfs::open_flags::APPEND
    | vfs::open_flags::DIRECTORY;

#[derive(Clone, Copy)]
struct OpenFile {
    used: bool,
    generation: u32,
    owner: u64,
    object_id: u64,
    offset: u64,
    flags: u32,
    path: [u8; MAX_PATH_BYTES],
}

impl OpenFile {
    const EMPTY: Self = Self {
        used: false,
        generation: 1,
        owner: 0,
        object_id: 0,
        offset: 0,
        flags: 0,
        path: [0; MAX_PATH_BYTES],
    };
}

struct Device {
    handle: Handle,
}

impl Device {
    fn call(&mut self, write: bool, block: u64, buffer: *mut u8) -> Result<(), StorageError> {
        let request = BlockIoRequest {
            version: BLOCK_ABI_VERSION,
            flags: 0,
            block,
            buffer_address: buffer as u64,
            block_count: 1,
            reserved: 0,
        };
        let result = if write {
            block_write(self.handle, &request)
        } else {
            block_read(self.handle, &request)
        };
        if result == syscall::status::OK {
            Ok(())
        } else {
            Err(StorageError::Io)
        }
    }
}

impl BlockDevice for Device {
    fn read(&mut self, block: u64, output: &mut Block) -> Result<(), StorageError> {
        self.call(false, block, output.as_mut_ptr())
    }

    fn write(&mut self, block: u64, input: &Block) -> Result<(), StorageError> {
        self.call(true, block, input.as_ptr() as *mut u8)
    }

    fn flush(&mut self) -> Result<(), StorageError> {
        if block_flush(self.handle) == syscall::status::OK {
            Ok(())
        } else {
            Err(StorageError::Io)
        }
    }
}

struct Server {
    device: Handle,
    endpoint: Handle,
    volume_blocks: u64,
    mounted: Option<Superblock>,
    opens: [OpenFile; MAX_OPEN_FILES],
    transaction_workspace: TransactionWorkspace,
}

impl Server {
    const fn empty() -> Self {
        Self {
            device: Handle::INVALID,
            endpoint: Handle::INVALID,
            volume_blocks: 0,
            mounted: None,
            opens: [OpenFile::EMPTY; MAX_OPEN_FILES],
            transaction_workspace: TransactionWorkspace::new(),
        }
    }

    fn mount(&mut self, endpoint: Handle, device: Handle) -> Result<(), i32> {
        self.endpoint = endpoint;
        self.device = device;
        let blocks = block_get_size(device);
        if blocks <= 0 {
            return Err(vfs::status::IO);
        }
        self.volume_blocks = blocks as u64;
        let mut block_device = Device { handle: device };
        let recovered = format::recover_latest(self.volume_blocks, |number, output| {
            block_device.read(number, output).is_ok()
        })
        .map_err(storage_status)?;
        self.mounted = Some(recovered.superblock);
        Ok(())
    }

    fn serve(&mut self) -> ! {
        loop {
            let mut request = Message::EMPTY;
            if ipc_receive(self.endpoint, &mut request) != syscall::status::OK {
                process_exit(151);
            }
            if request.header.abi_version != IPC_ABI_VERSION
                || request.header.sender_pid == 0
                || request.header.handle_count == 0
                || request.handles[0].handle == Handle::INVALID
            {
                self.close_transferred(&request);
                continue;
            }
            let reply_endpoint = request.handles[0].handle;
            let shared = if request.header.handle_count >= 2 {
                self.map_shared(request.handles[1].handle).ok()
            } else {
                None
            };
            let (reply, shutdown) = self.dispatch(&request, shared);
            let mut response = Message::EMPTY;
            response.header.abi_version = IPC_ABI_VERSION;
            response.header.opcode = request.header.opcode;
            response.header.flags = ipc_flags::REPLY;
            response.header.request_id = request.header.request_id;
            response.header.payload_len = size_of::<Reply>() as u32;
            response.payload[..size_of::<Reply>()].copy_from_slice(bytes_of(&reply));
            let _ = ipc_send(reply_endpoint, &response);
            if let Some(address) = shared {
                let _ = vm_unmap(address as u64, SHARED_BYTES as u64);
            }
            if request.header.handle_count >= 2 {
                let _ = handle_close(request.handles[1].handle);
            }
            let _ = handle_close(reply_endpoint);
            if shutdown {
                process_exit(if reply.status == vfs::status::OK {
                    0
                } else {
                    152
                });
            }
        }
    }

    fn dispatch(&mut self, message: &Message, shared: Option<*mut u8>) -> (Reply, bool) {
        let sender = message.header.sender_pid;
        let result = match message.header.opcode {
            vfs::opcode::OPEN => self
                .payload::<OpenRequest>(message)
                .and_then(|r| self.open(sender, r, shared)),
            vfs::opcode::CLOSE => self
                .payload::<VfsObject>(message)
                .and_then(|r| self.close(sender, r)),
            vfs::opcode::READ => self
                .payload::<IoRequest>(message)
                .and_then(|r| self.read(sender, r, shared)),
            vfs::opcode::WRITE => self
                .payload::<IoRequest>(message)
                .and_then(|r| self.write(sender, r, shared)),
            vfs::opcode::SEEK => self
                .payload::<SeekRequest>(message)
                .and_then(|r| self.seek(sender, r)),
            vfs::opcode::RESIZE => self
                .payload::<ResizeRequest>(message)
                .and_then(|r| self.resize(sender, r)),
            vfs::opcode::READ_DIR => self
                .payload::<IoRequest>(message)
                .and_then(|r| self.read_dir(sender, r, shared)),
            vfs::opcode::STAT => self
                .payload::<PathRequest>(message)
                .and_then(|r| self.stat(sender, r, shared)),
            vfs::opcode::MAKE_DIR => self
                .payload::<PathRequest>(message)
                .and_then(|r| self.make_dir(sender, r, shared)),
            vfs::opcode::UNLINK => self
                .payload::<PathRequest>(message)
                .and_then(|r| self.unlink(sender, r, shared)),
            vfs::opcode::RENAME => self
                .payload::<RenameRequest>(message)
                .and_then(|r| self.rename(sender, r, shared)),
            vfs::opcode::SYNC | vfs::opcode::SHUTDOWN => self
                .sync()
                .map(|_| ok_reply(vfs::object_kind::NONE, VfsObject::INVALID, 0, 0)),
            _ => Err(vfs::status::PROTOCOL),
        };
        let reply = result.unwrap_or_else(error_reply);
        (reply, message.header.opcode == vfs::opcode::SHUTDOWN)
    }

    fn open(
        &mut self,
        owner: u64,
        request: OpenRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        if request.reserved != 0 || request.open_flags & !ALL_OPEN_FLAGS != 0 {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        let path = self.request_path(
            owner,
            request.directory,
            request.path_offset,
            request.path_length,
            shared,
        )?;
        let path_bytes = path_slice(&path);
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        let mut changed = false;
        let object_id = match namespace::resolve(&mut transaction, path_bytes) {
            Ok(object) => {
                if request.open_flags & vfs::open_flags::CREATE != 0
                    && request.open_flags & vfs::open_flags::EXCLUSIVE != 0
                {
                    return Err(vfs::status::ALREADY_EXISTS);
                }
                object
            }
            Err(NamespaceError::NotFound)
                if request.open_flags & vfs::open_flags::CREATE != 0
                    && request.open_flags & vfs::open_flags::DIRECTORY == 0 =>
            {
                changed = true;
                namespace::create(&mut transaction, path_bytes, InodeKind::File, 0)
                    .map_err(namespace_status)?
            }
            Err(error) => return Err(namespace_status(error)),
        };
        let mut inode = namespace::inode(&mut transaction, object_id).map_err(namespace_status)?;
        if request.open_flags & vfs::open_flags::DIRECTORY != 0
            && inode.kind != InodeKind::Directory
        {
            return Err(vfs::status::NOT_DIRECTORY);
        }
        if request.open_flags & vfs::open_flags::TRUNCATE != 0 {
            if request.open_flags & vfs::open_flags::WRITE == 0 {
                return Err(vfs::status::READ_ONLY);
            }
            if inode.kind == InodeKind::Directory {
                return Err(vfs::status::IS_DIRECTORY);
            }
            file::resize(&mut transaction, object_id, 0, 0).map_err(storage_status)?;
            inode.size = 0;
            changed = true;
        }
        if changed {
            self.mounted = Some(transaction.commit().map_err(storage_status)?);
        } else {
            // Освобождаем mutable borrow переиспользуемого transaction
            // workspace до изменения таблицы открытых объектов.
            drop(transaction);
        }
        let object = self.allocate_open(owner, object_id, request.open_flags, path)?;
        Ok(ok_reply(object_kind(inode.kind), object, inode.size, 0))
    }

    fn close(&mut self, owner: u64, object: VfsObject) -> Result<Reply, i32> {
        let index = self.open_index(owner, object)?;
        self.opens[index].used = false;
        self.opens[index].generation = self.opens[index].generation.wrapping_add(1).max(1);
        Ok(ok_reply(vfs::object_kind::NONE, VfsObject::INVALID, 0, 0))
    }

    fn read(
        &mut self,
        owner: u64,
        request: IoRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        let buffer = checked_shared(shared, request.buffer_offset, request.length)?;
        let index = self.open_index(owner, request.file)?;
        let open = self.opens[index];
        if open.flags & vfs::open_flags::READ == 0 {
            return Err(vfs::status::READ_ONLY);
        }
        let start = if request.file_offset == u64::MAX {
            open.offset
        } else {
            request.file_offset
        };
        let output = unsafe { slice::from_raw_parts_mut(buffer, request.length as usize) };
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        if namespace::inode(&mut transaction, open.object_id)
            .map_err(namespace_status)?
            .kind
            == InodeKind::Directory
        {
            return Err(vfs::status::IS_DIRECTORY);
        }
        let done = file::read_at(&mut transaction, open.object_id, start, output)
            .map_err(storage_status)?;
        let end = start + done as u64;
        if request.file_offset == u64::MAX {
            self.opens[index].offset = end;
        }
        Ok(ok_reply(
            vfs::object_kind::FILE,
            request.file,
            done as u64,
            end,
        ))
    }

    fn write(
        &mut self,
        owner: u64,
        request: IoRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        let buffer = checked_shared(shared, request.buffer_offset, request.length)?;
        let index = self.open_index(owner, request.file)?;
        let open = self.opens[index];
        if open.flags & vfs::open_flags::WRITE == 0 {
            return Err(vfs::status::READ_ONLY);
        }
        let input = unsafe { slice::from_raw_parts(buffer, request.length as usize) };
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        let inode = namespace::inode(&mut transaction, open.object_id).map_err(namespace_status)?;
        if inode.kind == InodeKind::Directory {
            return Err(vfs::status::IS_DIRECTORY);
        }
        let start = if open.flags & vfs::open_flags::APPEND != 0 {
            inode.size
        } else if request.file_offset == u64::MAX {
            open.offset
        } else {
            request.file_offset
        };
        let done = file::write_at(&mut transaction, open.object_id, start, input, 0)
            .map_err(storage_status)?;
        self.mounted = Some(transaction.commit().map_err(storage_status)?);
        let end = start + done as u64;
        if request.file_offset == u64::MAX || open.flags & vfs::open_flags::APPEND != 0 {
            self.opens[index].offset = end;
        }
        Ok(ok_reply(
            vfs::object_kind::FILE,
            request.file,
            done as u64,
            end,
        ))
    }

    fn seek(&mut self, owner: u64, request: SeekRequest) -> Result<Reply, i32> {
        if request.reserved != 0 {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        let index = self.open_index(owner, request.file)?;
        let open = self.opens[index];
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        let size = namespace::inode(&mut transaction, open.object_id)
            .map_err(namespace_status)?
            .size;
        let base = match request.whence {
            vfs::seek_from::START => 0i128,
            vfs::seek_from::CURRENT => i128::from(open.offset),
            vfs::seek_from::END => i128::from(size),
            _ => return Err(vfs::status::INVALID_ARGUMENT),
        };
        let position = base + i128::from(request.offset);
        if position < 0 || position > i128::from(u64::MAX) {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        self.opens[index].offset = position as u64;
        Ok(ok_reply(
            vfs::object_kind::NONE,
            request.file,
            position as u64,
            0,
        ))
    }

    fn resize(&mut self, owner: u64, request: ResizeRequest) -> Result<Reply, i32> {
        if request.reserved != 0 {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        let index = self.open_index(owner, request.file)?;
        let open = self.opens[index];
        if open.flags & vfs::open_flags::WRITE == 0 {
            return Err(vfs::status::READ_ONLY);
        }
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        file::resize(&mut transaction, open.object_id, request.length, 0)
            .map_err(storage_status)?;
        self.mounted = Some(transaction.commit().map_err(storage_status)?);
        Ok(ok_reply(
            vfs::object_kind::FILE,
            request.file,
            request.length,
            0,
        ))
    }

    fn read_dir(
        &mut self,
        owner: u64,
        request: IoRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        if request.length < size_of::<DirectoryEntry>() as u64 {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        let buffer = checked_shared(
            shared,
            request.buffer_offset,
            size_of::<DirectoryEntry>() as u64,
        )?;
        let index = self.open_index(owner, request.file)?;
        let open = self.opens[index];
        let cursor = if request.file_offset == u64::MAX {
            open.offset
        } else {
            request.file_offset
        };
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        let Some(source) = namespace::read_dir_at(&mut transaction, open.object_id, cursor)
            .map_err(namespace_status)?
        else {
            return Ok(ok_reply(
                vfs::object_kind::DIRECTORY,
                request.file,
                0,
                cursor,
            ));
        };
        let mut entry = DirectoryEntry::EMPTY;
        entry.object = VfsObject(source.object_id);
        entry.size = source.inode.size;
        entry.kind = object_kind(source.inode.kind);
        entry.name_length = source.name_len;
        entry.name[..source.name().len()].copy_from_slice(source.name());
        unsafe { ptr::write_unaligned(buffer.cast::<DirectoryEntry>(), entry) };
        if request.file_offset == u64::MAX {
            self.opens[index].offset = cursor + 1;
        }
        Ok(ok_reply(
            vfs::object_kind::DIRECTORY,
            request.file,
            1,
            cursor + 1,
        ))
    }

    fn stat(
        &mut self,
        owner: u64,
        request: PathRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        let path = self.request_path(
            owner,
            request.directory,
            request.path_offset,
            request.path_length,
            shared,
        )?;
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        let object =
            namespace::resolve(&mut transaction, path_slice(&path)).map_err(namespace_status)?;
        let inode = namespace::inode(&mut transaction, object).map_err(namespace_status)?;
        Ok(ok_reply(
            object_kind(inode.kind),
            VfsObject::INVALID,
            inode.size,
            inode.generation,
        ))
    }

    fn make_dir(
        &mut self,
        owner: u64,
        request: PathRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        let path = self.request_path(
            owner,
            request.directory,
            request.path_offset,
            request.path_length,
            shared,
        )?;
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        namespace::create(&mut transaction, path_slice(&path), InodeKind::Directory, 0)
            .map_err(namespace_status)?;
        self.mounted = Some(transaction.commit().map_err(storage_status)?);
        Ok(ok_reply(
            vfs::object_kind::DIRECTORY,
            VfsObject::INVALID,
            0,
            0,
        ))
    }

    fn unlink(
        &mut self,
        owner: u64,
        request: PathRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        let path = self.request_path(
            owner,
            request.directory,
            request.path_offset,
            request.path_length,
            shared,
        )?;
        let bytes = path_slice(&path);
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        let object = namespace::resolve(&mut transaction, bytes).map_err(namespace_status)?;
        if namespace::inode(&mut transaction, object)
            .map_err(namespace_status)?
            .kind
            == InodeKind::File
        {
            file::resize(&mut transaction, object, 0, 0).map_err(storage_status)?;
        }
        namespace::unlink(&mut transaction, bytes).map_err(namespace_status)?;
        self.mounted = Some(transaction.commit().map_err(storage_status)?);
        for open in &mut self.opens {
            if open.used && open.object_id == object {
                open.used = false;
                open.generation = open.generation.wrapping_add(1).max(1);
            }
        }
        Ok(ok_reply(vfs::object_kind::NONE, VfsObject::INVALID, 0, 0))
    }

    fn rename(
        &mut self,
        owner: u64,
        request: RenameRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        if request.flags != 0 || request.reserved != 0 {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        let old = self.request_path(
            owner,
            request.old_directory,
            request.old_offset,
            request.old_length,
            shared,
        )?;
        let new = self.request_path(
            owner,
            request.new_directory,
            request.new_offset,
            request.new_length,
            shared,
        )?;
        let mut device = Device {
            handle: self.device,
        };
        let mounted = self.superblock()?;
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut self.transaction_workspace)
                .map_err(storage_status)?;
        namespace::rename(&mut transaction, path_slice(&old), path_slice(&new))
            .map_err(namespace_status)?;
        self.mounted = Some(transaction.commit().map_err(storage_status)?);
        self.update_open_paths(path_slice(&old), path_slice(&new));
        Ok(ok_reply(vfs::object_kind::NONE, VfsObject::INVALID, 0, 0))
    }

    fn request_path(
        &self,
        owner: u64,
        directory: VfsObject,
        offset: u64,
        length: u32,
        shared: Option<*mut u8>,
    ) -> Result<[u8; MAX_PATH_BYTES], i32> {
        let raw = checked_shared(shared, offset, u64::from(length))?;
        let raw = unsafe { slice::from_raw_parts(raw, length as usize) };
        if raw.is_empty() || raw.contains(&0) {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        let base = if directory == VfsObject::ROOT {
            b"/".as_slice()
        } else {
            path_slice(&self.opens[self.open_index(owner, directory)?].path)
        };
        normalize_path(base, raw)
    }

    fn allocate_open(
        &mut self,
        owner: u64,
        object_id: u64,
        flags: u32,
        path: [u8; MAX_PATH_BYTES],
    ) -> Result<VfsObject, i32> {
        let index = self
            .opens
            .iter()
            .position(|entry| !entry.used)
            .ok_or(vfs::status::LIMIT_REACHED)?;
        let generation = self.opens[index].generation.max(1);
        self.opens[index] = OpenFile {
            used: true,
            generation,
            owner,
            object_id,
            offset: 0,
            flags,
            path,
        };
        Ok(VfsObject(
            (u64::from(generation) << 32) | (index as u64 + 1),
        ))
    }

    fn open_index(&self, owner: u64, object: VfsObject) -> Result<usize, i32> {
        let slot = (object.0 as u32)
            .checked_sub(1)
            .ok_or(vfs::status::BAD_OBJECT)? as usize;
        let generation = (object.0 >> 32) as u32;
        let open = self.opens.get(slot).ok_or(vfs::status::BAD_OBJECT)?;
        if !open.used || open.owner != owner || open.generation != generation {
            return Err(vfs::status::BAD_OBJECT);
        }
        Ok(slot)
    }

    fn update_open_paths(&mut self, old: &[u8], new: &[u8]) {
        for open in &mut self.opens {
            let current = path_slice(&open.path);
            if !open.used || (current != old && !is_descendant(old, current)) {
                continue;
            }
            let suffix = &current[old.len()..];
            if new.len() + suffix.len() > MAX_PATH_BYTES {
                continue;
            }
            let mut path = [0; MAX_PATH_BYTES];
            path[..new.len()].copy_from_slice(new);
            path[new.len()..new.len() + suffix.len()].copy_from_slice(suffix);
            open.path = path;
        }
    }

    fn superblock(&self) -> Result<Superblock, i32> {
        self.mounted.ok_or(vfs::status::IO)
    }
    fn sync(&self) -> Result<(), i32> {
        Device {
            handle: self.device,
        }
        .flush()
        .map_err(storage_status)
    }

    fn map_shared(&self, handle: Handle) -> Result<*mut u8, i32> {
        let request = SharedMemoryMap {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            address: 0,
            offset: 0,
            length: SHARED_BYTES as u64,
            flags: VmFlags::READ.union(VmFlags::WRITE),
        };
        let address = shared_memory_map(handle, &request);
        if address < 0 {
            Err(vfs::status::PROTOCOL)
        } else {
            Ok(address as *mut u8)
        }
    }

    fn payload<T: Copy>(&self, message: &Message) -> Result<T, i32> {
        if message.header.payload_len as usize != size_of::<T>() {
            return Err(vfs::status::PROTOCOL);
        }
        Ok(unsafe { ptr::read_unaligned(message.payload.as_ptr().cast::<T>()) })
    }

    fn close_transferred(&self, message: &Message) {
        for item in message
            .handles
            .iter()
            .take(message.header.handle_count as usize)
        {
            if item.handle.is_valid() {
                let _ = handle_close(item.handle);
            }
        }
    }
}

static mut SERVER: Server = Server::empty();

#[no_mangle]
pub extern "C" fn _start(endpoint: u64, block_device: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(150);
    }
    let server = unsafe { &mut *core::ptr::addr_of_mut!(SERVER) };
    if server
        .mount(Handle(endpoint as u32), Handle(block_device as u32))
        .is_err()
    {
        process_exit(153);
    }
    server.serve()
}

fn checked_shared(shared: Option<*mut u8>, offset: u64, length: u64) -> Result<*mut u8, i32> {
    let base = shared.ok_or(vfs::status::PROTOCOL)?;
    let end = offset
        .checked_add(length)
        .ok_or(vfs::status::INVALID_ARGUMENT)?;
    if end > SHARED_BYTES as u64 {
        return Err(vfs::status::INVALID_ARGUMENT);
    }
    Ok(unsafe { base.add(offset as usize) })
}

fn normalize_path(base: &[u8], input: &[u8]) -> Result<[u8; MAX_PATH_BYTES], i32> {
    let mut raw = [0u8; MAX_PATH_BYTES * 2];
    let mut length = 0usize;
    if input.first() == Some(&b'/') {
        append(&mut raw, &mut length, input)?;
    } else {
        append(&mut raw, &mut length, base)?;
        if base != b"/" {
            append(&mut raw, &mut length, b"/")?;
        }
        append(&mut raw, &mut length, input)?;
    }
    let mut result = [0u8; MAX_PATH_BYTES];
    result[0] = b'/';
    let mut output = 1usize;
    let mut starts = [0usize; 32];
    let mut depth = 0usize;
    let mut cursor = 0usize;
    while cursor < length {
        while cursor < length && raw[cursor] == b'/' {
            cursor += 1;
        }
        let start = cursor;
        while cursor < length && raw[cursor] != b'/' {
            cursor += 1;
        }
        let component = &raw[start..cursor];
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            if depth > 0 {
                depth -= 1;
                output = if starts[depth] == 1 {
                    1
                } else {
                    starts[depth] - 1
                };
                result[output..].fill(0);
            }
            continue;
        }
        if depth == starts.len()
            || output + component.len() + usize::from(output > 1) > MAX_PATH_BYTES
        {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        if output > 1 {
            result[output] = b'/';
            output += 1;
        }
        starts[depth] = output;
        depth += 1;
        result[output..output + component.len()].copy_from_slice(component);
        output += component.len();
    }
    Ok(result)
}

fn append(output: &mut [u8], length: &mut usize, bytes: &[u8]) -> Result<(), i32> {
    let end = length
        .checked_add(bytes.len())
        .ok_or(vfs::status::INVALID_ARGUMENT)?;
    if end > output.len() {
        return Err(vfs::status::INVALID_ARGUMENT);
    }
    output[*length..end].copy_from_slice(bytes);
    *length = end;
    Ok(())
}

fn path_slice(path: &[u8; MAX_PATH_BYTES]) -> &[u8] {
    let length = path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(MAX_PATH_BYTES);
    &path[..length]
}

fn is_descendant(parent: &[u8], candidate: &[u8]) -> bool {
    candidate.len() > parent.len()
        && candidate.starts_with(parent)
        && (parent == b"/" || candidate.get(parent.len()) == Some(&b'/'))
}

fn object_kind(kind: InodeKind) -> u32 {
    if kind == InodeKind::Directory {
        vfs::object_kind::DIRECTORY
    } else {
        vfs::object_kind::FILE
    }
}

fn namespace_status(error: NamespaceError) -> i32 {
    match error {
        NamespaceError::Storage(error) => storage_status(error),
        NamespaceError::InvalidPath => vfs::status::INVALID_ARGUMENT,
        NamespaceError::NotFound => vfs::status::NOT_FOUND,
        NamespaceError::AlreadyExists => vfs::status::ALREADY_EXISTS,
        NamespaceError::NotDirectory => vfs::status::NOT_DIRECTORY,
        NamespaceError::IsDirectory => vfs::status::IS_DIRECTORY,
        NamespaceError::DirectoryNotEmpty => vfs::status::NOT_EMPTY,
    }
}

fn storage_status(error: StorageError) -> i32 {
    match error {
        StorageError::Capacity => vfs::status::NO_SPACE,
        StorageError::InvalidArgument | StorageError::InvalidItem => vfs::status::INVALID_ARGUMENT,
        _ => vfs::status::IO,
    }
}

fn ok_reply(kind: u32, object: VfsObject, value: u64, auxiliary: u64) -> Reply {
    Reply {
        status: vfs::status::OK,
        object_kind: kind,
        object,
        value,
        auxiliary,
    }
}
fn error_reply(status: i32) -> Reply {
    Reply {
        status,
        ..Reply::EMPTY
    }
}
fn bytes_of<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(159)
}

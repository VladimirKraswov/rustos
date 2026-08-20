//! Изолированный ring-3 VFS server.
//!
//! Только этот процесс получает raw block capability. Ошибка parser'а,
//! pathname logic или filesystem metadata завершит `vfsd`, но не kernel и не
//! остальные процессы. Control plane идёт через capability IPC, данные —
//! через переданное shared-memory окно.

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
    crc32, kind, metadata_slot_start, FileExtent, FreeExtent, Inode, Metadata, Superblock,
    BLOCK_SIZE, MAX_EXTENTS_PER_INODE, MAX_FREE_EXTENTS, MAX_INODES, MAX_PATH_BYTES,
    METADATA_BLOCKS,
};

const SHARED_BYTES: usize = 16 * BLOCK_SIZE;
const MAX_OPEN_FILES: usize = 32;
const ROOT_INODE: u16 = u16::MAX;
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
    inode: u16,
    inode_generation: u64,
    offset: u64,
    flags: u32,
}

impl OpenFile {
    const EMPTY: Self = Self {
        used: false,
        generation: 1,
        owner: 0,
        inode: ROOT_INODE,
        inode_generation: 0,
        offset: 0,
        flags: 0,
    };
}

struct Server {
    device: Handle,
    endpoint: Handle,
    volume_blocks: u64,
    active_slot: u32,
    metadata: Metadata,
    opens: [OpenFile; MAX_OPEN_FILES],
    block_buffer: [u8; BLOCK_SIZE],
}

impl Server {
    const fn empty() -> Self {
        Self {
            device: Handle::INVALID,
            endpoint: Handle::INVALID,
            volume_blocks: 0,
            active_slot: 0,
            metadata: Metadata::empty(),
            opens: [OpenFile::EMPTY; MAX_OPEN_FILES],
            block_buffer: [0; BLOCK_SIZE],
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

        let first = self.read_superblock(0).ok();
        let second = self.read_superblock(1).ok();
        let order = match (first, second) {
            (Some(a), Some(b)) if b.sequence > a.sequence => [Some(b), Some(a)],
            (Some(a), Some(b)) => [Some(a), Some(b)],
            (Some(a), None) => [Some(a), None],
            (None, Some(b)) => [Some(b), None],
            (None, None) => return Err(vfs::status::IO),
        };
        for candidate in order.into_iter().flatten() {
            if self.load_metadata(candidate.active_slot).is_ok()
                && self.metadata.sequence == candidate.sequence
                && crc32(self.metadata.bytes()) == candidate.metadata_crc32
            {
                self.active_slot = candidate.active_slot;
                return Ok(());
            }
        }
        Err(vfs::status::IO)
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
            // Даже если map отклонён, полученный capability необходимо
            // закрыть: malformed client не должен исчерпать таблицу vfsd.
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
                .and_then(|request| self.open(sender, request, shared)),
            vfs::opcode::CLOSE => self
                .payload::<VfsObject>(message)
                .and_then(|object| self.close(sender, object)),
            vfs::opcode::READ => self
                .payload::<IoRequest>(message)
                .and_then(|request| self.read(sender, request, shared)),
            vfs::opcode::WRITE => self
                .payload::<IoRequest>(message)
                .and_then(|request| self.write(sender, request, shared)),
            vfs::opcode::SEEK => self
                .payload::<SeekRequest>(message)
                .and_then(|request| self.seek(sender, request)),
            vfs::opcode::RESIZE => self
                .payload::<ResizeRequest>(message)
                .and_then(|request| self.resize(sender, request)),
            vfs::opcode::READ_DIR => self
                .payload::<IoRequest>(message)
                .and_then(|request| self.read_dir(sender, request, shared)),
            vfs::opcode::STAT => self
                .payload::<PathRequest>(message)
                .and_then(|request| self.stat(sender, request, shared)),
            vfs::opcode::MAKE_DIR => self
                .payload::<PathRequest>(message)
                .and_then(|request| self.make_dir(sender, request, shared)),
            vfs::opcode::UNLINK => self
                .payload::<PathRequest>(message)
                .and_then(|request| self.unlink(sender, request, shared)),
            vfs::opcode::RENAME => self
                .payload::<RenameRequest>(message)
                .and_then(|request| self.rename(sender, request, shared)),
            vfs::opcode::SYNC => self
                .sync()
                .map(|_| ok_reply(vfs::object_kind::NONE, VfsObject::INVALID, 0, 0)),
            vfs::opcode::SHUTDOWN => self
                .sync()
                .map(|_| ok_reply(vfs::object_kind::NONE, VfsObject::INVALID, 0, 0)),
            _ => Err(vfs::status::PROTOCOL),
        };
        let reply = result.unwrap_or_else(error_reply);
        let shutdown = message.header.opcode == vfs::opcode::SHUTDOWN;
        (reply, shutdown)
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
        let mut inode_index = self.find_inode(&path);
        if let Some(index) = inode_index {
            if request.open_flags & vfs::open_flags::CREATE != 0
                && request.open_flags & vfs::open_flags::EXCLUSIVE != 0
            {
                return Err(vfs::status::ALREADY_EXISTS);
            }
            let inode = &self.metadata.inodes[index];
            if request.open_flags & vfs::open_flags::DIRECTORY != 0 && inode.kind != kind::DIRECTORY
            {
                return Err(vfs::status::NOT_DIRECTORY);
            }
            if request.open_flags & vfs::open_flags::TRUNCATE != 0 {
                if request.open_flags & vfs::open_flags::WRITE == 0 {
                    return Err(vfs::status::READ_ONLY);
                }
                if inode.kind == kind::DIRECTORY {
                    return Err(vfs::status::IS_DIRECTORY);
                }
                self.truncate_inode(index)?;
                self.commit()?;
            }
        } else {
            if request.open_flags & vfs::open_flags::CREATE == 0 {
                return Err(vfs::status::NOT_FOUND);
            }
            if request.open_flags & vfs::open_flags::DIRECTORY != 0 {
                return Err(vfs::status::INVALID_ARGUMENT);
            }
            self.require_parent_directory(&path)?;
            inode_index = Some(self.create_inode(&path, kind::FILE)?);
            self.commit()?;
        }
        let index = inode_index.ok_or(vfs::status::NOT_FOUND)?;
        let object = self.allocate_open(owner, index as u16, request.open_flags)?;
        let inode = &self.metadata.inodes[index];
        Ok(ok_reply(
            if inode.kind == kind::DIRECTORY {
                vfs::object_kind::DIRECTORY
            } else {
                vfs::object_kind::FILE
            },
            object,
            inode.size,
            0,
        ))
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
        let open_index = self.open_index(owner, request.file)?;
        let open = self.opens[open_index];
        if open.flags & vfs::open_flags::READ == 0 {
            return Err(vfs::status::READ_ONLY);
        }
        let inode_index = usize::from(open.inode);
        let inode = *self
            .metadata
            .inodes
            .get(inode_index)
            .ok_or(vfs::status::BAD_OBJECT)?;
        if inode.kind == kind::DIRECTORY {
            return Err(vfs::status::IS_DIRECTORY);
        }
        let start = if request.file_offset == u64::MAX {
            open.offset
        } else {
            request.file_offset
        };
        let total = request.length.min(inode.size.saturating_sub(start));
        let mut done = 0u64;
        while done < total {
            let position = start + done;
            let logical = position / BLOCK_SIZE as u64;
            let within = (position % BLOCK_SIZE as u64) as usize;
            let count = ((total - done) as usize).min(BLOCK_SIZE - within);
            if let Some(physical) = inode_block(&inode, logical) {
                self.read_disk_block(physical)?;
            } else {
                self.block_buffer.fill(0);
            }
            unsafe {
                ptr::copy_nonoverlapping(
                    self.block_buffer[within..].as_ptr(),
                    buffer.add(done as usize),
                    count,
                )
            };
            done += count as u64;
        }
        if request.file_offset == u64::MAX {
            self.opens[open_index].offset = start + done;
        }
        Ok(ok_reply(
            vfs::object_kind::FILE,
            request.file,
            done,
            start + done,
        ))
    }

    fn write(
        &mut self,
        owner: u64,
        request: IoRequest,
        shared: Option<*mut u8>,
    ) -> Result<Reply, i32> {
        let buffer = checked_shared(shared, request.buffer_offset, request.length)?;
        let open_index = self.open_index(owner, request.file)?;
        let open = self.opens[open_index];
        if open.flags & vfs::open_flags::WRITE == 0 {
            return Err(vfs::status::READ_ONLY);
        }
        let inode_index = usize::from(open.inode);
        if self.metadata.inodes[inode_index].kind == kind::DIRECTORY {
            return Err(vfs::status::IS_DIRECTORY);
        }
        let start = if open.flags & vfs::open_flags::APPEND != 0 {
            self.metadata.inodes[inode_index].size
        } else if request.file_offset == u64::MAX {
            open.offset
        } else {
            request.file_offset
        };
        let end = start
            .checked_add(request.length)
            .ok_or(vfs::status::NO_SPACE)?;
        let mut done = 0u64;
        while done < request.length {
            let position = start + done;
            let logical = position / BLOCK_SIZE as u64;
            let within = (position % BLOCK_SIZE as u64) as usize;
            let count = ((request.length - done) as usize).min(BLOCK_SIZE - within);
            let physical = match inode_block(&self.metadata.inodes[inode_index], logical) {
                Some(block) => {
                    if within != 0 || count != BLOCK_SIZE {
                        self.read_disk_block(block)?;
                    }
                    block
                }
                None => {
                    self.block_buffer.fill(0);
                    let block = self.allocate_data_block()?;
                    add_inode_block(&mut self.metadata.inodes[inode_index], logical, block)?;
                    block
                }
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    buffer.add(done as usize),
                    self.block_buffer[within..].as_mut_ptr(),
                    count,
                )
            };
            self.write_disk_block(physical)?;
            done += count as u64;
        }
        self.metadata.inodes[inode_index].size = self.metadata.inodes[inode_index].size.max(end);
        if request.file_offset == u64::MAX || open.flags & vfs::open_flags::APPEND != 0 {
            self.opens[open_index].offset = end;
        }
        self.commit()?;
        Ok(ok_reply(vfs::object_kind::FILE, request.file, done, end))
    }

    fn seek(&mut self, owner: u64, request: SeekRequest) -> Result<Reply, i32> {
        if request.reserved != 0 {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        let open_index = self.open_index(owner, request.file)?;
        let open = self.opens[open_index];
        let size = if open.inode == ROOT_INODE {
            0
        } else {
            self.metadata.inodes[open.inode as usize].size
        };
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
        self.opens[open_index].offset = position as u64;
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
        let open_index = self.open_index(owner, request.file)?;
        let open = self.opens[open_index];
        if open.flags & vfs::open_flags::WRITE == 0 {
            return Err(vfs::status::READ_ONLY);
        }
        let inode_index = usize::from(open.inode);
        if self.metadata.inodes[inode_index].kind == kind::DIRECTORY {
            return Err(vfs::status::IS_DIRECTORY);
        }

        let old_size = self.metadata.inodes[inode_index].size;
        if request.length < old_size {
            self.shrink_inode(inode_index, request.length)?;
        } else {
            // VaraniaFS является sparse-aware: для увеличения длины не нужно
            // записывать гигабайты нулей. Неотображённые logical blocks уже
            // возвращаются read() как нули и займут место при первой записи.
            self.metadata.inodes[inode_index].size = request.length;
        }
        self.commit()?;
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
        let open_index = self.open_index(owner, request.file)?;
        let open = self.opens[open_index];
        let parent = if open.inode == ROOT_INODE {
            b"/".as_slice()
        } else {
            let inode = &self.metadata.inodes[open.inode as usize];
            if inode.kind != kind::DIRECTORY {
                return Err(vfs::status::NOT_DIRECTORY);
            }
            inode.path()
        };
        let mut cursor = if request.file_offset == u64::MAX {
            open.offset as usize
        } else {
            request.file_offset as usize
        };
        while cursor < MAX_INODES {
            let inode = &self.metadata.inodes[cursor];
            cursor += 1;
            if inode.used == 0 {
                continue;
            }
            let Some(name) = immediate_child(parent, inode.path()) else {
                continue;
            };
            let mut entry = DirectoryEntry::EMPTY;
            entry.object = VfsObject((inode.generation << 16) | (cursor - 1) as u64);
            entry.size = inode.size;
            entry.kind = if inode.kind == kind::DIRECTORY {
                vfs::object_kind::DIRECTORY
            } else {
                vfs::object_kind::FILE
            };
            entry.name_length = name.len() as u16;
            entry.name[..name.len()].copy_from_slice(name);
            unsafe { ptr::write_unaligned(buffer.cast::<DirectoryEntry>(), entry) };
            if request.file_offset == u64::MAX {
                self.opens[open_index].offset = cursor as u64;
            }
            return Ok(ok_reply(
                vfs::object_kind::DIRECTORY,
                request.file,
                1,
                cursor as u64,
            ));
        }
        Ok(ok_reply(
            vfs::object_kind::DIRECTORY,
            request.file,
            0,
            cursor as u64,
        ))
    }

    fn stat(
        &self,
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
        if path_slice(&path) == b"/" {
            return Ok(ok_reply(vfs::object_kind::DIRECTORY, VfsObject::ROOT, 0, 0));
        }
        let index = self.find_inode(&path).ok_or(vfs::status::NOT_FOUND)?;
        let inode = &self.metadata.inodes[index];
        Ok(ok_reply(
            if inode.kind == kind::DIRECTORY {
                vfs::object_kind::DIRECTORY
            } else {
                vfs::object_kind::FILE
            },
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
        if self.find_inode(&path).is_some() || path_slice(&path) == b"/" {
            return Err(vfs::status::ALREADY_EXISTS);
        }
        self.require_parent_directory(&path)?;
        self.create_inode(&path, kind::DIRECTORY)?;
        self.commit()?;
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
        let index = self.find_inode(&path).ok_or(vfs::status::NOT_FOUND)?;
        if self.metadata.inodes[index].kind == kind::DIRECTORY
            && self
                .metadata
                .inodes
                .iter()
                .any(|inode| inode.used != 0 && is_descendant(path_slice(&path), inode.path()))
        {
            return Err(vfs::status::NOT_EMPTY);
        }
        self.truncate_inode(index)?;
        self.metadata.inodes[index] = Inode::EMPTY;
        self.metadata.inode_count = self.metadata.inode_count.saturating_sub(1);
        for open in &mut self.opens {
            if open.used && open.inode as usize == index {
                open.used = false;
                open.generation = open.generation.wrapping_add(1).max(1);
            }
        }
        self.commit()?;
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
        let old_path = path_slice(&old);
        let new_path = path_slice(&new);
        let source = self.find_inode(&old).ok_or(vfs::status::NOT_FOUND)?;
        if self.find_inode(&new).is_some() {
            return Err(vfs::status::ALREADY_EXISTS);
        }
        self.require_parent_directory(&new)?;
        if self.metadata.inodes[source].kind == kind::DIRECTORY && is_descendant(old_path, new_path)
        {
            return Err(vfs::status::INVALID_ARGUMENT);
        }
        for inode in &self.metadata.inodes {
            if inode.used == 0
                || (!same_path(inode.path(), old_path) && !is_descendant(old_path, inode.path()))
            {
                continue;
            }
            let suffix = &inode.path()[old_path.len()..];
            if new_path.len() + suffix.len() > MAX_PATH_BYTES {
                return Err(vfs::status::INVALID_ARGUMENT);
            }
        }
        for inode in &mut self.metadata.inodes {
            if inode.used == 0
                || (!same_path(inode.path(), old_path) && !is_descendant(old_path, inode.path()))
            {
                continue;
            }
            let mut path = [0u8; MAX_PATH_BYTES];
            let suffix_start = old_path.len();
            let suffix_len = inode.path_len as usize - suffix_start;
            path[..new_path.len()].copy_from_slice(new_path);
            path[new_path.len()..new_path.len() + suffix_len]
                .copy_from_slice(&inode.path[suffix_start..suffix_start + suffix_len]);
            inode.path = path;
            inode.path_len = (new_path.len() + suffix_len) as u16;
        }
        self.commit()?;
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
            let index = self.open_index(owner, directory)?;
            let open = self.opens[index];
            if open.inode == ROOT_INODE {
                b"/".as_slice()
            } else {
                let inode = &self.metadata.inodes[open.inode as usize];
                if inode.kind != kind::DIRECTORY {
                    return Err(vfs::status::NOT_DIRECTORY);
                }
                inode.path()
            }
        };
        normalize_path(base, raw)
    }

    fn allocate_open(&mut self, owner: u64, inode: u16, flags: u32) -> Result<VfsObject, i32> {
        let index = self
            .opens
            .iter()
            .position(|entry| !entry.used)
            .ok_or(vfs::status::LIMIT_REACHED)?;
        let generation = self.opens[index].generation.max(1);
        let inode_generation = if inode == ROOT_INODE {
            0
        } else {
            self.metadata.inodes[inode as usize].generation
        };
        self.opens[index] = OpenFile {
            used: true,
            generation,
            owner,
            inode,
            inode_generation,
            offset: 0,
            flags,
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
        if open.inode != ROOT_INODE {
            let inode = &self.metadata.inodes[open.inode as usize];
            if inode.used == 0 || inode.generation != open.inode_generation {
                return Err(vfs::status::BAD_OBJECT);
            }
        }
        Ok(slot)
    }

    fn find_inode(&self, path: &[u8; MAX_PATH_BYTES]) -> Option<usize> {
        let path = path_slice(path);
        self.metadata
            .inodes
            .iter()
            .position(|inode| inode.used != 0 && same_path(inode.path(), path))
    }

    fn require_parent_directory(&self, path: &[u8; MAX_PATH_BYTES]) -> Result<(), i32> {
        let path = path_slice(path);
        let split = path
            .iter()
            .rposition(|byte| *byte == b'/')
            .ok_or(vfs::status::INVALID_ARGUMENT)?;
        if split == 0 {
            return Ok(());
        }
        let mut parent = [0u8; MAX_PATH_BYTES];
        parent[..split].copy_from_slice(&path[..split]);
        let index = self.find_inode(&parent).ok_or(vfs::status::NOT_FOUND)?;
        if self.metadata.inodes[index].kind != kind::DIRECTORY {
            Err(vfs::status::NOT_DIRECTORY)
        } else {
            Ok(())
        }
    }

    fn create_inode(&mut self, path: &[u8; MAX_PATH_BYTES], inode_kind: u8) -> Result<usize, i32> {
        let index = self
            .metadata
            .inodes
            .iter()
            .position(|inode| inode.used == 0)
            .ok_or(vfs::status::NO_SPACE)?;
        let bytes = path_slice(path);
        let mut inode = Inode::EMPTY;
        inode.used = 1;
        inode.kind = inode_kind;
        inode.generation = self.metadata.next_inode_generation;
        self.metadata.next_inode_generation =
            self.metadata.next_inode_generation.wrapping_add(1).max(1);
        inode.path_len = bytes.len() as u16;
        inode.path[..bytes.len()].copy_from_slice(bytes);
        self.metadata.inodes[index] = inode;
        self.metadata.inode_count += 1;
        Ok(index)
    }

    fn truncate_inode(&mut self, index: usize) -> Result<(), i32> {
        let inode = self.metadata.inodes[index];
        for extent in inode.extents.iter().take(inode.extent_count as usize) {
            self.release_extent(extent.physical, extent.blocks)?;
        }
        self.metadata.inodes[index].size = 0;
        self.metadata.inodes[index].extent_count = 0;
        self.metadata.inodes[index].extents = [FileExtent::EMPTY; MAX_EXTENTS_PER_INODE];
        Ok(())
    }

    fn shrink_inode(&mut self, index: usize, new_size: u64) -> Result<(), i32> {
        let keep_blocks = new_size.div_ceil(BLOCK_SIZE as u64);
        let inode = self.metadata.inodes[index];
        let mut kept = [FileExtent::EMPTY; MAX_EXTENTS_PER_INODE];
        let mut kept_count = 0usize;

        // POSIX/Rust гарантируют нули после последовательности shrink→grow.
        // Поэтому остаток последнего сохранённого блока нельзя оставлять со
        // старыми данными: иначе другой процесс увидит содержимое за EOF.
        let tail = (new_size % BLOCK_SIZE as u64) as usize;
        if tail != 0 {
            let logical = new_size / BLOCK_SIZE as u64;
            if let Some(physical) = inode_block(&inode, logical) {
                self.read_disk_block(physical)?;
                self.block_buffer[tail..].fill(0);
                self.write_disk_block(physical)?;
            }
        }

        for extent in inode.extents.iter().take(inode.extent_count as usize) {
            if extent.logical >= keep_blocks {
                self.release_extent(extent.physical, extent.blocks)?;
                continue;
            }
            let keep = extent.blocks.min(keep_blocks - extent.logical);
            kept[kept_count] = FileExtent {
                blocks: keep,
                ..*extent
            };
            kept_count += 1;
            if keep < extent.blocks {
                self.release_extent(extent.physical + keep, extent.blocks - keep)?;
            }
        }
        self.metadata.inodes[index].size = new_size;
        self.metadata.inodes[index].extent_count = kept_count as u16;
        self.metadata.inodes[index].extents = kept;
        Ok(())
    }

    fn allocate_data_block(&mut self) -> Result<u64, i32> {
        for index in 0..self.metadata.free_extent_count as usize {
            let extent = self.metadata.free_extents[index];
            if extent.blocks == 0 {
                continue;
            }
            let result = extent.start;
            self.metadata.free_extents[index].start += 1;
            self.metadata.free_extents[index].blocks -= 1;
            if self.metadata.free_extents[index].blocks == 0 {
                let count = self.metadata.free_extent_count as usize;
                for cursor in index..count - 1 {
                    self.metadata.free_extents[cursor] = self.metadata.free_extents[cursor + 1];
                }
                self.metadata.free_extents[count - 1] = FreeExtent::EMPTY;
                self.metadata.free_extent_count -= 1;
            }
            return Ok(result);
        }
        if self.metadata.next_data_block >= self.volume_blocks {
            return Err(vfs::status::NO_SPACE);
        }
        let result = self.metadata.next_data_block;
        self.metadata.next_data_block += 1;
        Ok(result)
    }

    fn release_extent(&mut self, start: u64, blocks: u64) -> Result<(), i32> {
        if blocks == 0 {
            return Ok(());
        }
        let count = self.metadata.free_extent_count as usize;
        if count == MAX_FREE_EXTENTS {
            return Err(vfs::status::LIMIT_REACHED);
        }
        self.metadata.free_extents[count] = FreeExtent { start, blocks };
        self.metadata.free_extent_count += 1;
        Ok(())
    }

    fn read_superblock(&mut self, block: u64) -> Result<Superblock, i32> {
        self.read_disk_block(block)?;
        let superblock =
            unsafe { ptr::read_unaligned(self.block_buffer.as_ptr().cast::<Superblock>()) };
        if superblock.validate(self.volume_blocks) {
            Ok(superblock)
        } else {
            Err(vfs::status::IO)
        }
    }

    fn load_metadata(&mut self, slot: u32) -> Result<(), i32> {
        let start = metadata_slot_start(slot);
        for block in 0..METADATA_BLOCKS as usize {
            let target = self.metadata.bytes_mut()[block * BLOCK_SIZE..][..BLOCK_SIZE].as_mut_ptr();
            Self::block_call(self.device, false, start + block as u64, target)?;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), i32> {
        if block_flush(self.device) != syscall::status::OK {
            return Err(vfs::status::IO);
        }
        self.metadata.sequence = self.metadata.sequence.wrapping_add(1).max(1);
        let inactive = 1 - self.active_slot;
        let start = metadata_slot_start(inactive);
        for block in 0..METADATA_BLOCKS as usize {
            let source =
                self.metadata.bytes()[block * BLOCK_SIZE..][..BLOCK_SIZE].as_ptr() as *mut u8;
            Self::block_call(self.device, true, start + block as u64, source)?;
        }
        if block_flush(self.device) != syscall::status::OK {
            return Err(vfs::status::IO);
        }
        let superblock = Superblock::new(
            self.volume_blocks,
            self.metadata.sequence,
            inactive,
            crc32(self.metadata.bytes()),
        );
        Self::block_call(
            self.device,
            true,
            self.metadata.sequence & 1,
            superblock.bytes().as_ptr() as *mut u8,
        )?;
        if block_flush(self.device) != syscall::status::OK {
            return Err(vfs::status::IO);
        }
        self.active_slot = inactive;
        Ok(())
    }

    fn sync(&self) -> Result<(), i32> {
        if block_flush(self.device) == syscall::status::OK {
            Ok(())
        } else {
            Err(vfs::status::IO)
        }
    }

    fn read_disk_block(&mut self, block: u64) -> Result<(), i32> {
        Self::block_call(self.device, false, block, self.block_buffer.as_mut_ptr())
    }

    fn write_disk_block(&mut self, block: u64) -> Result<(), i32> {
        Self::block_call(self.device, true, block, self.block_buffer.as_mut_ptr())
    }

    fn block_call(device: Handle, write: bool, block: u64, buffer: *mut u8) -> Result<(), i32> {
        let request = BlockIoRequest {
            version: BLOCK_ABI_VERSION,
            flags: 0,
            block,
            buffer_address: buffer as u64,
            block_count: 1,
            reserved: 0,
        };
        let result = if write {
            block_write(device, &request)
        } else {
            block_read(device, &request)
        };
        if result == syscall::status::OK {
            Ok(())
        } else {
            Err(vfs::status::IO)
        }
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

fn same_path(left: &[u8], right: &[u8]) -> bool {
    left == right
}

fn is_descendant(parent: &[u8], candidate: &[u8]) -> bool {
    candidate.len() > parent.len()
        && candidate.starts_with(parent)
        && (parent == b"/" || candidate.get(parent.len()) == Some(&b'/'))
}

fn immediate_child<'a>(parent: &[u8], candidate: &'a [u8]) -> Option<&'a [u8]> {
    let remainder = if parent == b"/" {
        candidate.strip_prefix(b"/")?
    } else {
        let rest = candidate.strip_prefix(parent)?;
        rest.strip_prefix(b"/")?
    };
    if remainder.is_empty() || remainder.contains(&b'/') {
        None
    } else {
        Some(remainder)
    }
}

fn inode_block(inode: &Inode, logical: u64) -> Option<u64> {
    inode
        .extents
        .iter()
        .take(inode.extent_count as usize)
        .find_map(|extent| {
            (logical >= extent.logical && logical < extent.logical + extent.blocks)
                .then_some(extent.physical + logical - extent.logical)
        })
}

fn add_inode_block(inode: &mut Inode, logical: u64, physical: u64) -> Result<(), i32> {
    if let Some(last) = inode
        .extents
        .get_mut(inode.extent_count.saturating_sub(1) as usize)
    {
        if inode.extent_count != 0
            && last.logical + last.blocks == logical
            && last.physical + last.blocks == physical
        {
            last.blocks += 1;
            return Ok(());
        }
    }
    let index = inode.extent_count as usize;
    if index == MAX_EXTENTS_PER_INODE {
        return Err(vfs::status::NO_SPACE);
    }
    inode.extents[index] = FileExtent {
        logical,
        physical,
        blocks: 1,
    };
    inode.extent_count += 1;
    Ok(())
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

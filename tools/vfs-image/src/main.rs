//! Host-утилита создания и проверки постоянного VaraniaFS-диска.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    process::ExitCode,
};
use varaniafs::{
    crc32, kind, metadata_slot_start, FileExtent, FreeExtent, Inode, Metadata, Superblock,
    BLOCK_SIZE, MAX_EXTENTS_PER_INODE, MAX_FREE_EXTENTS, MAX_PATH_BYTES, METADATA_BLOCKS,
    MIN_VOLUME_BLOCKS,
};

const DEFAULT_SIZE_MIB: u64 = 64;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustos-vfs-image: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match arguments.as_slice() {
        [flag, path] if flag == "--verify" => verify(path),
        [path] => create(path, DEFAULT_SIZE_MIB, false),
        [path, size] => create(path, parse_size(size)?, false),
        [flag, path, size] if flag == "--force" => create(path, parse_size(size)?, true),
        [flag, image, host, destination] if flag == "--put" => {
            put(image, host, destination)
        }
        _ => Err("usage: rustos-vfs-image [--force] <image> [size-MiB]\n       rustos-vfs-image --verify <image>\n       rustos-vfs-image --put <image> <host-file> </absolute/path>".into()),
    }
}

fn parse_size(value: &str) -> Result<u64, String> {
    value.parse().map_err(|_| format!("invalid size: {value}"))
}

fn create(path: &str, size_mib: u64, force: bool) -> Result<(), String> {
    if std::path::Path::new(path).exists() && !force {
        println!("rustos-vfs-image: keep existing persistent image {path}");
        return verify(path);
    }
    let bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or("image is too large")?;
    if bytes % BLOCK_SIZE as u64 != 0 || bytes / (BLOCK_SIZE as u64) < MIN_VOLUME_BLOCKS {
        return Err("image must be aligned and at least 16 MiB".into());
    }
    let blocks = bytes / BLOCK_SIZE as u64;
    let mut file = File::create(path).map_err(|error| format!("{path}: {error}"))?;
    file.set_len(bytes)
        .map_err(|error| format!("set_len: {error}"))?;

    let mut metadata = Metadata::empty();
    for directory in [
        b"/system".as_slice(),
        b"/system/lib",
        b"/bin",
        b"/tmp",
        b"/home",
        b"/apps",
        b"/apps/loader-test",
    ] {
        add_directory(&mut metadata, directory)?;
    }
    let checksum = crc32(metadata.bytes());
    for slot in 0..2 {
        write_at(
            &mut file,
            metadata_slot_start(slot) * BLOCK_SIZE as u64,
            metadata.bytes(),
        )?;
    }
    let superblock = Superblock::new(blocks, metadata.sequence, 0, checksum);
    for copy in 0..2u64 {
        write_at(&mut file, copy * BLOCK_SIZE as u64, superblock.bytes())?;
    }
    file.sync_all().map_err(|error| format!("sync: {error}"))?;
    println!("rustos-vfs-image: created {path} ({size_mib} MiB, {blocks} blocks)");
    Ok(())
}

fn add_directory(metadata: &mut Metadata, path: &[u8]) -> Result<(), String> {
    if path.len() > MAX_PATH_BYTES {
        return Err("bootstrap directory path is too long".into());
    }
    let index = metadata.inode_count as usize;
    let inode = metadata
        .inodes
        .get_mut(index)
        .ok_or("too many bootstrap directories")?;
    *inode = Inode::EMPTY;
    inode.used = 1;
    inode.kind = kind::DIRECTORY;
    inode.generation = metadata.next_inode_generation;
    metadata.next_inode_generation += 1;
    inode.path_len = path.len() as u16;
    inode.path[..path.len()].copy_from_slice(path);
    metadata.inode_count += 1;
    Ok(())
}

fn verify(path: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| format!("{path}: {error}"))?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if length % BLOCK_SIZE as u64 != 0 {
        return Err("unaligned image length".into());
    }
    let blocks = length / BLOCK_SIZE as u64;
    let first = read_superblock(&mut file, 0, blocks).ok();
    let second = read_superblock(&mut file, 1, blocks).ok();
    let candidates = match (first, second) {
        (Some(a), Some(b)) if b.sequence > a.sequence => [Some(b), Some(a)],
        (Some(a), Some(b)) => [Some(a), Some(b)],
        (Some(a), None) => [Some(a), None],
        (None, Some(b)) => [Some(b), None],
        (None, None) => return Err("both superblock copies are invalid".into()),
    };

    // Как и mount в vfsd, не доверяем только самому новому superblock: при
    // оборванном commit откатываемся к предыдущей полностью валидной паре.
    for superblock in candidates.into_iter().flatten() {
        let mut metadata = Metadata::empty();
        read_at(
            &mut file,
            metadata_slot_start(superblock.active_slot) * BLOCK_SIZE as u64,
            metadata.bytes_mut(),
        )?;
        if crc32(metadata.bytes()) != superblock.metadata_crc32
            || metadata.sequence != superblock.sequence
            || metadata.next_data_block < 2 + u64::from(METADATA_BLOCKS) * 2
        {
            continue;
        }
        println!(
            "rustos-vfs-image: OK {path}, sequence={}, inodes={}",
            metadata.sequence, metadata.inode_count
        );
        return Ok(());
    }
    Err("no valid superblock/metadata pair".into())
}

fn put(image_path: &str, host_path: &str, destination: &str) -> Result<(), String> {
    let data = fs::read(host_path).map_err(|error| format!("{host_path}: {error}"))?;
    let path = destination.as_bytes();
    if !destination.starts_with('/')
        || destination.ends_with('/')
        || destination.contains("//")
        || destination
            .split('/')
            .any(|part| matches!(part, "." | ".."))
        || path.len() > MAX_PATH_BYTES
    {
        return Err("destination must be a normalized absolute VaraniaFS path".into());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image_path)
        .map_err(|error| format!("{image_path}: {error}"))?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if length % BLOCK_SIZE as u64 != 0 {
        return Err("unaligned image length".into());
    }
    let volume_blocks = length / BLOCK_SIZE as u64;
    let (superblock, mut metadata) = load_latest(&mut file, volume_blocks)?;
    ensure_parent_directories(&mut metadata, path)?;

    let (inode_index, previous_inode) = if let Some(index) = find_inode(&metadata, path) {
        if metadata.inodes[index].kind != kind::FILE {
            return Err("destination exists and is not a file".into());
        }
        (index, Some(metadata.inodes[index]))
    } else {
        let index = metadata
            .inodes
            .iter()
            .position(|inode| inode.used == 0)
            .ok_or("inode table is full")?;
        metadata.inode_count = metadata.inode_count.saturating_add(1);
        (index, None)
    };
    let generation = metadata.next_inode_generation;
    metadata.next_inode_generation = metadata.next_inode_generation.wrapping_add(1).max(1);
    let mut inode = Inode::EMPTY;
    inode.used = 1;
    inode.kind = kind::FILE;
    inode.generation = generation;
    inode.path_len = path.len() as u16;
    inode.path[..path.len()].copy_from_slice(path);
    inode.size = data.len() as u64;

    let blocks = data.len().div_ceil(BLOCK_SIZE);
    let mut block_buffer = [0u8; BLOCK_SIZE];
    for logical in 0..blocks {
        let physical = allocate_block(&mut metadata, volume_blocks)?;
        append_inode_block(&mut inode, logical as u64, physical)?;
        block_buffer.fill(0);
        let start = logical * BLOCK_SIZE;
        let end = (start + BLOCK_SIZE).min(data.len());
        block_buffer[..end - start].copy_from_slice(&data[start..end]);
        write_at(&mut file, physical * BLOCK_SIZE as u64, &block_buffer)?;
    }
    // Старые extent'ы освобождаем только после успешной записи новых данных.
    // До metadata commit предыдущая snapshot продолжает указывать на
    // неизменённые блоки, поэтому оборванный host import безопасно откатится.
    if let Some(previous) = previous_inode {
        for extent in previous.extents.iter().take(previous.extent_count as usize) {
            release_extent(&mut metadata, extent.physical, extent.blocks)?;
        }
    }
    metadata.inodes[inode_index] = inode;
    file.sync_all()
        .map_err(|error| format!("data sync: {error}"))?;

    metadata.sequence = metadata.sequence.wrapping_add(1).max(1);
    let inactive = 1 - superblock.active_slot;
    write_at(
        &mut file,
        metadata_slot_start(inactive) * BLOCK_SIZE as u64,
        metadata.bytes(),
    )?;
    file.sync_all()
        .map_err(|error| format!("metadata sync: {error}"))?;
    let next = Superblock::new(
        volume_blocks,
        metadata.sequence,
        inactive,
        crc32(metadata.bytes()),
    );
    write_at(
        &mut file,
        (metadata.sequence & 1) * BLOCK_SIZE as u64,
        next.bytes(),
    )?;
    file.sync_all()
        .map_err(|error| format!("superblock sync: {error}"))?;
    println!(
        "rustos-vfs-image: put {host_path} -> {destination} ({} bytes, sequence={})",
        data.len(),
        metadata.sequence
    );
    Ok(())
}

fn load_latest(file: &mut File, blocks: u64) -> Result<(Superblock, Metadata), String> {
    let first = read_superblock(file, 0, blocks).ok();
    let second = read_superblock(file, 1, blocks).ok();
    let candidates = match (first, second) {
        (Some(a), Some(b)) if b.sequence > a.sequence => [Some(b), Some(a)],
        (Some(a), Some(b)) => [Some(a), Some(b)],
        (Some(a), None) => [Some(a), None],
        (None, Some(b)) => [Some(b), None],
        (None, None) => return Err("both superblock copies are invalid".into()),
    };
    for superblock in candidates.into_iter().flatten() {
        let mut metadata = Metadata::empty();
        read_at(
            file,
            metadata_slot_start(superblock.active_slot) * BLOCK_SIZE as u64,
            metadata.bytes_mut(),
        )?;
        if metadata.sequence == superblock.sequence
            && crc32(metadata.bytes()) == superblock.metadata_crc32
        {
            return Ok((superblock, metadata));
        }
    }
    Err("no valid superblock/metadata pair".into())
}

fn ensure_parent_directories(metadata: &mut Metadata, path: &[u8]) -> Result<(), String> {
    for slash in path
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(index, byte)| (*byte == b'/').then_some(index))
    {
        let parent = &path[..slash];
        if let Some(index) = find_inode(metadata, parent) {
            if metadata.inodes[index].kind != kind::DIRECTORY {
                return Err("path parent is not a directory".into());
            }
        } else {
            add_directory(metadata, parent)?;
        }
    }
    Ok(())
}

fn find_inode(metadata: &Metadata, path: &[u8]) -> Option<usize> {
    metadata
        .inodes
        .iter()
        .position(|inode| inode.used != 0 && inode.path() == path)
}

fn allocate_block(metadata: &mut Metadata, volume_blocks: u64) -> Result<u64, String> {
    for index in 0..metadata.free_extent_count as usize {
        if metadata.free_extents[index].blocks == 0 {
            continue;
        }
        let block = metadata.free_extents[index].start;
        metadata.free_extents[index].start += 1;
        metadata.free_extents[index].blocks -= 1;
        if metadata.free_extents[index].blocks == 0 {
            remove_free_extent(metadata, index);
        }
        return Ok(block);
    }
    if metadata.next_data_block >= volume_blocks {
        return Err("volume is full".into());
    }
    let block = metadata.next_data_block;
    metadata.next_data_block += 1;
    Ok(block)
}

fn append_inode_block(inode: &mut Inode, logical: u64, physical: u64) -> Result<(), String> {
    if inode.extent_count != 0 {
        let last = &mut inode.extents[inode.extent_count as usize - 1];
        if last.logical + last.blocks == logical && last.physical + last.blocks == physical {
            last.blocks += 1;
            return Ok(());
        }
    }
    if inode.extent_count as usize == MAX_EXTENTS_PER_INODE {
        return Err("file extent table is full".into());
    }
    inode.extents[inode.extent_count as usize] = FileExtent {
        logical,
        physical,
        blocks: 1,
    };
    inode.extent_count += 1;
    Ok(())
}

fn release_extent(metadata: &mut Metadata, start: u64, blocks: u64) -> Result<(), String> {
    if blocks == 0 {
        return Ok(());
    }
    let end = start.checked_add(blocks).ok_or("invalid extent")?;
    for index in 0..metadata.free_extent_count as usize {
        let current = metadata.free_extents[index];
        let current_end = current
            .start
            .checked_add(current.blocks)
            .ok_or("invalid free extent")?;
        if current_end == start || end == current.start {
            metadata.free_extents[index] = FreeExtent {
                start: current.start.min(start),
                blocks: current.blocks + blocks,
            };
            return Ok(());
        }
        if start < current_end && current.start < end {
            return Err("overlapping free extent".into());
        }
    }
    if metadata.free_extent_count as usize == MAX_FREE_EXTENTS {
        return Err("free extent table is full".into());
    }
    let index = metadata.free_extent_count as usize;
    metadata.free_extents[index] = FreeExtent { start, blocks };
    metadata.free_extent_count += 1;
    Ok(())
}

fn remove_free_extent(metadata: &mut Metadata, index: usize) {
    let count = metadata.free_extent_count as usize;
    for cursor in index..count - 1 {
        metadata.free_extents[cursor] = metadata.free_extents[cursor + 1];
    }
    metadata.free_extents[count - 1] = FreeExtent::EMPTY;
    metadata.free_extent_count -= 1;
}

fn read_superblock(file: &mut File, copy: u64, blocks: u64) -> Result<Superblock, String> {
    let mut superblock = Superblock::new(0, 0, 0, 0);
    read_at(file, copy * BLOCK_SIZE as u64, superblock.bytes_mut())?;
    if superblock.validate(blocks) {
        Ok(superblock)
    } else {
        Err(format!("invalid superblock copy {copy}"))
    }
}

fn write_at(file: &mut File, offset: u64, bytes: &[u8]) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())
}

fn read_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| error.to_string())?;
    file.read_exact(bytes).map_err(|error| error.to_string())
}

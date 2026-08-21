//! Host-утилита создания и проверки постоянного VaraniaFS-диска.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};
use varaniafs::v2;
use varaniafs::{
    crc32, kind, metadata_slot_start, FileExtent, FreeExtent, Inode, Metadata, Superblock,
    BLOCK_SIZE, MAX_EXTENTS_PER_INODE, MAX_FREE_EXTENTS, MAX_PATH_BYTES, METADATA_BLOCKS,
    MIN_VOLUME_BLOCKS,
};

const DEFAULT_SIZE_MIB: u64 = 1024;

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
        [flag, path] if flag == "--verify-v2" => verify_v2(path),
        [flag, path, size, uuid] if flag == "--create-v2" => {
            create_v2(path, parse_size(size)?, parse_uuid(uuid)?)
        }
        [path] => create(path, DEFAULT_SIZE_MIB, false),
        [path, size] => create(path, parse_size(size)?, false),
        [flag, path, size] if flag == "--force" => create(path, parse_size(size)?, true),
        [flag, path, size] if flag == "--grow" => grow(path, parse_size(size)?),
        [flag, image, host, destination] if flag == "--put" => {
            put(image, host, destination)
        }
        _ => Err("usage: rustos-vfs-image [--force] <image> [size-MiB]\n       rustos-vfs-image --grow <image> <minimum-size-MiB>\n       rustos-vfs-image --verify <image>\n       rustos-vfs-image --put <image> <host-file> </absolute/path>\n       rustos-vfs-image --create-v2 <image> <size-MiB> <32-hex-uuid>\n       rustos-vfs-image --verify-v2 <image>".into()),
    }
}

/// Создаёт отдельный экспериментальный v2 volume через temporary output.
/// Рабочий системный образ остаётся v1 до завершения migration gates.
fn create_v2(path: &str, size_mib: u64, uuid: [u8; 16]) -> Result<(), String> {
    let target = Path::new(path);
    if target.exists() {
        return Err(format!("{path}: destination already exists"));
    }
    let bytes = checked_volume_bytes(size_mib)?;
    let blocks = bytes / BLOCK_SIZE as u64;
    let image = v2::format_empty(blocks, uuid).map_err(v2_error)?;
    let temporary = temporary_path(target);
    let result: Result<(), String> = (|| {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        file.set_len(bytes)
            .map_err(|error| format!("v2 set_len: {error}"))?;
        for copy in 0..v2::SUPERBLOCK_COPIES {
            write_at(&mut file, copy * BLOCK_SIZE as u64, &image.superblock)?;
        }
        for (index, root) in image.roots.iter().enumerate() {
            let block = v2::FIRST_ALLOCATABLE_BLOCK + index as u64;
            write_at(&mut file, block * BLOCK_SIZE as u64, root)?;
        }
        file.sync_all()
            .map_err(|error| format!("v2 sync: {error}"))?;
        drop(file);
        verify_v2_path(&temporary)?;
        fs::rename(&temporary, target).map_err(|error| format!("publish {path}: {error}"))?;
        sync_parent_directory(target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    println!("rustos-vfs-image: created v2 {path} ({size_mib} MiB, {blocks} blocks)");
    Ok(())
}

fn verify_v2(path: &str) -> Result<(), String> {
    verify_v2_path(Path::new(path))
}

fn verify_v2_path(path: &Path) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if length % BLOCK_SIZE as u64 != 0 {
        return Err("unaligned v2 image length".into());
    }
    let blocks = length / BLOCK_SIZE as u64;
    let mut io_error = None;
    let mounted = v2::recover(blocks, |block, output| {
        let Some(offset) = block.checked_mul(BLOCK_SIZE as u64) else {
            io_error = Some("v2 block offset overflow".into());
            return false;
        };
        match read_at(&mut file, offset, output) {
            Ok(()) => true,
            Err(error) => {
                io_error = Some(error);
                false
            }
        }
    })
    .map_err(v2_error)?;
    if let Some(error) = io_error {
        return Err(error);
    }
    println!(
        "rustos-vfs-image: OK v2 {}, sequence={}, copy={}",
        path.display(),
        mounted.superblock.sequence,
        mounted.copy
    );
    Ok(())
}

fn checked_volume_bytes(size_mib: u64) -> Result<u64, String> {
    let bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or("image is too large")?;
    if bytes % BLOCK_SIZE as u64 != 0 || bytes / (BLOCK_SIZE as u64) < MIN_VOLUME_BLOCKS {
        return Err("image must be aligned and at least 16 MiB".into());
    }
    Ok(bytes)
}

fn parse_uuid(value: &str) -> Result<[u8; 16], String> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("v2 UUID must contain exactly 32 hexadecimal digits".into());
    }
    let mut uuid = [0u8; 16];
    for (index, output) in uuid.iter_mut().enumerate() {
        let offset = index * 2;
        *output =
            u8::from_str_radix(&value[offset..offset + 2], 16).map_err(|_| "invalid v2 UUID")?;
    }
    if !uuid.iter().any(|byte| *byte != 0) {
        return Err("v2 UUID must not be zero".into());
    }
    Ok(uuid)
}

fn temporary_path(target: &Path) -> PathBuf {
    let mut temporary = target.as_os_str().to_os_string();
    temporary.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(temporary)
}

/// `sync_all` самого image не делает durable запись имени после `rename`.
/// Синхронизация parent directory завершает host-side atomic publication на
/// поддерживаемых macOS/Linux filesystem; ошибка не маскируется как успех.
fn sync_parent_directory(target: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync parent {}: {error}", parent.display()))
}

fn v2_error(error: v2::Error) -> String {
    format!("VaraniaFS v2: {error:?}")
}

/// Не разрушая существующий том, доводит его до размера developer profile.
/// Файл остаётся sparse на host, поэтому 1 ГиБ адресного пространства диска
/// не означает немедленного расхода 1 ГиБ на SSD.
fn grow(path: &str, size_mib: u64) -> Result<(), String> {
    let requested_bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or("image is too large")?;
    if requested_bytes % BLOCK_SIZE as u64 != 0
        || requested_bytes / (BLOCK_SIZE as u64) < MIN_VOLUME_BLOCKS
    {
        return Err("image must be aligned and at least 16 MiB".into());
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("{path}: {error}"))?;
    let old_bytes = file.metadata().map_err(|error| error.to_string())?.len();
    if old_bytes % BLOCK_SIZE as u64 != 0 {
        return Err("unaligned image length".into());
    }
    if old_bytes >= requested_bytes {
        println!(
            "rustos-vfs-image: keep {path} at {} MiB (minimum {size_mib} MiB)",
            old_bytes / 1024 / 1024
        );
        return verify(path);
    }

    let old_blocks = old_bytes / BLOCK_SIZE as u64;
    let (superblock, mut metadata) = load_latest(&mut file, old_blocks)?;
    file.set_len(requested_bytes)
        .map_err(|error| format!("grow set_len: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("grow data sync: {error}"))?;

    // Публикация использует тот же copy-on-write порядок, что vfsd: новая
    // metadata snapshot, flush, затем один новый superblock. Благодаря
    // `Superblock::validate(old <= actual)` старая копия остаётся recovery
    // point даже при выключении между set_len и последним flush.
    metadata.sequence = metadata.sequence.wrapping_add(1).max(1);
    let inactive = 1 - superblock.active_slot;
    write_at(
        &mut file,
        metadata_slot_start(inactive) * BLOCK_SIZE as u64,
        metadata.bytes(),
    )?;
    file.sync_all()
        .map_err(|error| format!("grow metadata sync: {error}"))?;
    let new_blocks = requested_bytes / BLOCK_SIZE as u64;
    let next = Superblock::new(
        new_blocks,
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
        .map_err(|error| format!("grow superblock sync: {error}"))?;
    println!(
        "rustos-vfs-image: grew {path}: {} -> {size_mib} MiB",
        old_bytes / 1024 / 1024
    );
    verify(path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);
    const TEST_UUID_TEXT: &str = "00112233445566778899aabbccddeeff";

    fn test_path(label: &str) -> PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rustos-vfs-image-{label}-{}-{id}.vfs",
            std::process::id()
        ))
    }

    #[test]
    fn v2_create_is_deterministic_and_verified_before_publication() {
        let first = test_path("deterministic-a");
        let second = test_path("deterministic-b");
        let uuid = parse_uuid(TEST_UUID_TEXT).unwrap();
        create_v2(first.to_str().unwrap(), 16, uuid).unwrap();
        create_v2(second.to_str().unwrap(), 16, uuid).unwrap();
        verify_v2_path(&first).unwrap();
        verify_v2_path(&second).unwrap();

        let prefix_bytes = (v2::SUPERBLOCK_COPIES + v2::ROOT_COUNT as u64) * BLOCK_SIZE as u64;
        let mut first_prefix = vec![0u8; prefix_bytes as usize];
        let mut second_prefix = vec![0u8; prefix_bytes as usize];
        File::open(&first)
            .unwrap()
            .read_exact(&mut first_prefix)
            .unwrap();
        File::open(&second)
            .unwrap()
            .read_exact(&mut second_prefix)
            .unwrap();
        assert_eq!(first_prefix, second_prefix);

        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
    }

    #[test]
    fn v2_verify_rejects_corrupt_root_node() {
        let path = test_path("corrupt-root");
        create_v2(
            path.to_str().unwrap(),
            16,
            parse_uuid(TEST_UUID_TEXT).unwrap(),
        )
        .unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(
            v2::FIRST_ALLOCATABLE_BLOCK * BLOCK_SIZE as u64 + 100,
        ))
        .unwrap();
        file.write_all(&[0x5a]).unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert!(verify_v2_path(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn v2_uuid_parser_is_strict() {
        assert_eq!(
            parse_uuid(TEST_UUID_TEXT).unwrap(),
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
        assert!(parse_uuid("0011").is_err());
        assert!(parse_uuid("00112233445566778899aabbccddeefz").is_err());
        assert!(parse_uuid("00000000000000000000000000000000").is_err());
    }
}

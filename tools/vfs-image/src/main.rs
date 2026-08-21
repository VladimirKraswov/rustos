//! Host-утилита создания, миграции, наполнения и проверки VaraniaFS.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};
use varaniafs::{
    experimental_import as import, file,
    format::{self, format_empty, Block, Error, InodeKind, Superblock, FIRST_ALLOCATABLE_BLOCK},
    integrity, namespace,
    tree::{BlockDevice, Transaction, TransactionWorkspace},
    BLOCK_SIZE, MIN_VOLUME_BLOCKS,
};

const DEFAULT_SIZE_MIB: u64 = 1024;
const DEFAULT_UUID: [u8; 16] = *b"RustOS-VaraniaFS";
const STREAM_BLOCKS: usize = 8;

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
        [path] => create_or_keep(path, DEFAULT_SIZE_MIB),
        [flag, path] if flag == "--verify" || flag == "--fsck" => verify(path),
        [flag, path] if flag == "--scrub" => scrub(path),
        [path, size] => create_or_keep(path, parse_size(size)?),
        [flag, path, size] if flag == "--force" => create_atomic(path, parse_size(size)?, true),
        [flag, path, size] if flag == "--grow" => grow(path, parse_size(size)?),
        [flag, image, host, destination] if flag == "--put" => put(image, host, destination),
        [flag, source, destination] if flag == "--migrate-experimental" => {
            migrate(Path::new(source), Path::new(destination), false)
        }
        _ => Err("usage: rustos-vfs-image [--force] <image> [size-MiB]\n       rustos-vfs-image --grow <image> <minimum-size-MiB>\n       rustos-vfs-image --verify|--fsck <image>\n       rustos-vfs-image --scrub <image>\n       rustos-vfs-image --put <image> <host-file> </absolute/path>\n       rustos-vfs-image --migrate-experimental <source> <destination>".into()),
    }
}

struct FileDevice {
    file: File,
    blocks: u64,
}

impl FileDevice {
    fn open(path: &Path, writable: bool) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let bytes = file.metadata().map_err(|error| error.to_string())?.len();
        if bytes % BLOCK_SIZE as u64 != 0 || bytes / (BLOCK_SIZE as u64) < MIN_VOLUME_BLOCKS {
            return Err(format!("{}: invalid aligned volume size", path.display()));
        }
        Ok(Self {
            file,
            blocks: bytes / BLOCK_SIZE as u64,
        })
    }

    fn mount(&mut self) -> Result<Superblock, String> {
        let recovered = format::recover_latest(self.blocks, |number, output| {
            self.read(number, output).is_ok()
        })
        .map_err(fs_error)?;
        Ok(recovered.superblock)
    }
}

impl BlockDevice for FileDevice {
    fn read(&mut self, block: u64, output: &mut Block) -> Result<(), Error> {
        if block >= self.blocks {
            return Err(Error::Io);
        }
        self.file
            .seek(SeekFrom::Start(block * BLOCK_SIZE as u64))
            .and_then(|_| self.file.read_exact(output))
            .map_err(|_| Error::Io)
    }

    fn write(&mut self, block: u64, input: &Block) -> Result<(), Error> {
        if block >= self.blocks {
            return Err(Error::Io);
        }
        self.file
            .seek(SeekFrom::Start(block * BLOCK_SIZE as u64))
            .and_then(|_| self.file.write_all(input))
            .map_err(|_| Error::Io)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.file.sync_data().map_err(|_| Error::Io)
    }
}

fn create_or_keep(path: &str, size_mib: u64) -> Result<(), String> {
    let target = Path::new(path);
    if !target.exists() {
        return create_atomic(path, size_mib, false);
    }
    if verify_path(target, false).is_ok() {
        println!("rustos-vfs-image: keep {}", target.display());
        return Ok(());
    }
    if load_import(target).is_ok() {
        let temporary = temporary_path(target);
        migrate(target, &temporary, false)?;
        let backup = backup_path(target);
        if backup.exists() {
            return Err(format!(
                "refusing to overwrite migration backup {}",
                backup.display()
            ));
        }
        fs::rename(target, &backup)
            .map_err(|error| format!("backup {}: {error}", target.display()))?;
        fs::rename(&temporary, target)
            .map_err(|error| format!("publish migrated {}: {error}", target.display()))?;
        sync_parent(target)?;
        println!(
            "rustos-vfs-image: migrated {}, recoverable backup: {}",
            target.display(),
            backup.display()
        );
        return Ok(());
    }
    Err(format!(
        "{} exists but is neither VaraniaFS nor a supported experimental image",
        target.display()
    ))
}

fn create_atomic(path: &str, size_mib: u64, replace: bool) -> Result<(), String> {
    let target = Path::new(path);
    if target.exists() && !replace {
        return Err(format!("{} already exists", target.display()));
    }
    let temporary = temporary_path(target);
    if temporary.exists() {
        fs::remove_file(&temporary)
            .map_err(|error| format!("remove {}: {error}", temporary.display()))?;
    }
    let bytes = checked_bytes(size_mib)?;
    let blocks = bytes / BLOCK_SIZE as u64;
    let empty = format_empty(blocks, DEFAULT_UUID).map_err(fs_error)?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        file.set_len(bytes).map_err(|error| error.to_string())?;
        write_at(&mut file, 0, &empty.superblock)?;
        write_at(&mut file, BLOCK_SIZE as u64, &empty.superblock)?;
        for (index, root) in empty.roots.iter().enumerate() {
            let primary = FIRST_ALLOCATABLE_BLOCK + index as u64 * 2;
            write_at(&mut file, primary * BLOCK_SIZE as u64, root)?;
            write_at(&mut file, (primary + 1) * BLOCK_SIZE as u64, root)?;
        }
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        verify_path(&temporary, false)?;
        fs::rename(&temporary, target)
            .map_err(|error| format!("publish {}: {error}", target.display()))?;
        sync_parent(target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    println!("rustos-vfs-image: created {path} ({size_mib} MiB)");
    Ok(())
}

fn verify(path: &str) -> Result<(), String> {
    let report = verify_path(Path::new(path), true)?;
    if !report.is_clean() {
        return Err(format!("{}: fsck found {report:?}", path));
    }
    Ok(())
}

fn verify_path(path: &Path, verbose: bool) -> Result<integrity::ScrubReport, String> {
    let mut device = FileDevice::open(path, false)?;
    let mounted = device.mount()?;
    let report = integrity::fsck(&mut device, mounted).map_err(fs_error)?;
    if verbose {
        println!(
            "rustos-vfs-image: OK {}, sequence={}, nodes={}, items={}, data-blocks={}",
            path.display(),
            mounted.sequence,
            report.metadata_nodes,
            report.metadata_items,
            report.data_blocks
        );
    }
    Ok(report)
}

fn scrub(path: &str) -> Result<(), String> {
    let target = Path::new(path);
    let mut device = FileDevice::open(target, true)?;
    let mounted = device.mount()?;
    let report = integrity::scrub(&mut device, mounted, true).map_err(fs_error)?;
    println!("rustos-vfs-image: scrub {}: {report:?}", target.display());
    if report.is_clean() {
        Ok(())
    } else {
        Err("unrecoverable filesystem damage remains".into())
    }
}

fn grow(path: &str, size_mib: u64) -> Result<(), String> {
    let target = Path::new(path);
    let requested = checked_bytes(size_mib)?;
    let mut device = FileDevice::open(target, true)?;
    let old_bytes = device.blocks * BLOCK_SIZE as u64;
    if requested <= old_bytes {
        println!(
            "rustos-vfs-image: keep {path} at {} MiB",
            old_bytes / 1024 / 1024
        );
        return verify(path);
    }
    let mounted = device.mount()?;
    device
        .file
        .set_len(requested)
        .map_err(|error| error.to_string())?;
    device.file.sync_all().map_err(|error| error.to_string())?;
    device.blocks = requested / BLOCK_SIZE as u64;
    let mut grown = mounted;
    grown.sequence = mounted.sequence.wrapping_add(1).max(1);
    grown.volume_blocks = device.blocks;
    let encoded = grown.encode().map_err(fs_error)?;
    device
        .write(format::superblock_copy(grown.sequence), &encoded)
        .map_err(fs_error)?;
    device.flush().map_err(fs_error)?;
    drop(device);
    verify(path)?;
    println!("rustos-vfs-image: grew {path} to {size_mib} MiB");
    Ok(())
}

fn put(image: &str, host: &str, destination: &str) -> Result<(), String> {
    if !destination.starts_with('/') || destination == "/" {
        return Err("destination must be an absolute file path".into());
    }
    let mut source = File::open(host).map_err(|error| format!("{host}: {error}"))?;
    let source_len = source.metadata().map_err(|error| error.to_string())?.len();
    let mut device = FileDevice::open(Path::new(image), true)?;
    let mut mounted = device.mount()?;
    mounted = ensure_parents(&mut device, mounted, destination.as_bytes())?;
    let mut workspace = TransactionWorkspace::new();
    let object = {
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut workspace).map_err(fs_error)?;
        let object = match namespace::resolve(&mut transaction, destination.as_bytes()) {
            Ok(object) => {
                if namespace::inode(&mut transaction, object)
                    .map_err(namespace_error)?
                    .kind
                    != InodeKind::File
                {
                    return Err("destination is a directory".into());
                }
                file::resize(&mut transaction, object, 0, 0).map_err(fs_error)?;
                object
            }
            Err(namespace::NamespaceError::NotFound) => {
                namespace::create(&mut transaction, destination.as_bytes(), InodeKind::File, 0)
                    .map_err(namespace_error)?
            }
            Err(error) => return Err(namespace_error(error)),
        };
        mounted = transaction.commit().map_err(fs_error)?;
        object
    };
    let mut buffer = vec![0u8; STREAM_BLOCKS * BLOCK_SIZE];
    let mut offset = 0u64;
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        let mut transaction =
            Transaction::begin(&mut device, mounted, &mut workspace).map_err(fs_error)?;
        file::write_at(&mut transaction, object, offset, &buffer[..count], 0).map_err(fs_error)?;
        mounted = transaction.commit().map_err(fs_error)?;
        offset += count as u64;
    }
    let mut transaction =
        Transaction::begin(&mut device, mounted, &mut workspace).map_err(fs_error)?;
    file::resize(&mut transaction, object, source_len, 0).map_err(fs_error)?;
    transaction.commit().map_err(fs_error)?;
    drop(device);
    verify(image)?;
    println!("rustos-vfs-image: put {host} -> {image}:{destination} ({source_len} bytes)");
    Ok(())
}

fn ensure_parents(
    device: &mut FileDevice,
    mut mounted: Superblock,
    destination: &[u8],
) -> Result<Superblock, String> {
    let split = destination
        .iter()
        .rposition(|byte| *byte == b'/')
        .ok_or("invalid destination")?;
    let parent = &destination[..split];
    if parent.is_empty() {
        return Ok(mounted);
    }
    let mut path = Vec::with_capacity(parent.len());
    let mut workspace = TransactionWorkspace::new();
    for component in parent[1..].split(|byte| *byte == b'/') {
        path.push(b'/');
        path.extend_from_slice(component);
        let mut transaction =
            Transaction::begin(device, mounted, &mut workspace).map_err(fs_error)?;
        match namespace::resolve(&mut transaction, &path) {
            Ok(object) => {
                if namespace::inode(&mut transaction, object)
                    .map_err(namespace_error)?
                    .kind
                    != InodeKind::Directory
                {
                    return Err(format!(
                        "{} is not a directory",
                        String::from_utf8_lossy(&path)
                    ));
                }
            }
            Err(namespace::NamespaceError::NotFound) => {
                namespace::create(&mut transaction, &path, InodeKind::Directory, 0)
                    .map_err(namespace_error)?;
                mounted = transaction.commit().map_err(fs_error)?;
            }
            Err(error) => return Err(namespace_error(error)),
        }
    }
    Ok(mounted)
}

fn migrate(source: &Path, destination: &Path, replace: bool) -> Result<(), String> {
    if destination.exists() && !replace {
        return Err(format!("{} already exists", destination.display()));
    }
    let (source_superblock, metadata) = load_import(source)?;
    let size_mib = source_superblock.volume_blocks * import::BLOCK_SIZE as u64 / 1024 / 1024;
    create_atomic(
        destination
            .to_str()
            .ok_or("destination path is not UTF-8")?,
        size_mib,
        replace,
    )?;
    let mut target = FileDevice::open(destination, true)?;
    let mut mounted = target.mount()?;
    let mut workspace = TransactionWorkspace::new();
    let mut migrated = [false; import::MAX_INODES];
    for _ in 0..import::MAX_INODES {
        let mut progress = false;
        for (index, inode) in metadata.inodes.iter().enumerate() {
            if migrated[index] || inode.used == 0 || inode.kind != import::kind::DIRECTORY {
                continue;
            }
            let mut transaction =
                Transaction::begin(&mut target, mounted, &mut workspace).map_err(fs_error)?;
            match namespace::create(&mut transaction, inode.path(), InodeKind::Directory, 0) {
                Ok(_) => {
                    mounted = transaction.commit().map_err(fs_error)?;
                    migrated[index] = true;
                    progress = true;
                }
                Err(namespace::NamespaceError::NotFound) => {}
                Err(error) => return Err(namespace_error(error)),
            }
        }
        if !progress {
            break;
        }
    }
    let mut source_file = File::open(source).map_err(|error| error.to_string())?;
    for (index, inode) in metadata.inodes.iter().enumerate() {
        if inode.used == 0 || inode.kind != import::kind::FILE {
            continue;
        }
        let object = {
            let mut transaction =
                Transaction::begin(&mut target, mounted, &mut workspace).map_err(fs_error)?;
            let object = namespace::create(&mut transaction, inode.path(), InodeKind::File, 0)
                .map_err(namespace_error)?;
            mounted = transaction.commit().map_err(fs_error)?;
            object
        };
        let mut offset = 0u64;
        let mut buffer = vec![0; STREAM_BLOCKS * BLOCK_SIZE];
        while offset < inode.size {
            let count = (inode.size - offset).min(buffer.len() as u64) as usize;
            read_import_file(&mut source_file, inode, offset, &mut buffer[..count])?;
            if buffer[..count].iter().any(|byte| *byte != 0) {
                let mut transaction =
                    Transaction::begin(&mut target, mounted, &mut workspace).map_err(fs_error)?;
                file::write_at(&mut transaction, object, offset, &buffer[..count], 0)
                    .map_err(fs_error)?;
                mounted = transaction.commit().map_err(fs_error)?;
            }
            offset += count as u64;
        }
        let mut transaction =
            Transaction::begin(&mut target, mounted, &mut workspace).map_err(fs_error)?;
        file::resize(&mut transaction, object, inode.size, 0).map_err(fs_error)?;
        mounted = transaction.commit().map_err(fs_error)?;
        migrated[index] = true;
    }
    if metadata
        .inodes
        .iter()
        .enumerate()
        .any(|(index, inode)| inode.used != 0 && !migrated[index])
    {
        return Err("experimental image contains entries with unresolved parents".into());
    }
    drop(target);
    verify_path(destination, false)?;
    Ok(())
}

fn load_import(path: &Path) -> Result<(import::Superblock, import::Metadata), String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let actual =
        file.metadata().map_err(|error| error.to_string())?.len() / import::BLOCK_SIZE as u64;
    let mut candidates = [None, None];
    for copy in 0..2u64 {
        let mut superblock = import::Superblock::new(import::MIN_VOLUME_BLOCKS, 1, 0, 0);
        read_at(
            &mut file,
            copy * import::BLOCK_SIZE as u64,
            superblock.bytes_mut(),
        )?;
        if superblock.validate(actual) {
            candidates[copy as usize] = Some(superblock);
        }
    }
    let mut order = candidates;
    if order[1].is_some_and(|second| order[0].is_none_or(|first| second.sequence > first.sequence))
    {
        order.swap(0, 1);
    }
    for superblock in order.into_iter().flatten() {
        let mut metadata = import::Metadata::empty();
        read_at(
            &mut file,
            import::metadata_slot_start(superblock.active_slot) * import::BLOCK_SIZE as u64,
            metadata.bytes_mut(),
        )?;
        if metadata.sequence == superblock.sequence
            && import::crc32(metadata.bytes()) == superblock.metadata_crc32
        {
            return Ok((superblock, metadata));
        }
    }
    Err("unsupported experimental image".into())
}

fn read_import_file(
    file: &mut File,
    inode: &import::Inode,
    offset: u64,
    output: &mut [u8],
) -> Result<(), String> {
    output.fill(0);
    let mut done = 0usize;
    let mut block = [0; import::BLOCK_SIZE];
    while done < output.len() {
        let position = offset + done as u64;
        let logical = position / import::BLOCK_SIZE as u64;
        let within = (position % import::BLOCK_SIZE as u64) as usize;
        let count = (output.len() - done).min(import::BLOCK_SIZE - within);
        if let Some(physical) = inode
            .extents
            .iter()
            .take(inode.extent_count as usize)
            .find_map(|extent| {
                (logical >= extent.logical && logical < extent.logical + extent.blocks)
                    .then_some(extent.physical + logical - extent.logical)
            })
        {
            read_at(file, physical * import::BLOCK_SIZE as u64, &mut block)?;
            output[done..done + count].copy_from_slice(&block[within..within + count]);
        }
        done += count;
    }
    Ok(())
}

fn checked_bytes(size_mib: u64) -> Result<u64, String> {
    let bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or("image is too large")?;
    if bytes % BLOCK_SIZE as u64 != 0 || bytes / (BLOCK_SIZE as u64) < MIN_VOLUME_BLOCKS {
        return Err("image must be aligned and at least 16 MiB".into());
    }
    Ok(bytes)
}

fn parse_size(value: &str) -> Result<u64, String> {
    value.parse().map_err(|_| "invalid size-MiB".into())
}

fn temporary_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_os_string();
    path.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(path)
}

fn backup_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_os_string();
    path.push(".pre-migration-backup");
    PathBuf::from(path)
}

fn sync_parent(target: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync {}: {error}", parent.display()))
}

fn write_at(file: &mut File, offset: u64, bytes: &[u8]) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.write_all(bytes))
        .map_err(|error| error.to_string())
}

fn read_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(bytes))
        .map_err(|error| error.to_string())
}

fn fs_error(error: Error) -> String {
    format!("VaraniaFS: {error:?}")
}

fn namespace_error(error: namespace::NamespaceError) -> String {
    format!("VaraniaFS namespace: {error:?}")
}

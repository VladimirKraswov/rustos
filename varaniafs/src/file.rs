//! Потоковый ввод-вывод файлов поверх extent/checksum B+tree.
//!
//! Одна операция обрабатывает только caller buffer и один 4-KiB scratch block.
//! Partial-block write сначала читает старое содержимое, затем создаёт новый
//! data block; in-place перезаписи нет, поэтому snapshot и старый superblock
//! никогда не видят наполовину изменённый файл.

use crate::{
    allocator::Extent,
    format::{
        crc32c, extent_key, object_key, ChecksumAlgorithm, DataChecksumValue, Error, ExtentValue,
        InodeKind, InodeValue, ReliabilityClass, TreeKind,
    },
    tree::{BlockDevice, Transaction, MAX_VALUE_BYTES},
    BLOCK_SIZE,
};

pub fn read_at<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    object_id: u64,
    offset: u64,
    output: &mut [u8],
) -> Result<usize, Error> {
    let inode = load_inode(transaction, object_id)?;
    if inode.kind != InodeKind::File {
        return Err(Error::InvalidArgument);
    }
    let available = inode.size.saturating_sub(offset);
    let total = output
        .len()
        .min(usize::try_from(available).unwrap_or(usize::MAX));
    let mut done = 0usize;
    let mut block = [0; BLOCK_SIZE];
    let mut value = [0; MAX_VALUE_BYTES];
    while done < total {
        let position = offset.checked_add(done as u64).ok_or(Error::Capacity)?;
        let logical = position / BLOCK_SIZE as u64;
        let within = (position % BLOCK_SIZE as u64) as usize;
        let count = (total - done).min(BLOCK_SIZE - within);
        if let Some(found) = transaction.lookup(
            TreeKind::Extent,
            &extent_key(object_id, logical),
            &mut value,
        )? {
            let extent = ExtentValue::decode(
                &value[..usize::from(found.length)],
                transaction.volume_blocks(),
            )?;
            if extent.blocks != 1 || extent.reliability == ReliabilityClass::Ephemeral {
                return Err(Error::InvalidItem);
            }
            transaction.read_data(extent.physical, &mut block)?;
            verify_block(transaction, extent.physical, &block, &mut value)?;
        } else {
            block.fill(0);
        }
        output[done..done + count].copy_from_slice(&block[within..within + count]);
        done += count;
    }
    Ok(done)
}

pub fn write_at<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    object_id: u64,
    offset: u64,
    input: &[u8],
    modified_ns: u64,
) -> Result<usize, Error> {
    let mut inode = load_inode(transaction, object_id)?;
    if inode.kind != InodeKind::File {
        return Err(Error::InvalidArgument);
    }
    let end = offset
        .checked_add(input.len() as u64)
        .ok_or(Error::Capacity)?;
    let mut done = 0usize;
    let mut block = [0; BLOCK_SIZE];
    let mut value = [0; MAX_VALUE_BYTES];
    while done < input.len() {
        let position = offset.checked_add(done as u64).ok_or(Error::Capacity)?;
        let logical = position / BLOCK_SIZE as u64;
        let within = (position % BLOCK_SIZE as u64) as usize;
        let count = (input.len() - done).min(BLOCK_SIZE - within);
        let key = extent_key(object_id, logical);
        let existing = transaction.lookup(TreeKind::Extent, &key, &mut value)?;
        let old_extent = existing
            .map(|found| {
                ExtentValue::decode(
                    &value[..usize::from(found.length)],
                    transaction.volume_blocks(),
                )
            })
            .transpose()?;
        if within != 0 || count != BLOCK_SIZE {
            if let Some(extent) = old_extent {
                transaction.read_data(extent.physical, &mut block)?;
                verify_block(transaction, extent.physical, &block, &mut value)?;
            } else {
                block.fill(0);
            }
        }
        block[within..within + count].copy_from_slice(&input[done..done + count]);
        let physical = transaction.stage_data(block)?;
        let extent = ExtentValue {
            physical,
            mirror_physical: 0,
            blocks: 1,
            flags: 0,
            reliability: ReliabilityClass::Checksummed,
        }
        .encode(transaction.volume_blocks())?;
        transaction.upsert(TreeKind::Extent, &key, &extent)?;
        let mut digest = [0; 32];
        digest[..4].copy_from_slice(&crc32c(&block).to_le_bytes());
        let checksum = DataChecksumValue {
            blocks: 1,
            algorithm: ChecksumAlgorithm::Crc32c,
            digest_len: 4,
            digest,
        }
        .encode()?;
        transaction.insert(TreeKind::Checksum, &object_key(physical), &checksum)?;
        if let Some(extent) = old_extent {
            transaction.remove(TreeKind::Checksum, &object_key(extent.physical))?;
            transaction.defer_free_data(Extent {
                start: extent.physical,
                blocks: extent.blocks,
            })?;
        }
        if existing.is_none() {
            inode.allocated_blocks = inode
                .allocated_blocks
                .checked_add(1)
                .ok_or(Error::Capacity)?;
        }
        done += count;
    }
    inode.size = inode.size.max(end);
    inode.modified_ns = modified_ns;
    inode.content_generation = transaction.generation();
    inode.generation = transaction.generation();
    transaction.upsert(TreeKind::Inode, &object_key(object_id), &inode.encode()?)?;
    Ok(done)
}

pub fn resize<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    object_id: u64,
    new_size: u64,
    modified_ns: u64,
) -> Result<(), Error> {
    let mut inode = load_inode(transaction, object_id)?;
    if inode.kind != InodeKind::File {
        return Err(Error::InvalidArgument);
    }
    if new_size < inode.size {
        let keep_blocks = new_size.div_ceil(BLOCK_SIZE as u64);
        loop {
            let mut extent_key_to_remove = [0; 16];
            let mut extent_value = [0; MAX_VALUE_BYTES];
            let mut extent_value_len = 0usize;
            let mut found = false;
            transaction.for_each(TreeKind::Extent, |key, value| {
                if key.len() == 16
                    && key[..8] == object_key(object_id)
                    && u64::from_be_bytes(key[8..16].try_into().unwrap_or([0; 8])) >= keep_blocks
                {
                    extent_key_to_remove.copy_from_slice(key);
                    extent_value[..value.len()].copy_from_slice(value);
                    extent_value_len = value.len();
                    found = true;
                    return crate::tree::Visit::Stop;
                }
                crate::tree::Visit::Continue
            })?;
            if !found {
                break;
            }
            let extent = ExtentValue::decode(
                &extent_value[..extent_value_len],
                transaction.volume_blocks(),
            )?;
            transaction.remove(TreeKind::Extent, &extent_key_to_remove)?;
            transaction.remove(TreeKind::Checksum, &object_key(extent.physical))?;
            transaction.defer_free_data(Extent {
                start: extent.physical,
                blocks: extent.blocks,
            })?;
            inode.allocated_blocks = inode.allocated_blocks.saturating_sub(extent.blocks);
        }
        let tail = (new_size % BLOCK_SIZE as u64) as usize;
        if tail != 0 {
            let logical = new_size / BLOCK_SIZE as u64;
            let key = extent_key(object_id, logical);
            let mut value = [0; MAX_VALUE_BYTES];
            if let Some(found) = transaction.lookup(TreeKind::Extent, &key, &mut value)? {
                let extent = ExtentValue::decode(
                    &value[..usize::from(found.length)],
                    transaction.volume_blocks(),
                )?;
                let mut block = [0; BLOCK_SIZE];
                transaction.read_data(extent.physical, &mut block)?;
                block[tail..].fill(0);
                let physical = transaction.stage_data(block)?;
                let replacement = ExtentValue {
                    physical,
                    mirror_physical: 0,
                    blocks: 1,
                    flags: 0,
                    reliability: ReliabilityClass::Checksummed,
                }
                .encode(transaction.volume_blocks())?;
                transaction.upsert(TreeKind::Extent, &key, &replacement)?;
                let mut digest = [0; 32];
                digest[..4].copy_from_slice(&crc32c(&block).to_le_bytes());
                transaction.insert(
                    TreeKind::Checksum,
                    &object_key(physical),
                    &DataChecksumValue {
                        blocks: 1,
                        algorithm: ChecksumAlgorithm::Crc32c,
                        digest_len: 4,
                        digest,
                    }
                    .encode()?,
                )?;
                transaction.remove(TreeKind::Checksum, &object_key(extent.physical))?;
                transaction.defer_free_data(Extent {
                    start: extent.physical,
                    blocks: extent.blocks,
                })?;
            }
        }
    }
    inode.size = new_size;
    inode.modified_ns = modified_ns;
    inode.generation = transaction.generation();
    inode.content_generation = transaction.generation();
    transaction.upsert(TreeKind::Inode, &object_key(object_id), &inode.encode()?)?;
    Ok(())
}

fn load_inode<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    object_id: u64,
) -> Result<InodeValue, Error> {
    let mut value = [0; MAX_VALUE_BYTES];
    let found = transaction
        .lookup(TreeKind::Inode, &object_key(object_id), &mut value)?
        .ok_or(Error::InvalidArgument)?;
    InodeValue::decode(
        &value[..usize::from(found.length)],
        transaction.volume_blocks(),
    )
}

fn verify_block<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    physical: u64,
    block: &[u8; BLOCK_SIZE],
    value: &mut [u8; MAX_VALUE_BYTES],
) -> Result<(), Error> {
    let found = transaction
        .lookup(TreeKind::Checksum, &object_key(physical), value)?
        .ok_or(Error::InvalidChecksum)?;
    let checksum = DataChecksumValue::decode(&value[..usize::from(found.length)])?;
    if checksum.algorithm != ChecksumAlgorithm::Crc32c
        || checksum.blocks != 1
        || checksum.digest[..4] != crc32c(block).to_le_bytes()
    {
        return Err(Error::InvalidChecksum);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::{
        format::{format_empty, Superblock, FIRST_ALLOCATABLE_BLOCK},
        tree::{BlockDevice, TransactionWorkspace},
        MIN_VOLUME_BLOCKS,
    };
    use std::vec;

    const UUID: [u8; 16] = *b"VaraniaFileTest!";

    struct Disk(std::vec::Vec<[u8; BLOCK_SIZE]>);

    impl BlockDevice for Disk {
        fn read(&mut self, block: u64, output: &mut [u8; BLOCK_SIZE]) -> Result<(), Error> {
            *output = *self.0.get(block as usize).ok_or(Error::Io)?;
            Ok(())
        }

        fn write(&mut self, block: u64, input: &[u8; BLOCK_SIZE]) -> Result<(), Error> {
            *self.0.get_mut(block as usize).ok_or(Error::Io)? = *input;
            Ok(())
        }

        fn flush(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    fn disk() -> (Disk, Superblock) {
        let empty = format_empty(MIN_VOLUME_BLOCKS, UUID).unwrap();
        let mut blocks = vec![[0; BLOCK_SIZE]; MIN_VOLUME_BLOCKS as usize];
        blocks[0] = empty.superblock;
        blocks[1] = empty.superblock;
        for (index, root) in empty.roots.into_iter().enumerate() {
            let primary = FIRST_ALLOCATABLE_BLOCK as usize + index * 2;
            blocks[primary] = root;
            blocks[primary + 1] = root;
        }
        let mounted = Superblock::decode(&blocks[0], MIN_VOLUME_BLOCKS).unwrap();
        (Disk(blocks), mounted)
    }

    #[test]
    fn partial_and_multiblock_stream_roundtrip_with_sparse_zeroes() {
        let (mut disk, initial) = disk();
        let mut workspace = TransactionWorkspace::new();
        let mut create = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        let inode = InodeValue {
            generation: create.generation(),
            size: 16_000,
            allocated_blocks: 0,
            created_ns: 1,
            modified_ns: 1,
            content_generation: create.generation(),
            flags: 0,
            kind: InodeKind::File,
        }
        .encode()
        .unwrap();
        create
            .insert(TreeKind::Inode, &object_key(2), &inode)
            .unwrap();
        let mounted = create.commit().unwrap();

        let payload = vec![0x5a; 9000];
        let mut writer = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        assert_eq!(write_at(&mut writer, 2, 123, &payload, 99).unwrap(), 9000);
        let mounted = writer.commit().unwrap();

        let mut reader = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        let mut prefix = [1u8; 123];
        assert_eq!(read_at(&mut reader, 2, 0, &mut prefix).unwrap(), 123);
        assert!(prefix.iter().all(|byte| *byte == 0));
        let mut output = vec![0; payload.len()];
        assert_eq!(
            read_at(&mut reader, 2, 123, &mut output).unwrap(),
            payload.len()
        );
        assert_eq!(output, payload);
    }

    #[test]
    fn corrupted_data_is_never_returned_silently() {
        let (mut disk, initial) = disk();
        let mut workspace = TransactionWorkspace::new();
        let mut create = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        let inode = InodeValue {
            generation: create.generation(),
            size: 0,
            allocated_blocks: 0,
            created_ns: 1,
            modified_ns: 1,
            content_generation: create.generation(),
            flags: 0,
            kind: InodeKind::File,
        }
        .encode()
        .unwrap();
        create
            .insert(TreeKind::Inode, &object_key(2), &inode)
            .unwrap();
        let mounted = create.commit().unwrap();
        let mut writer = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        write_at(&mut writer, 2, 0, b"checksum", 2).unwrap();
        let mounted = writer.commit().unwrap();
        let mut extent_bytes = [0; MAX_VALUE_BYTES];
        let mut inspect = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        let found = inspect
            .lookup(TreeKind::Extent, &extent_key(2, 0), &mut extent_bytes)
            .unwrap()
            .unwrap();
        let extent = ExtentValue::decode(
            &extent_bytes[..usize::from(found.length)],
            MIN_VOLUME_BLOCKS,
        )
        .unwrap();
        drop(inspect);
        disk.0[extent.physical as usize][0] ^= 1;
        let mut reader = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        let mut output = [0; 8];
        assert_eq!(
            read_at(&mut reader, 2, 0, &mut output),
            Err(Error::InvalidChecksum)
        );
    }
}

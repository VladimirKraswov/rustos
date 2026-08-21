//! Независимые scrub и offline-fsck примитивы.
//!
//! Проверяющий код не использует VFS cache и не доверяет каталогам. Он идёт
//! от атомарно восстановленного RootSet, сверяет каждый separator B+tree,
//! обе metadata-копии и ссылки на inode. В repair-режиме исправляется только
//! одна заведомо повреждённая копия из второй валидной; логические ошибки
//! никогда не «угадываются».

use crate::{
    format::{
        crc32c, object_key, DataChecksumValue, DirectoryValue, Error, ExtentValue, NodeView,
        RootPointer, Superblock, TreeKind, MAX_TREE_HEIGHT,
    },
    tree::{BlockDevice, MAX_KEY_BYTES, MAX_VALUE_BYTES},
    BLOCK_SIZE,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrubReport {
    pub metadata_nodes: u64,
    pub metadata_items: u64,
    pub data_blocks: u64,
    pub primary_failures: u64,
    pub mirror_failures: u64,
    pub repaired_copies: u64,
    pub separator_errors: u64,
    pub dangling_references: u64,
    pub checksum_errors: u64,
    pub unrecoverable_nodes: u64,
}

impl ScrubReport {
    pub const fn is_clean(&self) -> bool {
        self.separator_errors == 0
            && self.dangling_references == 0
            && self.checksum_errors == 0
            && self.unrecoverable_nodes == 0
    }
}

#[derive(Clone, Copy)]
struct Frame {
    block: u64,
    next_child: u16,
    entered: bool,
    expected_key: [u8; MAX_KEY_BYTES],
    expected_len: u16,
}

impl Frame {
    const EMPTY: Self = Self {
        block: 0,
        next_child: 0,
        entered: false,
        expected_key: [0; MAX_KEY_BYTES],
        expected_len: 0,
    };

    fn new(block: u64, expected: &[u8]) -> Result<Self, Error> {
        if expected.len() > MAX_KEY_BYTES {
            return Err(Error::InvalidItem);
        }
        let mut frame = Self::EMPTY;
        frame.block = block;
        frame.expected_key[..expected.len()].copy_from_slice(expected);
        frame.expected_len = expected.len() as u16;
        Ok(frame)
    }

    fn expected(&self) -> &[u8] {
        &self.expected_key[..usize::from(self.expected_len)]
    }
}

pub fn scrub<D: BlockDevice>(
    device: &mut D,
    superblock: Superblock,
    repair: bool,
) -> Result<ScrubReport, Error> {
    let mut report = ScrubReport::default();
    for root in superblock.roots.iter() {
        walk_tree(device, superblock, root, repair, &mut report)?;
    }
    if repair && report.repaired_copies != 0 {
        device.flush()?;
    }
    Ok(report)
}

/// Offline fsck использует тот же строгий обход, но никогда не пишет на диск.
pub fn fsck<D: BlockDevice>(device: &mut D, superblock: Superblock) -> Result<ScrubReport, Error> {
    scrub(device, superblock, false)
}

fn walk_tree<D: BlockDevice>(
    device: &mut D,
    superblock: Superblock,
    root: RootPointer,
    repair: bool,
    report: &mut ScrubReport,
) -> Result<(), Error> {
    let mut stack = [Frame::EMPTY; MAX_TREE_HEIGHT as usize + 1];
    stack[0] = Frame::new(root.block, &[])?;
    let mut depth = 1usize;
    while depth != 0 {
        let frame_index = depth - 1;
        let frame = stack[frame_index];
        let Some(image) = read_metadata_pair(device, superblock, frame.block, repair, report)?
        else {
            depth -= 1;
            continue;
        };
        let node = NodeView::parse(
            &image,
            frame.block,
            superblock.uuid,
            superblock.volume_blocks,
        )?;
        if node.header().kind != root.kind {
            report.unrecoverable_nodes += 1;
            depth -= 1;
            continue;
        }
        if !frame.entered {
            stack[frame_index].entered = true;
            report.metadata_nodes += 1;
            report.metadata_items += u64::from(node.header().item_count);
            if !frame.expected().is_empty()
                && node.item(0).is_none_or(|item| item.key != frame.expected())
            {
                report.separator_errors += 1;
            }
            if node.header().level == 0 {
                verify_leaf(device, superblock, &node, report)?;
                depth -= 1;
                continue;
            }
            if node.header().item_count == 0 {
                report.separator_errors += 1;
                depth -= 1;
                continue;
            }
        }

        let next = usize::from(stack[frame_index].next_child);
        if next == usize::from(node.header().item_count) {
            depth -= 1;
            continue;
        }
        let item = node.item(next).ok_or(Error::InvalidNode)?;
        let child = decode_child(item.value)?;
        stack[frame_index].next_child += 1;
        if depth == stack.len() {
            report.separator_errors += 1;
            continue;
        }
        stack[depth] = Frame::new(child, item.key)?;
        depth += 1;
    }
    Ok(())
}

fn read_metadata_pair<D: BlockDevice>(
    device: &mut D,
    superblock: Superblock,
    primary: u64,
    repair: bool,
    report: &mut ScrubReport,
) -> Result<Option<[u8; BLOCK_SIZE]>, Error> {
    let mut first = [0; BLOCK_SIZE];
    let mut second = [0; BLOCK_SIZE];
    let first_valid = device.read(primary, &mut first).is_ok()
        && NodeView::parse(&first, primary, superblock.uuid, superblock.volume_blocks).is_ok();
    let second_valid = device.read(primary + 1, &mut second).is_ok()
        && NodeView::parse(&second, primary, superblock.uuid, superblock.volume_blocks).is_ok();
    match (first_valid, second_valid) {
        (true, true) if first == second => Ok(Some(first)),
        (true, true) => {
            // Две independently checksummed, но различные immutable копии —
            // это логическая неоднозначность, автоматически чинить её нельзя.
            report.unrecoverable_nodes += 1;
            Ok(None)
        }
        (true, false) => {
            report.mirror_failures += 1;
            if repair {
                device.write(primary + 1, &first)?;
                report.repaired_copies += 1;
            }
            Ok(Some(first))
        }
        (false, true) => {
            report.primary_failures += 1;
            if repair {
                device.write(primary, &second)?;
                report.repaired_copies += 1;
            }
            Ok(Some(second))
        }
        (false, false) => {
            report.primary_failures += 1;
            report.mirror_failures += 1;
            report.unrecoverable_nodes += 1;
            Ok(None)
        }
    }
}

fn verify_leaf<D: BlockDevice>(
    device: &mut D,
    superblock: Superblock,
    node: &NodeView<'_>,
    report: &mut ScrubReport,
) -> Result<(), Error> {
    for index in 0..usize::from(node.header().item_count) {
        let item = node.item(index).ok_or(Error::InvalidNode)?;
        match node.header().kind {
            TreeKind::Directory => {
                let directory = DirectoryValue::decode(item.value)?;
                if lookup_value(
                    device,
                    superblock,
                    superblock.roots.get(TreeKind::Inode),
                    &object_key(directory.object_id),
                    &mut [0; MAX_VALUE_BYTES],
                )?
                .is_none()
                {
                    report.dangling_references += 1;
                }
            }
            TreeKind::Extent => {
                let extent = ExtentValue::decode(item.value, superblock.volume_blocks)?;
                report.data_blocks = report.data_blocks.saturating_add(extent.blocks);
                verify_extent_data(device, superblock, extent, report)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn verify_extent_data<D: BlockDevice>(
    device: &mut D,
    superblock: Superblock,
    extent: ExtentValue,
    report: &mut ScrubReport,
) -> Result<(), Error> {
    for offset in 0..extent.blocks {
        let physical = extent.physical + offset;
        let mut data = [0; BLOCK_SIZE];
        if device.read(physical, &mut data).is_err() {
            report.checksum_errors += 1;
            continue;
        }
        let mut checksum_bytes = [0; MAX_VALUE_BYTES];
        let Some(length) = lookup_value(
            device,
            superblock,
            superblock.roots.get(TreeKind::Checksum),
            &object_key(physical),
            &mut checksum_bytes,
        )?
        else {
            report.checksum_errors += 1;
            continue;
        };
        let checksum = DataChecksumValue::decode(&checksum_bytes[..length])?;
        if checksum.blocks != 1
            || checksum.digest_len != 4
            || checksum.digest[..4] != crc32c(&data).to_le_bytes()
        {
            report.checksum_errors += 1;
        }
    }
    Ok(())
}

fn lookup_value<D: BlockDevice>(
    device: &mut D,
    superblock: Superblock,
    root: RootPointer,
    key: &[u8],
    output: &mut [u8],
) -> Result<Option<usize>, Error> {
    let mut number = root.block;
    loop {
        let mut report = ScrubReport::default();
        let image = read_metadata_pair(device, superblock, number, false, &mut report)?
            .ok_or(Error::InvalidNode)?;
        let node = NodeView::parse(&image, number, superblock.uuid, superblock.volume_blocks)?;
        if node.header().level == 0 {
            let index = lower_bound(&node, key);
            let Some(item) = node.item(index) else {
                return Ok(None);
            };
            if item.key != key {
                return Ok(None);
            }
            if item.value.len() > output.len() {
                return Err(Error::Capacity);
            }
            output[..item.value.len()].copy_from_slice(item.value);
            return Ok(Some(item.value.len()));
        }
        let lower = lower_bound(&node, key);
        let count = usize::from(node.header().item_count);
        if count == 0 {
            return Err(Error::InvalidNode);
        }
        let index = if lower == count {
            count - 1
        } else if node.item(lower).is_some_and(|item| item.key == key) || lower == 0 {
            lower
        } else {
            lower - 1
        };
        number = decode_child(node.item(index).ok_or(Error::InvalidNode)?.value)?;
    }
}

fn lower_bound(node: &NodeView<'_>, key: &[u8]) -> usize {
    let mut low = 0usize;
    let mut high = usize::from(node.header().item_count);
    while low < high {
        let middle = low + (high - low) / 2;
        if node.item(middle).is_some_and(|item| item.key < key) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn decode_child(value: &[u8]) -> Result<u64, Error> {
    let bytes: [u8; 8] = value.try_into().map_err(|_| Error::InvalidItem)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::{
        file,
        format::{format_empty, InodeKind, InodeValue, FIRST_ALLOCATABLE_BLOCK},
        tree::{Transaction, TransactionWorkspace},
        MIN_VOLUME_BLOCKS,
    };
    use std::vec;

    const UUID: [u8; 16] = *b"VaraniaScrubTest";

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

    fn formatted() -> (Disk, Superblock) {
        let empty = format_empty(MIN_VOLUME_BLOCKS, UUID).unwrap();
        let mut disk = Disk(vec![[0; BLOCK_SIZE]; MIN_VOLUME_BLOCKS as usize]);
        disk.0[0] = empty.superblock;
        disk.0[1] = empty.superblock;
        for (index, root) in empty.roots.into_iter().enumerate() {
            let primary = FIRST_ALLOCATABLE_BLOCK as usize + index * 2;
            disk.0[primary] = root;
            disk.0[primary + 1] = root;
        }
        let mounted = Superblock::decode(&disk.0[0], MIN_VOLUME_BLOCKS).unwrap();
        (disk, mounted)
    }

    #[test]
    fn repairs_one_bad_metadata_copy_and_is_clean_afterwards() {
        let (mut disk, mounted) = formatted();
        let root = mounted.roots.get(TreeKind::Inode).block as usize;
        disk.0[root][101] ^= 1;
        let report = scrub(&mut disk, mounted, true).unwrap();
        assert_eq!(report.primary_failures, 1);
        assert_eq!(report.repaired_copies, 1);
        assert!(report.is_clean());
        assert_eq!(fsck(&mut disk, mounted).unwrap().primary_failures, 0);
    }

    #[test]
    fn reports_data_corruption_without_repairing_user_bytes() {
        let (mut disk, initial) = formatted();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        let inode = InodeValue {
            generation: transaction.generation(),
            size: 0,
            allocated_blocks: 0,
            created_ns: 0,
            modified_ns: 0,
            content_generation: transaction.generation(),
            flags: 0,
            kind: InodeKind::File,
        }
        .encode()
        .unwrap();
        transaction
            .insert(TreeKind::Inode, &object_key(2), &inode)
            .unwrap();
        let mounted = transaction.commit().unwrap();
        let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        file::write_at(&mut transaction, 2, 0, b"scrub me", 1).unwrap();
        let mounted = transaction.commit().unwrap();
        let mut value = [0; MAX_VALUE_BYTES];
        let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        let found = transaction
            .lookup(
                TreeKind::Extent,
                &crate::format::extent_key(2, 0),
                &mut value,
            )
            .unwrap()
            .unwrap();
        let extent =
            ExtentValue::decode(&value[..usize::from(found.length)], MIN_VOLUME_BLOCKS).unwrap();
        drop(transaction);
        disk.0[extent.physical as usize][0] ^= 1;
        let report = scrub(&mut disk, mounted, true).unwrap();
        assert_eq!(report.checksum_errors, 1);
        assert!(!report.is_clean());
    }
}

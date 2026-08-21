//! Управление lightweight snapshots VaraniaFS.

use crate::{
    format::{object_key, Error, SnapshotValue, TreeKind},
    tree::{BlockDevice, Transaction, MAX_VALUE_BYTES},
};

pub fn create<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    snapshot_id: u64,
    created_ns: u64,
) -> Result<SnapshotValue, Error> {
    if snapshot_id == 0 {
        return Err(Error::InvalidArgument);
    }
    let value = SnapshotValue {
        sequence: transaction.generation(),
        created_ns,
        next_object_id: transaction.next_object_id(),
        roots: transaction.roots(),
    };
    let encoded = value.encode(transaction.volume_blocks())?;
    transaction.insert(TreeKind::Snapshot, &object_key(snapshot_id), &encoded)?;
    Ok(value)
}

pub fn get<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    snapshot_id: u64,
) -> Result<Option<SnapshotValue>, Error> {
    let mut bytes = [0; MAX_VALUE_BYTES];
    let Some(found) =
        transaction.lookup(TreeKind::Snapshot, &object_key(snapshot_id), &mut bytes)?
    else {
        return Ok(None);
    };
    SnapshotValue::decode(
        &bytes[..usize::from(found.length)],
        transaction.volume_blocks(),
    )
    .map(Some)
}

pub fn remove<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    snapshot_id: u64,
) -> Result<(), Error> {
    transaction.remove(TreeKind::Snapshot, &object_key(snapshot_id))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::{
        format::{format_empty, Superblock, FIRST_ALLOCATABLE_BLOCK},
        tree::{BlockDevice, TransactionWorkspace},
        BLOCK_SIZE, MIN_VOLUME_BLOCKS,
    };
    use std::vec;

    const UUID: [u8; 16] = *b"VaraniaSnapshot!";

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

    #[test]
    fn snapshot_manifest_survives_commit_and_can_be_removed() {
        let empty = format_empty(MIN_VOLUME_BLOCKS, UUID).unwrap();
        let mut disk = Disk(vec![[0; BLOCK_SIZE]; MIN_VOLUME_BLOCKS as usize]);
        disk.0[0] = empty.superblock;
        disk.0[1] = empty.superblock;
        for (index, root) in empty.roots.into_iter().enumerate() {
            let primary = FIRST_ALLOCATABLE_BLOCK as usize + index * 2;
            disk.0[primary] = root;
            disk.0[primary + 1] = root;
        }
        let initial = Superblock::decode(&disk.0[0], MIN_VOLUME_BLOCKS).unwrap();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        let created = create(&mut transaction, 7, 123).unwrap();
        assert_eq!(created.created_ns, 123);
        let mounted = transaction.commit().unwrap();
        let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        assert_eq!(get(&mut transaction, 7).unwrap(), Some(created));
        remove(&mut transaction, 7).unwrap();
        assert_eq!(get(&mut transaction, 7).unwrap(), None);
    }
}

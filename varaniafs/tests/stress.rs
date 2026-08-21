use std::collections::BTreeMap;
use varaniafs::{
    format::{
        format_empty, object_key, recover_latest, Block, Error, InodeKind, InodeValue, NodeView,
        Superblock, TreeKind, FIRST_ALLOCATABLE_BLOCK, ROOT_COUNT,
    },
    integrity,
    tree::{BlockDevice, Transaction, TransactionWorkspace, MAX_VALUE_BYTES},
    BLOCK_SIZE, MIN_VOLUME_BLOCKS,
};

const UUID: [u8; 16] = *b"VaraniaStressTst";

#[derive(Clone)]
struct MemoryDisk {
    durable: Vec<Block>,
    volatile: Vec<Block>,
    commands: usize,
    fail_before: Option<usize>,
}

impl MemoryDisk {
    fn formatted() -> (Self, Superblock) {
        let empty = format_empty(MIN_VOLUME_BLOCKS, UUID).unwrap();
        let mut durable = vec![[0; BLOCK_SIZE]; MIN_VOLUME_BLOCKS as usize];
        durable[0] = empty.superblock;
        durable[1] = empty.superblock;
        for (index, root) in empty.roots.into_iter().enumerate() {
            let primary = FIRST_ALLOCATABLE_BLOCK as usize + index * 2;
            durable[primary] = root;
            durable[primary + 1] = root;
        }
        let mounted = Superblock::decode(&durable[0], MIN_VOLUME_BLOCKS).unwrap();
        (
            Self {
                volatile: durable.clone(),
                durable,
                commands: 0,
                fail_before: None,
            },
            mounted,
        )
    }

    fn command(&mut self) -> Result<(), Error> {
        if self.fail_before == Some(self.commands) {
            return Err(Error::Io);
        }
        self.commands += 1;
        Ok(())
    }

    fn power_loss(&mut self) {
        self.volatile.clone_from(&self.durable);
        self.fail_before = None;
    }

    fn recover(&mut self) -> Superblock {
        recover_latest(MIN_VOLUME_BLOCKS, |number, output| {
            let Some(source) = self.durable.get(number as usize) else {
                return false;
            };
            *output = *source;
            true
        })
        .unwrap()
        .superblock
    }
}

impl BlockDevice for MemoryDisk {
    fn read(&mut self, block: u64, output: &mut Block) -> Result<(), Error> {
        *output = *self.volatile.get(block as usize).ok_or(Error::Io)?;
        Ok(())
    }

    fn write(&mut self, block: u64, input: &Block) -> Result<(), Error> {
        self.command()?;
        *self.volatile.get_mut(block as usize).ok_or(Error::Io)? = *input;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.command()?;
        self.durable.clone_from(&self.volatile);
        Ok(())
    }
}

fn inode_value(generation: u64, payload: u64) -> [u8; InodeValue::ENCODED_SIZE] {
    InodeValue {
        generation,
        size: payload,
        allocated_blocks: 0,
        created_ns: 0,
        modified_ns: 0,
        content_generation: generation,
        flags: 0,
        kind: InodeKind::File,
    }
    .encode()
    .unwrap()
}

fn reachable_metadata(disk: &MemoryDisk, mounted: Superblock) -> Vec<u64> {
    let mut pending = mounted
        .roots
        .iter()
        .map(|root| root.block)
        .collect::<Vec<_>>();
    let mut reachable = Vec::new();
    while let Some(primary) = pending.pop() {
        if reachable.contains(&primary) {
            continue;
        }
        let node = NodeView::parse(
            &disk.durable[primary as usize],
            primary,
            mounted.uuid,
            mounted.volume_blocks,
        )
        .unwrap();
        if node.header().level != 0 {
            for index in (0..usize::from(node.header().item_count)).rev() {
                let item = node.item(index).unwrap();
                let child = u64::from_le_bytes(item.value.try_into().unwrap());
                pending.push(child);
            }
        }
        reachable.push(primary);
    }
    reachable
}

#[test]
fn every_power_cut_keeps_complete_old_or_complete_new_generation() {
    let (base_disk, initial) = MemoryDisk::formatted();
    let mut successful_command_count = None;
    for cut in 0..160usize {
        let mut disk = base_disk.clone();
        let mut workspace = TransactionWorkspace::new();
        disk.fail_before = Some(cut);
        let result = {
            let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
            for object in 2..22u64 {
                transaction
                    .insert(
                        TreeKind::Inode,
                        &object_key(object),
                        &inode_value(transaction.generation(), object),
                    )
                    .unwrap();
            }
            transaction.commit()
        };
        if result.is_ok() {
            successful_command_count = Some(cut);
            break;
        }
        disk.power_loss();
        let recovered = disk.recover();
        assert!(
            recovered.sequence == initial.sequence || recovered.sequence == initial.sequence + 1
        );
        let mut transaction = Transaction::begin(&mut disk, recovered, &mut workspace).unwrap();
        let mut output = [0; MAX_VALUE_BYTES];
        let mut present = 0usize;
        for object in 2..22u64 {
            present += usize::from(
                transaction
                    .lookup(TreeKind::Inode, &object_key(object), &mut output)
                    .unwrap()
                    .is_some(),
            );
        }
        assert!(
            present == 0 || present == 20,
            "cut={cut}, present={present}"
        );
    }
    assert!(successful_command_count.is_some());
}

#[test]
fn randomized_insert_delete_workload_matches_reference_map() {
    let (mut disk, mut mounted) = MemoryDisk::formatted();
    let mut expected = BTreeMap::<u64, u64>::new();
    let mut state = 0x5eed_cafe_f00d_beefu64;
    let mut workspace = TransactionWorkspace::new();
    for batch in 0..50u64 {
        let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        for _ in 0..16 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let object = 2 + state % 300;
            if state & 3 == 0 && expected.remove(&object).is_some() {
                transaction
                    .remove(TreeKind::Inode, &object_key(object))
                    .unwrap();
            } else {
                let payload = state ^ batch;
                transaction
                    .upsert(
                        TreeKind::Inode,
                        &object_key(object),
                        &inode_value(transaction.generation(), payload),
                    )
                    .unwrap();
                expected.insert(object, payload);
            }
        }
        mounted = transaction.commit().unwrap();
    }
    let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
    let mut output = [0; MAX_VALUE_BYTES];
    for object in 2..302u64 {
        let found = transaction
            .lookup(TreeKind::Inode, &object_key(object), &mut output)
            .unwrap();
        match expected.get(&object) {
            Some(payload) => {
                assert!(found.is_some());
                assert_eq!(
                    u64::from_le_bytes(output[8..16].try_into().unwrap()),
                    *payload
                );
            }
            None => assert!(found.is_none()),
        }
    }
}

#[test]
fn repeated_single_copy_corruption_is_repaired_without_logical_loss() {
    let (mut disk, mut mounted) = MemoryDisk::formatted();
    let mut workspace = TransactionWorkspace::new();
    for batch in 0..20u64 {
        let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
        for offset in 0..20u64 {
            let object = 2 + batch * 20 + offset;
            transaction
                .insert(
                    TreeKind::Inode,
                    &object_key(object),
                    &inode_value(transaction.generation(), object),
                )
                .unwrap();
        }
        mounted = transaction.commit().unwrap();
    }
    disk.power_loss();
    // Повреждаем по одной копии каждого достижимого metadata node, включая
    // внутренние уровни деревьев после множества split. Чередование копий
    // проверяет оба направления восстановления, а не только happy path.
    let reachable = reachable_metadata(&disk, mounted);
    assert!(reachable.len() > ROOT_COUNT);
    for (index, primary) in reachable.iter().copied().enumerate() {
        let copy = primary + (index as u64 & 1);
        disk.volatile[copy as usize][333] ^= 0x80;
        disk.durable[copy as usize][333] ^= 0x80;
    }
    let report = integrity::scrub(&mut disk, mounted, true).unwrap();
    assert_eq!(report.repaired_copies as usize, reachable.len());
    assert!(report.is_clean());
    disk.flush().unwrap();
    let final_report = integrity::fsck(&mut disk, mounted).unwrap();
    assert!(final_report.is_clean());
    assert_eq!(
        final_report.primary_failures + final_report.mirror_failures,
        0
    );
}

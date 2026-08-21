//! Короткий intent log VaraniaFS.
//!
//! `fsync` не обязан ждать полного checkpoint superblock. После durable data и
//! metadata writer публикует компактное описание нового RootSet в кольцевом
//! журнале. Mount проверяет checksum записи и все корни; только после этого
//! запись может опередить superblock.

use crate::{
    format::{
        crc32c, Block, Error, Superblock, INTENT_LOG_BLOCKS, INTENT_LOG_START,
        SUPERBLOCK_HEADER_SIZE,
    },
    BLOCK_SIZE,
};

pub const INTENT_LOG_SLOTS: u64 = INTENT_LOG_BLOCKS / 2;
const MAGIC: [u8; 8] = *b"VINTENT\0";
const VERSION: u16 = 1;
const HEADER_SIZE: usize = 64;
const CHECKSUM_OFFSET: usize = BLOCK_SIZE - 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntentRecord {
    pub superblock: Superblock,
}

impl IntentRecord {
    pub const fn slot(sequence: u64) -> u64 {
        sequence % INTENT_LOG_SLOTS
    }

    pub const fn primary_block(sequence: u64) -> u64 {
        INTENT_LOG_START + Self::slot(sequence) * 2
    }

    pub fn encode(self) -> Result<Block, Error> {
        let encoded_superblock = self.superblock.encode()?;
        let mut block = [0; BLOCK_SIZE];
        block[..8].copy_from_slice(&MAGIC);
        block[8..10].copy_from_slice(&VERSION.to_le_bytes());
        block[10..12].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        block[16..24].copy_from_slice(&self.superblock.sequence.to_le_bytes());
        block[24..32].copy_from_slice(&(SUPERBLOCK_HEADER_SIZE as u64).to_le_bytes());
        block[HEADER_SIZE..HEADER_SIZE + SUPERBLOCK_HEADER_SIZE]
            .copy_from_slice(&encoded_superblock[..SUPERBLOCK_HEADER_SIZE]);
        let checksum = checksum_with_zero(&block);
        block[CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
        Ok(block)
    }

    pub fn decode(block: &Block, actual_blocks: u64) -> Result<Self, Error> {
        if block[..8] != MAGIC
            || u16::from_le_bytes(block[8..10].try_into().map_err(|_| Error::InvalidItem)?)
                != VERSION
            || u16::from_le_bytes(block[10..12].try_into().map_err(|_| Error::InvalidItem)?)
                as usize
                != HEADER_SIZE
            || block[12..16].iter().any(|byte| *byte != 0)
            || u64::from_le_bytes(block[24..32].try_into().map_err(|_| Error::InvalidItem)?)
                != SUPERBLOCK_HEADER_SIZE as u64
            || block[32..HEADER_SIZE].iter().any(|byte| *byte != 0)
            || block[HEADER_SIZE + SUPERBLOCK_HEADER_SIZE..CHECKSUM_OFFSET]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(Error::InvalidItem);
        }
        let stored = u32::from_le_bytes(
            block[CHECKSUM_OFFSET..]
                .try_into()
                .map_err(|_| Error::InvalidItem)?,
        );
        if stored != checksum_with_zero(block) {
            return Err(Error::InvalidChecksum);
        }
        let mut encoded_superblock = [0; BLOCK_SIZE];
        encoded_superblock[..SUPERBLOCK_HEADER_SIZE]
            .copy_from_slice(&block[HEADER_SIZE..HEADER_SIZE + SUPERBLOCK_HEADER_SIZE]);
        let superblock = Superblock::decode(&encoded_superblock, actual_blocks)?;
        let sequence =
            u64::from_le_bytes(block[16..24].try_into().map_err(|_| Error::InvalidItem)?);
        if sequence != superblock.sequence {
            return Err(Error::InvalidItem);
        }
        Ok(Self { superblock })
    }
}

fn checksum_with_zero(block: &Block) -> u32 {
    let mut checksum = crc32c(&block[..CHECKSUM_OFFSET]);
    // CRC32C всего блока с нулевым checksum. Последние четыре нулевых байта
    // нельзя просто опустить: они участвуют в стандартном chaining.
    for _ in 0..4 {
        checksum = crc32c_extend(checksum, 0);
    }
    checksum
}

fn crc32c_extend(previous: u32, byte: u8) -> u32 {
    let mut crc = !previous ^ u32::from(byte);
    for _ in 0..8 {
        crc = if crc & 1 != 0 {
            (crc >> 1) ^ 0x82f6_3b78
        } else {
            crc >> 1
        };
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{format_empty, Superblock};
    use crate::MIN_VOLUME_BLOCKS;

    const UUID: [u8; 16] = *b"VaraniaIntentLog";

    #[test]
    fn record_roundtrip_and_torn_copy_rejection() {
        let empty = format_empty(MIN_VOLUME_BLOCKS, UUID).unwrap();
        let superblock = Superblock::decode(&empty.superblock, MIN_VOLUME_BLOCKS).unwrap();
        let record = IntentRecord { superblock };
        let mut encoded = record.encode().unwrap();
        assert_eq!(
            IntentRecord::decode(&encoded, MIN_VOLUME_BLOCKS),
            Ok(record)
        );
        encoded[HEADER_SIZE + 17] ^= 1;
        assert_eq!(
            IntentRecord::decode(&encoded, MIN_VOLUME_BLOCKS),
            Err(Error::InvalidChecksum)
        );
    }

    #[test]
    fn ring_uses_mirrored_pairs_without_touching_tree_area() {
        for sequence in 1..1000 {
            let primary = IntentRecord::primary_block(sequence);
            assert!(primary >= INTENT_LOG_START);
            assert!(primary + 1 < INTENT_LOG_START + INTENT_LOG_BLOCKS);
            assert_eq!(primary & 1, 0);
        }
    }
}

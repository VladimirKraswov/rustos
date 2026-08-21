//! Дисковый формат VaraniaFS v1.
//!
//! Формат намеренно отделён и от `vfsd`, и от block driver. Один и тот же
//! код проверяет образ на host, монтирует его в RustOS и в дальнейшем станет
//! основой утилиты обмена файлами для macOS/Linux.
//!
//! Метаданные имеют две полные копии. Сначала filesystem server записывает
//! неактивную копию и выполняет flush, затем одним 4-KiB блоком публикует
//! новый superblock. После потери питания mount выбирает валидную копию с
//! наибольшим `sequence`. Все размеры и номера блоков 64-битные.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

/// Масштабируемый формат следующего поколения. V1 остаётся доступен во время
/// миграции: образ нельзя молча интерпретировать другой версией parser'а.
pub mod v2;

/// Размер логического блока. Он совпадает со страницей RustOS и естественным
/// erase/program granularity современных SSD, но остаётся удобным для HDD.
pub const BLOCK_SIZE: usize = 4096;
pub const SUPERBLOCK_COPIES: u64 = 2;
pub const METADATA_BLOCKS: u32 = 16;
pub const METADATA_SLOTS: u32 = 2;
pub const DATA_START_BLOCK: u64 = SUPERBLOCK_COPIES + METADATA_BLOCKS as u64 * 2;
pub const MIN_VOLUME_BLOCKS: u64 = 4096;
pub const MAX_INODES: usize = 64;
pub const MAX_EXTENTS_PER_INODE: usize = 32;
pub const MAX_FREE_EXTENTS: usize = 64;
pub const MAX_PATH_BYTES: usize = 192;
pub const MAGIC: [u8; 8] = *b"VARNFS1\0";
pub const VERSION: u32 = 1;

pub mod kind {
    pub const FILE: u8 = 1;
    pub const DIRECTORY: u8 = 2;
}

/// Непрерывный диапазон блоков файла. `logical` позволяет иметь sparse-файлы,
/// а 64-битные `physical`/`blocks` не ограничивают том несколькими TiB.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileExtent {
    pub logical: u64,
    pub physical: u64,
    pub blocks: u64,
}

impl FileExtent {
    pub const EMPTY: Self = Self {
        logical: 0,
        physical: 0,
        blocks: 0,
    };
}

/// Свободный физический диапазон. На текущем этапе используется bounded
/// extent table; дерево свободного пространства можно добавить без смены VFS
/// protocol, выпустив disk-format v2.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeExtent {
    pub start: u64,
    pub blocks: u64,
}

impl FreeExtent {
    pub const EMPTY: Self = Self {
        start: 0,
        blocks: 0,
    };
}

/// Inode хранит нормализованный абсолютный путь. Это проще B-tree для
/// учебного v1, но rename каталога всё равно транзакционен: меняются все
/// дочерние inode в одной metadata commit.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Inode {
    pub generation: u64,
    pub size: u64,
    pub flags: u32,
    pub path_len: u16,
    pub extent_count: u16,
    pub used: u8,
    pub kind: u8,
    pub reserved: [u8; 6],
    pub path: [u8; MAX_PATH_BYTES],
    pub extents: [FileExtent; MAX_EXTENTS_PER_INODE],
}

impl Inode {
    pub const EMPTY: Self = Self {
        generation: 0,
        size: 0,
        flags: 0,
        path_len: 0,
        extent_count: 0,
        used: 0,
        kind: 0,
        reserved: [0; 6],
        path: [0; MAX_PATH_BYTES],
        extents: [FileExtent::EMPTY; MAX_EXTENTS_PER_INODE],
    };

    pub fn path(&self) -> &[u8] {
        &self.path[..usize::from(self.path_len).min(MAX_PATH_BYTES)]
    }
}

/// Полная metadata snapshot. Размер ровно 16 блоков: сервер может писать её
/// последовательно одним небольшим буфером и не требует heap allocator.
#[repr(C)]
pub struct Metadata {
    pub sequence: u64,
    pub next_inode_generation: u64,
    pub next_data_block: u64,
    pub free_extent_count: u16,
    pub inode_count: u16,
    pub reserved0: [u8; 36],
    pub inodes: [Inode; MAX_INODES],
    pub free_extents: [FreeExtent; MAX_FREE_EXTENTS],
    pub reserved1: [u8; 960],
}

impl Metadata {
    pub const fn empty() -> Self {
        Self {
            sequence: 1,
            next_inode_generation: 1,
            next_data_block: DATA_START_BLOCK,
            free_extent_count: 0,
            inode_count: 0,
            reserved0: [0; 36],
            inodes: [Inode::EMPTY; MAX_INODES],
            free_extents: [FreeExtent::EMPTY; MAX_FREE_EXTENTS],
            reserved1: [0; 960],
        }
    }

    /// Полное детерминированное представление metadata для записи и CRC.
    pub fn bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }

    /// Заполняет структуру байтами с диска. Вызывающий предварительно
    /// проверяет размер и CRC всей snapshot.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                (self as *mut Self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }
}

/// Один атомарно записываемый 4-KiB superblock.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Superblock {
    pub magic: [u8; 8],
    pub version: u32,
    pub block_size: u32,
    pub sequence: u64,
    pub volume_blocks: u64,
    pub active_slot: u32,
    pub metadata_blocks: u32,
    pub metadata_crc32: u32,
    pub flags: u32,
    pub uuid: [u8; 16],
    pub reserved: [u8; 4032],
}

impl Superblock {
    pub const fn new(
        volume_blocks: u64,
        sequence: u64,
        active_slot: u32,
        metadata_crc32: u32,
    ) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            block_size: BLOCK_SIZE as u32,
            sequence,
            volume_blocks,
            active_slot,
            metadata_blocks: METADATA_BLOCKS,
            metadata_crc32,
            flags: 0,
            uuid: *b"RustOS-VaraniaFS",
            reserved: [0; 4032],
        }
    }

    pub fn validate(&self, actual_blocks: u64) -> bool {
        self.magic == MAGIC
            && self.version == VERSION
            && self.block_size == BLOCK_SIZE as u32
            // Образ можно безопасно расширить до публикации нового
            // superblock: старая копия продолжает описывать валидный префикс.
            // Уменьшение запрещено, потому что могло бы обрезать extent'ы.
            && self.volume_blocks <= actual_blocks
            && self.volume_blocks >= MIN_VOLUME_BLOCKS
            && self.active_slot < METADATA_SLOTS
            && self.metadata_blocks == METADATA_BLOCKS
            && self.flags == 0
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                (self as *mut Self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }
}

pub const fn metadata_slot_start(slot: u32) -> u64 {
    SUPERBLOCK_COPIES + slot as u64 * METADATA_BLOCKS as u64
}

/// CRC-32/ISO-HDLC. Нужен только для обнаружения torn/corrupt metadata;
/// криптографические свойства здесь не требуются.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

const _: () = assert!(core::mem::size_of::<FileExtent>() == 24);
const _: () = assert!(core::mem::size_of::<Inode>() == 992);
const _: () = assert!(core::mem::size_of::<Metadata>() == BLOCK_SIZE * METADATA_BLOCKS as usize);
const _: () = assert!(core::mem::size_of::<Superblock>() == BLOCK_SIZE);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_layout_and_crc_are_stable() {
        let metadata = Metadata::empty();
        assert_eq!(metadata.bytes().len(), 65_536);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let superblock = Superblock::new(16_384, 1, 0, crc32(metadata.bytes()));
        assert!(superblock.validate(16_384));
    }
}

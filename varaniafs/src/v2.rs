//! Масштабируемые on-disk структуры VaraniaFS v2.
//!
//! V2 не хранит полный bounded metadata snapshot. Шесть copy-on-write деревьев
//! индексируют inode, каталоги, extents, checksums, пространство и snapshots. Commit
//! сначала записывает новые immutable nodes, выполняет device flush и только
//! затем публикует один checksummed superblock. Предыдущая копия superblock и
//! все достижимые из неё nodes остаются recovery point до reclamation.
//!
//! Этот модуль описывает дисковую границу, parser и recovery выбора корня. Он
//! не содержит VFS policy, block driver и allocator cache. Код намеренно не
//! использует `unsafe` и декодирует little-endian поля явно: повреждённый диск
//! никогда не превращается в Rust struct через pointer cast.

use core::{cmp::Ordering, ops::Range};

use crate::{BLOCK_SIZE, MIN_VOLUME_BLOCKS};

/// Один физический блок формата v2.
pub type Block = [u8; BLOCK_SIZE];

pub const MAGIC: [u8; 8] = *b"VARNFS2\0";
pub const VERSION: u32 = 2;
pub const SUPERBLOCK_COPIES: u64 = 2;
pub const FIRST_ALLOCATABLE_BLOCK: u64 = SUPERBLOCK_COPIES;
pub const ROOT_COUNT: usize = 6;
pub const SUPERBLOCK_HEADER_SIZE: usize = 256;
pub const NODE_HEADER_SIZE: usize = 80;
pub const SLOT_SIZE: usize = 8;
pub const MAX_NODE_ITEMS: usize = (BLOCK_SIZE - NODE_HEADER_SIZE) / SLOT_SIZE;
pub const MAX_TREE_HEIGHT: u16 = 16;
pub const MAX_NAME_BYTES: usize = 255;
pub const ROOT_OBJECT_ID: u64 = 1;

const SUPERBLOCK_CHECKSUM_OFFSET: usize = 240;
const NODE_CHECKSUM_OFFSET: usize = 72;
const NODE_MAGIC: [u8; 4] = *b"VNOD";
const NODE_VERSION: u16 = 2;
const CHECKSUM_CRC32C: u16 = 1;
const INCOMPAT_COW_BTREE: u64 = 1;
const KNOWN_INCOMPAT_FEATURES: u64 = INCOMPAT_COW_BTREE;
const EXTENT_FLAG_REPLICATED: u32 = 1;
const KNOWN_EXTENT_FLAGS: u32 = EXTENT_FLAG_REPLICATED;

/// Ошибка проверки дискового формата. Ошибки не содержат host strings, чтобы
/// тот же parser можно было использовать в `no_std` filesystem service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Io,
    InvalidArgument,
    InvalidSuperblock,
    UnsupportedVersion,
    UnsupportedFeatures,
    InvalidChecksum,
    InvalidRoot,
    InvalidNode,
    InvalidItem,
    UnorderedKeys,
    Capacity,
}

/// Назначение отдельного B+tree.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeKind {
    Inode = 1,
    Directory = 2,
    Extent = 3,
    Space = 4,
    Checksum = 5,
    Snapshot = 6,
}

impl TreeKind {
    fn from_disk(value: u16) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::Inode),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Extent),
            4 => Ok(Self::Space),
            5 => Ok(Self::Checksum),
            6 => Ok(Self::Snapshot),
            _ => Err(Error::InvalidNode),
        }
    }

    const fn index(self) -> usize {
        self as usize - 1
    }
}

/// Корень immutable поколения дерева.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootPointer {
    pub block: u64,
    pub generation: u64,
    pub level: u16,
    pub kind: TreeKind,
}

impl RootPointer {
    pub const fn new(block: u64, generation: u64, level: u16, kind: TreeKind) -> Self {
        Self {
            block,
            generation,
            level,
            kind,
        }
    }
}

/// Все корни одного атомарно опубликованного поколения.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootSet {
    roots: [RootPointer; ROOT_COUNT],
}

impl RootSet {
    pub const fn new(
        inode: RootPointer,
        directory: RootPointer,
        extent: RootPointer,
        space: RootPointer,
        checksum: RootPointer,
        snapshot: RootPointer,
    ) -> Self {
        Self {
            roots: [inode, directory, extent, space, checksum, snapshot],
        }
    }

    pub const fn get(&self, kind: TreeKind) -> RootPointer {
        self.roots[kind.index()]
    }

    pub fn iter(&self) -> impl Iterator<Item = RootPointer> + '_ {
        self.roots.iter().copied()
    }

    fn validate(&self, volume_blocks: u64, sequence: u64) -> bool {
        for (index, root) in self.roots.iter().copied().enumerate() {
            if root.kind.index() != index
                || root.block < FIRST_ALLOCATABLE_BLOCK
                || root.block >= volume_blocks
                || root.generation == 0
                || root.generation > sequence
                || root.level > MAX_TREE_HEIGHT
            {
                return false;
            }
            if self.roots[..index]
                .iter()
                .any(|previous| previous.block == root.block)
            {
                return false;
            }
        }
        true
    }
}

/// Логическое представление superblock. `encode` всегда создаёт полностью
/// детерминированный 4-KiB блок: unused bytes нулевые и входят в checksum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Superblock {
    pub sequence: u64,
    pub volume_blocks: u64,
    pub next_object_id: u64,
    pub allocated_blocks: u64,
    pub uuid: [u8; 16],
    pub roots: RootSet,
    pub compatible_features: u64,
    pub read_only_features: u64,
    pub incompatible_features: u64,
}

impl Superblock {
    pub const fn new(
        volume_blocks: u64,
        sequence: u64,
        next_object_id: u64,
        allocated_blocks: u64,
        uuid: [u8; 16],
        roots: RootSet,
    ) -> Self {
        Self {
            sequence,
            volume_blocks,
            next_object_id,
            allocated_blocks,
            uuid,
            roots,
            compatible_features: 0,
            read_only_features: 0,
            incompatible_features: INCOMPAT_COW_BTREE,
        }
    }

    pub fn encode(&self) -> Result<Block, Error> {
        if !self.validate_fields(self.volume_blocks) {
            return Err(Error::InvalidArgument);
        }
        let mut block = [0u8; BLOCK_SIZE];
        block[0..8].copy_from_slice(&MAGIC);
        put_u32(&mut block, 8, VERSION);
        put_u32(&mut block, 12, BLOCK_SIZE as u32);
        put_u16(&mut block, 16, SUPERBLOCK_HEADER_SIZE as u16);
        put_u16(&mut block, 18, ROOT_COUNT as u16);
        put_u16(&mut block, 20, CHECKSUM_CRC32C);
        put_u64(&mut block, 24, self.sequence);
        put_u64(&mut block, 32, self.volume_blocks);
        put_u64(&mut block, 40, self.next_object_id);
        put_u64(&mut block, 48, self.allocated_blocks);
        block[56..72].copy_from_slice(&self.uuid);
        for (index, root) in self.roots.iter().enumerate() {
            let offset = 72 + index * 24;
            put_u64(&mut block, offset, root.block);
            put_u64(&mut block, offset + 8, root.generation);
            put_u16(&mut block, offset + 16, root.level);
            put_u16(&mut block, offset + 18, root.kind as u16);
        }
        put_u64(&mut block, 216, self.compatible_features);
        put_u64(&mut block, 224, self.read_only_features);
        put_u64(&mut block, 232, self.incompatible_features);
        let checksum = crc32c_with_zeroed_u32(&block, SUPERBLOCK_CHECKSUM_OFFSET);
        put_u32(&mut block, SUPERBLOCK_CHECKSUM_OFFSET, checksum);
        Ok(block)
    }

    pub fn decode(block: &Block, actual_blocks: u64) -> Result<Self, Error> {
        if block[0..8] != MAGIC {
            return Err(Error::InvalidSuperblock);
        }
        if get_u32(block, 8) != VERSION {
            return Err(Error::UnsupportedVersion);
        }
        if get_u32(block, 12) != BLOCK_SIZE as u32
            || get_u16(block, 16) as usize != SUPERBLOCK_HEADER_SIZE
            || get_u16(block, 18) as usize != ROOT_COUNT
            || get_u16(block, 20) != CHECKSUM_CRC32C
            || get_u16(block, 22) != 0
        {
            return Err(Error::InvalidSuperblock);
        }
        let stored_checksum = get_u32(block, SUPERBLOCK_CHECKSUM_OFFSET);
        if stored_checksum != crc32c_with_zeroed_u32(block, SUPERBLOCK_CHECKSUM_OFFSET) {
            return Err(Error::InvalidChecksum);
        }
        if block[SUPERBLOCK_CHECKSUM_OFFSET + 4..]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(Error::InvalidSuperblock);
        }

        let mut roots = [RootPointer::new(0, 0, 0, TreeKind::Inode); ROOT_COUNT];
        for (index, root) in roots.iter_mut().enumerate() {
            let offset = 72 + index * 24;
            if get_u32(block, offset + 20) != 0 {
                return Err(Error::InvalidRoot);
            }
            *root = RootPointer {
                block: get_u64(block, offset),
                generation: get_u64(block, offset + 8),
                level: get_u16(block, offset + 16),
                kind: TreeKind::from_disk(get_u16(block, offset + 18))?,
            };
        }
        let decoded = Self {
            sequence: get_u64(block, 24),
            volume_blocks: get_u64(block, 32),
            next_object_id: get_u64(block, 40),
            allocated_blocks: get_u64(block, 48),
            uuid: block[56..72]
                .try_into()
                .map_err(|_| Error::InvalidSuperblock)?,
            roots: RootSet { roots },
            compatible_features: get_u64(block, 216),
            read_only_features: get_u64(block, 224),
            incompatible_features: get_u64(block, 232),
        };
        if decoded.incompatible_features & !KNOWN_INCOMPAT_FEATURES != 0 {
            return Err(Error::UnsupportedFeatures);
        }
        if decoded.read_only_features != 0 {
            return Err(Error::UnsupportedFeatures);
        }
        if !decoded.validate_fields(actual_blocks) {
            return Err(Error::InvalidSuperblock);
        }
        Ok(decoded)
    }

    fn validate_fields(&self, actual_blocks: u64) -> bool {
        self.sequence != 0
            && self.volume_blocks >= MIN_VOLUME_BLOCKS
            && self.volume_blocks <= actual_blocks
            && self.next_object_id > ROOT_OBJECT_ID
            && self.uuid.iter().any(|byte| *byte != 0)
            && self.allocated_blocks >= SUPERBLOCK_COPIES + ROOT_COUNT as u64
            && self.allocated_blocks <= self.volume_blocks
            && self.incompatible_features & INCOMPAT_COW_BTREE != 0
            && self.incompatible_features & !KNOWN_INCOMPAT_FEATURES == 0
            && self.roots.validate(self.volume_blocks, self.sequence)
    }
}

/// Заголовок одного checksummed B+tree node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeHeader {
    pub kind: TreeKind,
    pub level: u16,
    pub item_count: u16,
    pub generation: u64,
    pub self_block: u64,
    pub content_start: u16,
    pub uuid: [u8; 16],
}

/// Zero-copy проверенное представление node.
pub struct NodeView<'a> {
    block: &'a Block,
    header: NodeHeader,
}

impl<'a> NodeView<'a> {
    pub fn parse(
        block: &'a Block,
        expected_block: u64,
        expected_uuid: [u8; 16],
        volume_blocks: u64,
    ) -> Result<Self, Error> {
        if block[0..4] != NODE_MAGIC
            || get_u16(block, 4) != NODE_VERSION
            || get_u16(block, 6) as usize != NODE_HEADER_SIZE
            || get_u16(block, 14) != 0
            || block[52..56].iter().any(|byte| *byte != 0)
            || block[76..80].iter().any(|byte| *byte != 0)
        {
            return Err(Error::InvalidNode);
        }
        if get_u32(block, NODE_CHECKSUM_OFFSET)
            != crc32c_with_zeroed_u32(block, NODE_CHECKSUM_OFFSET)
        {
            return Err(Error::InvalidChecksum);
        }
        let header = NodeHeader {
            kind: TreeKind::from_disk(get_u16(block, 8))?,
            level: get_u16(block, 10),
            item_count: get_u16(block, 12),
            generation: get_u64(block, 16),
            self_block: get_u64(block, 24),
            content_start: get_u16(block, 50),
            uuid: block[56..72].try_into().map_err(|_| Error::InvalidNode)?,
        };
        let slot_end = NODE_HEADER_SIZE
            .checked_add(usize::from(header.item_count) * SLOT_SIZE)
            .ok_or(Error::InvalidNode)?;
        if header.level > MAX_TREE_HEIGHT
            || usize::from(header.item_count) > MAX_NODE_ITEMS
            || header.generation == 0
            || header.uuid != expected_uuid
            || header.self_block != expected_block
            || header.self_block < FIRST_ALLOCATABLE_BLOCK
            || header.self_block >= volume_blocks
            || get_u64(block, 32) != 0
            || get_u64(block, 40) != 0
            || usize::from(get_u16(block, 48)) != slot_end
            || slot_end > usize::from(header.content_start)
            || usize::from(header.content_start) > BLOCK_SIZE
            || block[slot_end..usize::from(header.content_start)]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(Error::InvalidNode);
        }

        let view = Self { block, header };
        view.validate_items(volume_blocks)?;
        Ok(view)
    }

    pub const fn header(&self) -> NodeHeader {
        self.header
    }

    pub fn item(&self, index: usize) -> Option<NodeItem<'a>> {
        if index >= usize::from(self.header.item_count) {
            return None;
        }
        let slot = NODE_HEADER_SIZE + index * SLOT_SIZE;
        let key = range_from_slot(self.block, slot)?;
        let value = range_from_slot(self.block, slot + 4)?;
        Some(NodeItem {
            key: &self.block[key],
            value: &self.block[value],
        })
    }

    fn validate_items(&self, volume_blocks: u64) -> Result<(), Error> {
        let count = usize::from(self.header.item_count);
        let content_start = usize::from(self.header.content_start);
        let mut previous_key: Option<&[u8]> = None;
        for index in 0..count {
            let item = self.item(index).ok_or(Error::InvalidItem)?;
            if item.key.is_empty() || !leaf_shape_is_valid(self.header, item, volume_blocks) {
                return Err(Error::InvalidItem);
            }
            if previous_key.is_some_and(|previous| previous.cmp(item.key) != Ordering::Less) {
                return Err(Error::UnorderedKeys);
            }
            previous_key = Some(item.key);

            let slot = NODE_HEADER_SIZE + index * SLOT_SIZE;
            let key_range = range_from_slot(self.block, slot).ok_or(Error::InvalidItem)?;
            let value_range = range_from_slot(self.block, slot + 4).ok_or(Error::InvalidItem)?;
            if key_range.start < content_start
                || value_range.start < content_start
                || ranges_overlap(&key_range, &value_range)
            {
                return Err(Error::InvalidItem);
            }
            for previous in 0..index {
                let previous_slot = NODE_HEADER_SIZE + previous * SLOT_SIZE;
                let previous_key_range =
                    range_from_slot(self.block, previous_slot).ok_or(Error::InvalidItem)?;
                let previous_value_range =
                    range_from_slot(self.block, previous_slot + 4).ok_or(Error::InvalidItem)?;
                if ranges_overlap(&key_range, &previous_key_range)
                    || ranges_overlap(&key_range, &previous_value_range)
                    || ranges_overlap(&value_range, &previous_key_range)
                    || ranges_overlap(&value_range, &previous_value_range)
                {
                    return Err(Error::InvalidItem);
                }
            }
        }
        Ok(())
    }
}

/// Одна key/value запись проверенного node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeItem<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
}

/// Bounded builder одного immutable node. Items добавляются в строгом порядке;
/// при переполнении block не считается опубликованным и может быть отброшен.
pub struct NodeBuilder {
    block: Block,
    header: NodeHeader,
    content_start: usize,
    finalized: bool,
}

impl NodeBuilder {
    pub fn new(
        kind: TreeKind,
        level: u16,
        generation: u64,
        self_block: u64,
        uuid: [u8; 16],
        volume_blocks: u64,
    ) -> Result<Self, Error> {
        if level > MAX_TREE_HEIGHT
            || generation == 0
            || !uuid.iter().any(|byte| *byte != 0)
            || self_block < FIRST_ALLOCATABLE_BLOCK
            || self_block >= volume_blocks
        {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            block: [0u8; BLOCK_SIZE],
            header: NodeHeader {
                kind,
                level,
                item_count: 0,
                generation,
                self_block,
                content_start: BLOCK_SIZE as u16,
                uuid,
            },
            content_start: BLOCK_SIZE,
            finalized: false,
        })
    }

    pub fn push(&mut self, key: &[u8], value: &[u8], volume_blocks: u64) -> Result<(), Error> {
        if self.finalized
            || key.is_empty()
            || key.len() > u16::MAX as usize
            || value.len() > u16::MAX as usize
        {
            return Err(Error::InvalidArgument);
        }
        let candidate = NodeItem { key, value };
        if !leaf_shape_is_valid(self.header, candidate, volume_blocks) {
            return Err(Error::InvalidItem);
        }
        if self.header.item_count != 0 {
            let previous = self
                .item(usize::from(self.header.item_count) - 1)
                .ok_or(Error::InvalidNode)?;
            if previous.key.cmp(key) != Ordering::Less {
                return Err(Error::UnorderedKeys);
            }
        }
        let item_bytes = key.len().checked_add(value.len()).ok_or(Error::Capacity)?;
        let new_content = self
            .content_start
            .checked_sub(item_bytes)
            .ok_or(Error::Capacity)?;
        let new_slot_end = NODE_HEADER_SIZE
            .checked_add((usize::from(self.header.item_count) + 1) * SLOT_SIZE)
            .ok_or(Error::Capacity)?;
        if new_slot_end > new_content {
            return Err(Error::Capacity);
        }

        let key_start = new_content;
        let value_start = key_start + key.len();
        self.block[key_start..value_start].copy_from_slice(key);
        self.block[value_start..self.content_start].copy_from_slice(value);
        let slot = NODE_HEADER_SIZE + usize::from(self.header.item_count) * SLOT_SIZE;
        put_u16(&mut self.block, slot, key_start as u16);
        put_u16(&mut self.block, slot + 2, key.len() as u16);
        put_u16(&mut self.block, slot + 4, value_start as u16);
        put_u16(&mut self.block, slot + 6, value.len() as u16);
        self.content_start = new_content;
        self.header.content_start = new_content as u16;
        self.header.item_count += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<Block, Error> {
        self.write_header();
        self.finalized = true;
        let checksum = crc32c_with_zeroed_u32(&self.block, NODE_CHECKSUM_OFFSET);
        put_u32(&mut self.block, NODE_CHECKSUM_OFFSET, checksum);
        Ok(self.block)
    }

    fn item(&self, index: usize) -> Option<NodeItem<'_>> {
        if index >= usize::from(self.header.item_count) {
            return None;
        }
        let slot = NODE_HEADER_SIZE + index * SLOT_SIZE;
        let key = range_from_slot(&self.block, slot)?;
        let value = range_from_slot(&self.block, slot + 4)?;
        Some(NodeItem {
            key: &self.block[key],
            value: &self.block[value],
        })
    }

    fn write_header(&mut self) {
        self.block[0..4].copy_from_slice(&NODE_MAGIC);
        put_u16(&mut self.block, 4, NODE_VERSION);
        put_u16(&mut self.block, 6, NODE_HEADER_SIZE as u16);
        put_u16(&mut self.block, 8, self.header.kind as u16);
        put_u16(&mut self.block, 10, self.header.level);
        put_u16(&mut self.block, 12, self.header.item_count);
        put_u64(&mut self.block, 16, self.header.generation);
        put_u64(&mut self.block, 24, self.header.self_block);
        put_u16(
            &mut self.block,
            48,
            NODE_HEADER_SIZE as u16 + self.header.item_count * SLOT_SIZE as u16,
        );
        put_u16(&mut self.block, 50, self.header.content_start);
        self.block[56..72].copy_from_slice(&self.header.uuid);
    }
}

/// Тип inode без Unix permissions. Capability policy находится над filesystem.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeKind {
    File = 1,
    Directory = 2,
    SymbolicLink = 3,
}

/// Фиксированное значение inode tree. Ключ — big-endian object id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeValue {
    pub generation: u64,
    pub size: u64,
    pub allocated_blocks: u64,
    pub created_ns: u64,
    pub modified_ns: u64,
    pub content_generation: u64,
    pub flags: u32,
    pub kind: InodeKind,
}

impl InodeValue {
    pub const ENCODED_SIZE: usize = 64;

    pub fn encode(self) -> Result<[u8; Self::ENCODED_SIZE], Error> {
        if self.generation == 0 || self.content_generation == 0 || self.flags != 0 {
            return Err(Error::InvalidArgument);
        }
        let mut bytes = [0u8; Self::ENCODED_SIZE];
        put_u64(&mut bytes, 0, self.generation);
        put_u64(&mut bytes, 8, self.size);
        put_u64(&mut bytes, 16, self.allocated_blocks);
        put_u64(&mut bytes, 24, self.created_ns);
        put_u64(&mut bytes, 32, self.modified_ns);
        put_u64(&mut bytes, 40, self.content_generation);
        put_u32(&mut bytes, 48, self.flags);
        bytes[52] = self.kind as u8;
        Ok(bytes)
    }
}

/// Значение directory tree. Ключ состоит из parent object id и имени.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryValue {
    pub object_id: u64,
    pub generation: u64,
}

impl DirectoryValue {
    pub const ENCODED_SIZE: usize = 16;

    pub fn encode(self) -> Result<[u8; Self::ENCODED_SIZE], Error> {
        if self.object_id == 0 || self.generation == 0 {
            return Err(Error::InvalidArgument);
        }
        let mut bytes = [0u8; Self::ENCODED_SIZE];
        put_u64(&mut bytes, 0, self.object_id);
        put_u64(&mut bytes, 8, self.generation);
        Ok(bytes)
    }
}

/// Значение extent tree. Ключ — `(object_id, logical_block)`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReliabilityClass {
    Checksummed = 1,
    Replicated = 2,
    Ephemeral = 3,
}

/// Data checksum хранится отдельно в checksum tree: она не должна лежать в
/// проверяемом data extent и не заставляет читать весь extent целиком.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtentValue {
    pub physical: u64,
    pub mirror_physical: u64,
    pub blocks: u64,
    pub flags: u32,
    pub reliability: ReliabilityClass,
}

impl ExtentValue {
    pub const ENCODED_SIZE: usize = 32;

    pub fn encode(self, volume_blocks: u64) -> Result<[u8; Self::ENCODED_SIZE], Error> {
        let end = self
            .physical
            .checked_add(self.blocks)
            .ok_or(Error::InvalidArgument)?;
        let mirror_end = self
            .mirror_physical
            .checked_add(self.blocks)
            .ok_or(Error::InvalidArgument)?;
        let replicated = self.reliability == ReliabilityClass::Replicated;
        if self.physical < FIRST_ALLOCATABLE_BLOCK
            || self.blocks == 0
            || end > volume_blocks
            || self.flags & !KNOWN_EXTENT_FLAGS != 0
            || replicated != (self.flags & EXTENT_FLAG_REPLICATED != 0)
            || (replicated
                && (self.mirror_physical < FIRST_ALLOCATABLE_BLOCK
                    || mirror_end > volume_blocks
                    || ranges_u64_overlap(self.physical, end, self.mirror_physical, mirror_end)))
            || (!replicated && self.mirror_physical != 0)
        {
            return Err(Error::InvalidArgument);
        }
        let mut bytes = [0u8; Self::ENCODED_SIZE];
        put_u64(&mut bytes, 0, self.physical);
        put_u64(&mut bytes, 8, self.mirror_physical);
        put_u64(&mut bytes, 16, self.blocks);
        put_u32(&mut bytes, 24, self.flags);
        bytes[28] = self.reliability as u8;
        Ok(bytes)
    }
}

/// Значение free-space tree. Ключ — physical start block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeSpaceValue {
    pub blocks: u64,
    /// Zone/placement hints не влияют на корректность allocator'а.
    pub hints: u64,
}

/// Алгоритм detached checksum пользовательских данных. Metadata node пока
/// использует CRC32 software fallback; data format сразу допускает сильный
/// digest без очередной смены layout.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChecksumAlgorithm {
    Crc32c = 1,
    Blake3 = 2,
}

/// Checksum диапазона до 128 КиБ (32 логических блока). Малый диапазон не
/// заставляет читать огромный extent ради проверки частичного `read`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DataChecksumValue {
    pub blocks: u16,
    pub algorithm: ChecksumAlgorithm,
    pub digest_len: u16,
    pub digest: [u8; 32],
}

impl DataChecksumValue {
    pub const ENCODED_SIZE: usize = 40;

    pub fn encode(self) -> Result<[u8; Self::ENCODED_SIZE], Error> {
        let expected_len = match self.algorithm {
            ChecksumAlgorithm::Crc32c => 4,
            ChecksumAlgorithm::Blake3 => 32,
        };
        if self.blocks == 0
            || self.blocks > 32
            || usize::from(self.digest_len) != expected_len
            || self.digest[expected_len..].iter().any(|byte| *byte != 0)
        {
            return Err(Error::InvalidArgument);
        }
        let mut bytes = [0u8; Self::ENCODED_SIZE];
        put_u16(&mut bytes, 0, self.blocks);
        put_u16(&mut bytes, 2, self.algorithm as u16);
        put_u16(&mut bytes, 4, self.digest_len);
        bytes[8..40].copy_from_slice(&self.digest);
        Ok(bytes)
    }
}

/// Reverse mapping позволяет scrub/fsck независимо сверить physical extent с
/// владельцем. Ключ space tree содержит record type и physical start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReverseMapValue {
    pub object_id: u64,
    pub logical_block: u64,
    pub blocks: u64,
    pub generation: u64,
}

impl ReverseMapValue {
    pub const ENCODED_SIZE: usize = 32;

    pub fn encode(self) -> Result<[u8; Self::ENCODED_SIZE], Error> {
        if self.object_id == 0 || self.blocks == 0 || self.generation == 0 {
            return Err(Error::InvalidArgument);
        }
        let mut bytes = [0u8; Self::ENCODED_SIZE];
        put_u64(&mut bytes, 0, self.object_id);
        put_u64(&mut bytes, 8, self.logical_block);
        put_u64(&mut bytes, 16, self.blocks);
        put_u64(&mut bytes, 24, self.generation);
        Ok(bytes)
    }
}

pub const SPACE_KEY_FREE: u8 = 1;
pub const SPACE_KEY_REVERSE: u8 = 2;

/// Ключ space tree. Free range и reverse mapping живут в одном ordered
/// database, но имеют разные namespaces и строгие record validators.
pub const fn space_key(record_type: u8, physical_start: u64) -> [u8; 9] {
    let physical = physical_start.to_be_bytes();
    [
        record_type,
        physical[0],
        physical[1],
        physical[2],
        physical[3],
        physical[4],
        physical[5],
        physical[6],
        physical[7],
    ]
}

impl FreeSpaceValue {
    pub const ENCODED_SIZE: usize = 16;

    pub fn encode(self) -> Result<[u8; Self::ENCODED_SIZE], Error> {
        if self.blocks == 0 {
            return Err(Error::InvalidArgument);
        }
        let mut bytes = [0u8; Self::ENCODED_SIZE];
        put_u64(&mut bytes, 0, self.blocks);
        put_u64(&mut bytes, 8, self.hints);
        Ok(bytes)
    }
}

/// Numeric keys используют big endian, чтобы byte ordering node совпадал с
/// естественным порядком `u64` без architecture-dependent comparator'а.
pub const fn object_key(object_id: u64) -> [u8; 8] {
    object_id.to_be_bytes()
}

pub const fn extent_key(object_id: u64, logical_block: u64) -> [u8; 16] {
    let object = object_id.to_be_bytes();
    let logical = logical_block.to_be_bytes();
    [
        object[0], object[1], object[2], object[3], object[4], object[5], object[6], object[7],
        logical[0], logical[1], logical[2], logical[3], logical[4], logical[5], logical[6],
        logical[7],
    ]
}

/// Собирает key каталога в предоставленном bounded буфере. Имена хранятся как
/// bytes (как в Unix), поэтому Linux software легче портировать; UI отдельно
/// проверяет/отображает UTF-8 и не меняет on-disk identity имени.
pub fn directory_key<'a>(
    parent: u64,
    name: &[u8],
    output: &'a mut [u8; 8 + MAX_NAME_BYTES],
) -> Result<&'a [u8], Error> {
    if parent == 0
        || name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.iter().any(|byte| *byte == 0 || *byte == b'/')
        || name == b"."
        || name == b".."
    {
        return Err(Error::InvalidArgument);
    }
    output[..8].copy_from_slice(&parent.to_be_bytes());
    output[8..8 + name.len()].copy_from_slice(name);
    Ok(&output[..8 + name.len()])
}

/// Какая копия superblock была выбрана recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mount {
    pub superblock: Superblock,
    pub copy: u8,
}

/// Выбирает последнюю полностью публикуемую пару superblock/root nodes.
///
/// `read_block` не возвращает borrowed storage: caller может читать с диска,
/// из shared-memory block queue либо из тестового массива. Проверяются обе
/// копии и при torn latest commit выполняется откат к предыдущей.
pub fn recover<F>(actual_blocks: u64, mut read_block: F) -> Result<Mount, Error>
where
    F: FnMut(u64, &mut Block) -> bool,
{
    if actual_blocks < MIN_VOLUME_BLOCKS {
        return Err(Error::InvalidSuperblock);
    }
    let mut decoded: [Option<(u8, Superblock)>; 2] = [None, None];
    for copy in 0..2u8 {
        let mut block = [0u8; BLOCK_SIZE];
        if read_block(u64::from(copy), &mut block) {
            if let Ok(superblock) = Superblock::decode(&block, actual_blocks) {
                decoded[usize::from(copy)] = Some((copy, superblock));
            }
        }
    }
    let mut candidates = decoded;
    if sequence_is_newer(
        candidate_sequence(candidates[1]),
        candidate_sequence(candidates[0]),
    ) {
        candidates.swap(0, 1);
    }
    for (copy, superblock) in candidates.into_iter().flatten() {
        if roots_are_valid(superblock, &mut read_block) {
            return Ok(Mount { superblock, copy });
        }
    }
    Err(Error::InvalidRoot)
}

/// Commit чередует superblock copies. Nodes всегда записываются COW и поэтому
/// не выбирают copy по адресу старого root.
pub const fn superblock_copy(sequence: u64) -> u64 {
    sequence & 1
}

/// Durable write barrier, которого ожидает текущий COW commit.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitPhase {
    DataWrites = 1,
    DataFlush = 2,
    MetadataWrites = 3,
    MetadataFlush = 4,
    SuperblockWrite = 5,
    SuperblockFlush = 6,
    Complete = 7,
    Failed = 8,
}

/// Bounded state machine не даёт filesystem service случайно опубликовать
/// superblock до durable data/metadata. Она не выполняет I/O сама: completion
/// приходит от block capability queue после проверки device status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitOrder {
    sequence: u64,
    phase: CommitPhase,
    pending_data: u32,
    pending_metadata: u32,
}

impl CommitOrder {
    pub fn new(sequence: u64, data_writes: u32, metadata_writes: u32) -> Result<Self, Error> {
        if sequence == 0 || metadata_writes == 0 {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            sequence,
            phase: if data_writes == 0 {
                CommitPhase::DataFlush
            } else {
                CommitPhase::DataWrites
            },
            pending_data: data_writes,
            pending_metadata: metadata_writes,
        })
    }

    pub const fn phase(&self) -> CommitPhase {
        self.phase
    }

    pub const fn pending_data(&self) -> u32 {
        self.pending_data
    }

    pub const fn pending_metadata(&self) -> u32 {
        self.pending_metadata
    }

    pub fn data_write_completed(&mut self) -> Result<(), Error> {
        if self.phase != CommitPhase::DataWrites || self.pending_data == 0 {
            return Err(Error::InvalidArgument);
        }
        self.pending_data -= 1;
        if self.pending_data == 0 {
            self.phase = CommitPhase::DataFlush;
        }
        Ok(())
    }

    pub fn data_flush_completed(&mut self) -> Result<(), Error> {
        if self.phase != CommitPhase::DataFlush {
            return Err(Error::InvalidArgument);
        }
        self.phase = CommitPhase::MetadataWrites;
        Ok(())
    }

    pub fn metadata_write_completed(&mut self) -> Result<(), Error> {
        if self.phase != CommitPhase::MetadataWrites || self.pending_metadata == 0 {
            return Err(Error::InvalidArgument);
        }
        self.pending_metadata -= 1;
        if self.pending_metadata == 0 {
            self.phase = CommitPhase::MetadataFlush;
        }
        Ok(())
    }

    pub fn metadata_flush_completed(&mut self) -> Result<(), Error> {
        if self.phase != CommitPhase::MetadataFlush {
            return Err(Error::InvalidArgument);
        }
        self.phase = CommitPhase::SuperblockWrite;
        Ok(())
    }

    /// Адрес superblock разрешён только после durable metadata flush.
    pub fn superblock_target(&self) -> Result<u64, Error> {
        (self.phase == CommitPhase::SuperblockWrite)
            .then_some(superblock_copy(self.sequence))
            .ok_or(Error::InvalidArgument)
    }

    pub fn superblock_write_completed(&mut self) -> Result<(), Error> {
        if self.phase != CommitPhase::SuperblockWrite {
            return Err(Error::InvalidArgument);
        }
        self.phase = CommitPhase::SuperblockFlush;
        Ok(())
    }

    pub fn superblock_flush_completed(&mut self) -> Result<(), Error> {
        if self.phase != CommitPhase::SuperblockFlush {
            return Err(Error::InvalidArgument);
        }
        self.phase = CommitPhase::Complete;
        Ok(())
    }

    /// После любой device/queue ошибки commit навсегда закрыт. Caller может
    /// освободить только ещё не опубликованные новые blocks; старое поколение
    /// продолжает быть mountable.
    pub fn fail(&mut self) {
        if self.phase != CommitPhase::Complete {
            self.phase = CommitPhase::Failed;
        }
    }

    /// Reclamation старого поколения допустим только после последнего flush.
    pub fn can_reclaim_old_generation(&self) -> bool {
        self.phase == CommitPhase::Complete
    }
}

/// Полностью подготовленные блоки пустого тома. Host `mkfs` и будущий
/// installer записывают их без повторной реализации on-disk layout.
pub struct EmptyVolume {
    pub superblock: Block,
    pub roots: [Block; ROOT_COUNT],
}

/// Создаёт первое immutable поколение: root directory object, пустые
/// directory/extent/checksum/snapshot trees и один свободный диапазон.
pub fn format_empty(volume_blocks: u64, uuid: [u8; 16]) -> Result<EmptyVolume, Error> {
    if volume_blocks < MIN_VOLUME_BLOCKS || !uuid.iter().any(|byte| *byte != 0) {
        return Err(Error::InvalidArgument);
    }
    let sequence = 1;
    let first_root = FIRST_ALLOCATABLE_BLOCK;
    let roots = RootSet::new(
        RootPointer::new(first_root, sequence, 0, TreeKind::Inode),
        RootPointer::new(first_root + 1, sequence, 0, TreeKind::Directory),
        RootPointer::new(first_root + 2, sequence, 0, TreeKind::Extent),
        RootPointer::new(first_root + 3, sequence, 0, TreeKind::Space),
        RootPointer::new(first_root + 4, sequence, 0, TreeKind::Checksum),
        RootPointer::new(first_root + 5, sequence, 0, TreeKind::Snapshot),
    );
    let first_free = first_root + ROOT_COUNT as u64;
    let superblock = Superblock::new(
        volume_blocks,
        sequence,
        ROOT_OBJECT_ID + 1,
        first_free,
        uuid,
        roots,
    )
    .encode()?;

    let mut inode = NodeBuilder::new(
        TreeKind::Inode,
        0,
        sequence,
        first_root,
        uuid,
        volume_blocks,
    )?;
    let root_value = InodeValue {
        generation: sequence,
        size: 0,
        allocated_blocks: 0,
        created_ns: 0,
        modified_ns: 0,
        content_generation: sequence,
        flags: 0,
        kind: InodeKind::Directory,
    }
    .encode()?;
    inode.push(&object_key(ROOT_OBJECT_ID), &root_value, volume_blocks)?;

    let directory = NodeBuilder::new(
        TreeKind::Directory,
        0,
        sequence,
        first_root + 1,
        uuid,
        volume_blocks,
    )?;
    let extent = NodeBuilder::new(
        TreeKind::Extent,
        0,
        sequence,
        first_root + 2,
        uuid,
        volume_blocks,
    )?;
    let mut space = NodeBuilder::new(
        TreeKind::Space,
        0,
        sequence,
        first_root + 3,
        uuid,
        volume_blocks,
    )?;
    let free_value = FreeSpaceValue {
        blocks: volume_blocks - first_free,
        hints: 0,
    }
    .encode()?;
    space.push(
        &space_key(SPACE_KEY_FREE, first_free),
        &free_value,
        volume_blocks,
    )?;
    let checksum = NodeBuilder::new(
        TreeKind::Checksum,
        0,
        sequence,
        first_root + 4,
        uuid,
        volume_blocks,
    )?;
    let snapshot = NodeBuilder::new(
        TreeKind::Snapshot,
        0,
        sequence,
        first_root + 5,
        uuid,
        volume_blocks,
    )?;

    Ok(EmptyVolume {
        superblock,
        roots: [
            inode.finish()?,
            directory.finish()?,
            extent.finish()?,
            space.finish()?,
            checksum.finish()?,
            snapshot.finish()?,
        ],
    })
}

fn roots_are_valid<F>(superblock: Superblock, read_block: &mut F) -> bool
where
    F: FnMut(u64, &mut Block) -> bool,
{
    for root in superblock.roots.iter() {
        let mut block = [0u8; BLOCK_SIZE];
        if !read_block(root.block, &mut block) {
            return false;
        }
        // Файл-контейнер может быть больше опубликованного volume после grow.
        // Ни один child/extent из старого поколения не получает право ссылаться
        // в этот хвост до атомарной публикации нового размера в superblock.
        let Ok(node) = NodeView::parse(
            &block,
            root.block,
            superblock.uuid,
            superblock.volume_blocks,
        ) else {
            return false;
        };
        let header = node.header();
        if header.kind != root.kind
            || header.level != root.level
            || header.generation != root.generation
        {
            return false;
        }
    }
    true
}

fn candidate_sequence(candidate: Option<(u8, Superblock)>) -> u64 {
    candidate.map_or(0, |(_, superblock)| superblock.sequence)
}

/// Serial-number arithmetic: после `u64::MAX` поколение 1 новее, но скачок
/// более чем на половину пространства считается старым/повреждённым.
fn sequence_is_newer(candidate: u64, reference: u64) -> bool {
    candidate != reference && candidate.wrapping_sub(reference) < (1u64 << 63)
}

fn leaf_shape_is_valid(header: NodeHeader, item: NodeItem<'_>, volume_blocks: u64) -> bool {
    if !tree_key_is_valid(header.kind, item.key, volume_blocks) {
        return false;
    }
    if header.level != 0 {
        return item.value.len() == 8
            && get_u64(item.value, 0) >= FIRST_ALLOCATABLE_BLOCK
            && get_u64(item.value, 0) < volume_blocks;
    }
    match header.kind {
        TreeKind::Inode => {
            item.key.len() == 8
                && get_be_u64(item.key, 0) != 0
                && inode_value_is_valid(item.value, volume_blocks)
        }
        TreeKind::Directory => {
            item.value.len() == DirectoryValue::ENCODED_SIZE
                && get_u64(item.value, 0) != 0
                && get_u64(item.value, 8) != 0
        }
        TreeKind::Extent => {
            item.key.len() == 16
                && get_be_u64(item.key, 0) != 0
                && extent_value_is_valid(item.value, volume_blocks)
        }
        TreeKind::Space => {
            if item.key.len() != 9 {
                return false;
            }
            let start = get_be_u64(item.key, 1);
            if start < FIRST_ALLOCATABLE_BLOCK || start >= volume_blocks {
                return false;
            }
            match item.key[0] {
                SPACE_KEY_FREE if item.value.len() == FreeSpaceValue::ENCODED_SIZE => {
                    let blocks = get_u64(item.value, 0);
                    blocks != 0
                        && start
                            .checked_add(blocks)
                            .is_some_and(|end| end <= volume_blocks)
                }
                SPACE_KEY_REVERSE if item.value.len() == ReverseMapValue::ENCODED_SIZE => {
                    let blocks = get_u64(item.value, 16);
                    get_u64(item.value, 0) != 0
                        && blocks != 0
                        && get_u64(item.value, 24) != 0
                        && start
                            .checked_add(blocks)
                            .is_some_and(|end| end <= volume_blocks)
                }
                _ => false,
            }
        }
        TreeKind::Checksum => {
            let start = get_be_u64(item.key, 0);
            checksum_value_is_valid(item.value)
                && start
                    .checked_add(u64::from(get_u16(item.value, 0)))
                    .is_some_and(|end| end <= volume_blocks)
        }
        TreeKind::Snapshot => {
            // Snapshot manifest codec появится вместе с retention service.
            // До этого этапа разрешён только пустой root, чтобы нельзя было
            // опубликовать opaque запись без строгой проверки.
            false
        }
    }
}

fn tree_key_is_valid(kind: TreeKind, key: &[u8], volume_blocks: u64) -> bool {
    match kind {
        TreeKind::Inode => key.len() == 8 && get_be_u64(key, 0) != 0,
        TreeKind::Directory => {
            let name = key.get(8..).unwrap_or_default();
            key.len() > 8
                && key.len() <= 8 + MAX_NAME_BYTES
                && get_be_u64(key, 0) != 0
                && !name.iter().any(|byte| *byte == 0 || *byte == b'/')
                && name != b"."
                && name != b".."
        }
        TreeKind::Extent => key.len() == 16 && get_be_u64(key, 0) != 0,
        TreeKind::Space => {
            key.len() == 9
                && matches!(key[0], SPACE_KEY_FREE | SPACE_KEY_REVERSE)
                && get_be_u64(key, 1) >= FIRST_ALLOCATABLE_BLOCK
                && get_be_u64(key, 1) < volume_blocks
        }
        TreeKind::Checksum => {
            key.len() == 8
                && get_be_u64(key, 0) >= FIRST_ALLOCATABLE_BLOCK
                && get_be_u64(key, 0) < volume_blocks
        }
        TreeKind::Snapshot => key.len() == 8 && get_be_u64(key, 0) != 0,
    }
}

fn checksum_value_is_valid(value: &[u8]) -> bool {
    if value.len() != DataChecksumValue::ENCODED_SIZE {
        return false;
    }
    let blocks = get_u16(value, 0);
    let algorithm = get_u16(value, 2);
    let digest_len = usize::from(get_u16(value, 4));
    let expected_len = match algorithm {
        value if value == ChecksumAlgorithm::Crc32c as u16 => 4,
        value if value == ChecksumAlgorithm::Blake3 as u16 => 32,
        _ => return false,
    };
    blocks != 0
        && blocks <= 32
        && digest_len == expected_len
        && value[6..8].iter().all(|byte| *byte == 0)
        && value[8 + expected_len..].iter().all(|byte| *byte == 0)
}

fn inode_value_is_valid(value: &[u8], volume_blocks: u64) -> bool {
    value.len() == InodeValue::ENCODED_SIZE
        && get_u64(value, 0) != 0
        && get_u64(value, 16) <= volume_blocks
        && get_u64(value, 40) != 0
        && get_u32(value, 48) == 0
        && matches!(value[52], 1..=3)
        && value[53..].iter().all(|byte| *byte == 0)
}

fn extent_value_is_valid(value: &[u8], volume_blocks: u64) -> bool {
    if value.len() != ExtentValue::ENCODED_SIZE {
        return false;
    }
    let physical = get_u64(value, 0);
    let mirror = get_u64(value, 8);
    let blocks = get_u64(value, 16);
    let flags = get_u32(value, 24);
    let reliability = value[28];
    let Some(end) = physical.checked_add(blocks) else {
        return false;
    };
    let Some(mirror_end) = mirror.checked_add(blocks) else {
        return false;
    };
    let replicated = reliability == ReliabilityClass::Replicated as u8;
    physical >= FIRST_ALLOCATABLE_BLOCK
        && blocks != 0
        && end <= volume_blocks
        && flags & !KNOWN_EXTENT_FLAGS == 0
        && matches!(
            reliability,
            value if value == ReliabilityClass::Checksummed as u8
                || value == ReliabilityClass::Replicated as u8
                || value == ReliabilityClass::Ephemeral as u8
        )
        && replicated == (flags & EXTENT_FLAG_REPLICATED != 0)
        && ((!replicated && mirror == 0)
            || (replicated
                && mirror >= FIRST_ALLOCATABLE_BLOCK
                && mirror_end <= volume_blocks
                && !ranges_u64_overlap(physical, end, mirror, mirror_end)))
        && value[29..].iter().all(|byte| *byte == 0)
}

fn ranges_u64_overlap(left_start: u64, left_end: u64, right_start: u64, right_end: u64) -> bool {
    left_start < right_end && right_start < left_end
}

fn range_from_slot(block: &[u8], offset: usize) -> Option<Range<usize>> {
    let start = usize::from(get_u16(block, offset));
    let length = usize::from(get_u16(block, offset + 2));
    let end = start.checked_add(length)?;
    (end <= BLOCK_SIZE).then_some(start..end)
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

/// CRC-32C (Castagnoli). На AMD64 и AArch64 runtime сможет заменить этот
/// маленький software fallback hardware intrinsic без смены on-disk format.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes.iter().copied() {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn crc32c_with_zeroed_u32(block: &Block, offset: usize) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for (index, byte) in block.iter().copied().enumerate() {
        let byte = if (offset..offset + 4).contains(&index) {
            0
        } else {
            byte
        };
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn get_be_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;

    const VOLUME_BLOCKS: u64 = MIN_VOLUME_BLOCKS;
    const TEST_UUID: [u8; 16] = *b"VaraniaFS-v2test";

    fn roots(generation: u64, first_block: u64) -> RootSet {
        RootSet::new(
            RootPointer::new(first_block, generation, 0, TreeKind::Inode),
            RootPointer::new(first_block + 1, generation, 0, TreeKind::Directory),
            RootPointer::new(first_block + 2, generation, 0, TreeKind::Extent),
            RootPointer::new(first_block + 3, generation, 0, TreeKind::Space),
            RootPointer::new(first_block + 4, generation, 0, TreeKind::Checksum),
            RootPointer::new(first_block + 5, generation, 0, TreeKind::Snapshot),
        )
    }

    fn inode_value(generation: u64) -> [u8; InodeValue::ENCODED_SIZE] {
        InodeValue {
            generation,
            size: 0,
            allocated_blocks: 0,
            created_ns: 0,
            modified_ns: 0,
            content_generation: generation,
            flags: 0,
            kind: InodeKind::Directory,
        }
        .encode()
        .unwrap()
    }

    fn make_root_nodes(generation: u64, first_block: u64) -> [Block; ROOT_COUNT] {
        let mut inode = NodeBuilder::new(
            TreeKind::Inode,
            0,
            generation,
            first_block,
            TEST_UUID,
            VOLUME_BLOCKS,
        )
        .unwrap();
        inode
            .push(
                &object_key(ROOT_OBJECT_ID),
                &inode_value(generation),
                VOLUME_BLOCKS,
            )
            .unwrap();

        let directory = NodeBuilder::new(
            TreeKind::Directory,
            0,
            generation,
            first_block + 1,
            TEST_UUID,
            VOLUME_BLOCKS,
        )
        .unwrap();
        let extent = NodeBuilder::new(
            TreeKind::Extent,
            0,
            generation,
            first_block + 2,
            TEST_UUID,
            VOLUME_BLOCKS,
        )
        .unwrap();
        let mut free = NodeBuilder::new(
            TreeKind::Space,
            0,
            generation,
            first_block + 3,
            TEST_UUID,
            VOLUME_BLOCKS,
        )
        .unwrap();
        let free_start = first_block + ROOT_COUNT as u64;
        free.push(
            &space_key(SPACE_KEY_FREE, free_start),
            &FreeSpaceValue {
                blocks: VOLUME_BLOCKS - free_start,
                hints: 0,
            }
            .encode()
            .unwrap(),
            VOLUME_BLOCKS,
        )
        .unwrap();
        let checksum = NodeBuilder::new(
            TreeKind::Checksum,
            0,
            generation,
            first_block + 4,
            TEST_UUID,
            VOLUME_BLOCKS,
        )
        .unwrap();
        let snapshot = NodeBuilder::new(
            TreeKind::Snapshot,
            0,
            generation,
            first_block + 5,
            TEST_UUID,
            VOLUME_BLOCKS,
        )
        .unwrap();
        [
            inode.finish().unwrap(),
            directory.finish().unwrap(),
            extent.finish().unwrap(),
            free.finish().unwrap(),
            checksum.finish().unwrap(),
            snapshot.finish().unwrap(),
        ]
    }

    fn superblock(generation: u64, first_block: u64) -> Superblock {
        Superblock::new(
            VOLUME_BLOCKS,
            generation,
            ROOT_OBJECT_ID + 1,
            SUPERBLOCK_COPIES + ROOT_COUNT as u64,
            TEST_UUID,
            roots(generation, first_block),
        )
    }

    #[test]
    fn superblock_roundtrip_rejects_corruption_and_unknown_features() {
        let source = superblock(7, 2);
        let encoded = source.encode().unwrap();
        assert_eq!(Superblock::decode(&encoded, VOLUME_BLOCKS), Ok(source));

        let mut corrupt = encoded;
        corrupt[300] ^= 0x80;
        assert_eq!(
            Superblock::decode(&corrupt, VOLUME_BLOCKS),
            Err(Error::InvalidChecksum)
        );

        // Даже с пересчитанной checksum неизвестные поля текущей версии не
        // принимаются молча: для них нужна новая feature/version семантика.
        let mut unknown_reserved = encoded;
        unknown_reserved[300] = 1;
        put_u32(&mut unknown_reserved, SUPERBLOCK_CHECKSUM_OFFSET, 0);
        let checksum = crc32c_with_zeroed_u32(&unknown_reserved, SUPERBLOCK_CHECKSUM_OFFSET);
        put_u32(&mut unknown_reserved, SUPERBLOCK_CHECKSUM_OFFSET, checksum);
        assert_eq!(
            Superblock::decode(&unknown_reserved, VOLUME_BLOCKS),
            Err(Error::InvalidSuperblock)
        );

        let mut unsupported = source;
        unsupported.incompatible_features |= 1 << 63;
        assert_eq!(unsupported.encode(), Err(Error::InvalidArgument));
    }

    #[test]
    fn node_builder_preserves_sorted_keys_and_validates_grapheme_agnostic_names() {
        let mut builder =
            NodeBuilder::new(TreeKind::Directory, 0, 3, 10, TEST_UUID, VOLUME_BLOCKS).unwrap();
        let mut first_key = [0u8; 8 + MAX_NAME_BYTES];
        let mut second_key = [0u8; 8 + MAX_NAME_BYTES];
        let first = directory_key(ROOT_OBJECT_ID, "Документы".as_bytes(), &mut first_key).unwrap();
        let second = directory_key(ROOT_OBJECT_ID, "файл.txt".as_bytes(), &mut second_key).unwrap();
        let first_value = DirectoryValue {
            object_id: 2,
            generation: 1,
        }
        .encode()
        .unwrap();
        let second_value = DirectoryValue {
            object_id: 3,
            generation: 1,
        }
        .encode()
        .unwrap();
        builder.push(first, &first_value, VOLUME_BLOCKS).unwrap();
        builder.push(second, &second_value, VOLUME_BLOCKS).unwrap();
        let block = builder.finish().unwrap();
        let node = NodeView::parse(&block, 10, TEST_UUID, VOLUME_BLOCKS).unwrap();
        assert_eq!(node.header().item_count, 2);
        assert_eq!(node.item(0).unwrap().key, first);
        assert_eq!(node.item(1).unwrap().key, second);

        let mut invalid_key = [0u8; 8 + MAX_NAME_BYTES];
        assert_eq!(
            directory_key(ROOT_OBJECT_ID, b"bad/name", &mut invalid_key),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn node_rejects_checksum_overlap_and_unsorted_items() {
        let mut builder =
            NodeBuilder::new(TreeKind::Inode, 0, 1, 2, TEST_UUID, VOLUME_BLOCKS).unwrap();
        builder
            .push(&object_key(2), &inode_value(1), VOLUME_BLOCKS)
            .unwrap();
        assert_eq!(
            builder.push(&object_key(1), &inode_value(1), VOLUME_BLOCKS),
            Err(Error::UnorderedKeys)
        );
        let mut block = builder.finish().unwrap();
        assert_eq!(
            NodeView::parse(&block, 3, TEST_UUID, VOLUME_BLOCKS).err(),
            Some(Error::InvalidNode)
        );
        assert_eq!(
            NodeView::parse(&block, 2, *b"another-volume!!", VOLUME_BLOCKS).err(),
            Some(Error::InvalidNode)
        );
        block[BLOCK_SIZE - 1] ^= 1;
        assert!(matches!(
            NodeView::parse(&block, 2, TEST_UUID, VOLUME_BLOCKS),
            Err(Error::InvalidChecksum)
        ));

        let mut unknown_gap = builder_with_one_inode().finish().unwrap();
        unknown_gap[NODE_HEADER_SIZE + SLOT_SIZE] = 1;
        put_u32(&mut unknown_gap, NODE_CHECKSUM_OFFSET, 0);
        let checksum = crc32c_with_zeroed_u32(&unknown_gap, NODE_CHECKSUM_OFFSET);
        put_u32(&mut unknown_gap, NODE_CHECKSUM_OFFSET, checksum);
        assert_eq!(
            NodeView::parse(&unknown_gap, 2, TEST_UUID, VOLUME_BLOCKS).err(),
            Some(Error::InvalidNode)
        );
    }

    fn builder_with_one_inode() -> NodeBuilder {
        let mut builder =
            NodeBuilder::new(TreeKind::Inode, 0, 1, 2, TEST_UUID, VOLUME_BLOCKS).unwrap();
        builder
            .push(&object_key(2), &inode_value(1), VOLUME_BLOCKS)
            .unwrap();
        builder
    }

    #[test]
    fn recovery_rolls_back_torn_superblock_and_missing_new_root() {
        let old_nodes = make_root_nodes(1, 2);
        let new_nodes = make_root_nodes(2, 8);
        let old_superblock = superblock(1, 2).encode().unwrap();
        let new_superblock = superblock(2, 8).encode().unwrap();
        let mut disk = vec![[0u8; BLOCK_SIZE]; 14];
        disk[0] = old_superblock;
        for (index, node) in old_nodes.into_iter().enumerate() {
            disk[2 + index] = node;
        }
        for (index, node) in new_nodes.into_iter().enumerate() {
            disk[8 + index] = node;
        }

        let mounted = recover(VOLUME_BLOCKS, |block, output| {
            let Some(source) = disk.get(block as usize) else {
                return false;
            };
            *output = *source;
            true
        })
        .unwrap();
        assert_eq!(mounted.superblock.sequence, 1);

        // Новый superblock опубликован, но один root torn: mount обязан
        // сохранить последнее полностью валидное поколение.
        disk[1] = new_superblock;
        disk[10][100] ^= 1;
        let mounted = recover(VOLUME_BLOCKS, |block, output| {
            let Some(source) = disk.get(block as usize) else {
                return false;
            };
            *output = *source;
            true
        })
        .unwrap();
        assert_eq!(mounted.superblock.sequence, 1);

        disk[10][100] ^= 1;
        let mounted = recover(VOLUME_BLOCKS, |block, output| {
            let Some(source) = disk.get(block as usize) else {
                return false;
            };
            *output = *source;
            true
        })
        .unwrap();
        assert_eq!(mounted.superblock.sequence, 2);
        assert_eq!(mounted.copy, 1);
    }

    #[test]
    fn recovery_never_uses_container_tail_beyond_published_volume() {
        let actual_blocks = VOLUME_BLOCKS + 8;
        let mut nodes = make_root_nodes(1, 2);
        let mut extent =
            NodeBuilder::new(TreeKind::Extent, 0, 1, 4, TEST_UUID, actual_blocks).unwrap();
        let outside_published_volume = ExtentValue {
            physical: VOLUME_BLOCKS,
            mirror_physical: 0,
            blocks: 1,
            flags: 0,
            reliability: ReliabilityClass::Checksummed,
        }
        .encode(actual_blocks)
        .unwrap();
        extent
            .push(&extent_key(2, 0), &outside_published_volume, actual_blocks)
            .unwrap();
        nodes[TreeKind::Extent.index()] = extent.finish().unwrap();
        let encoded_superblock = superblock(1, 2).encode().unwrap();

        assert_eq!(
            recover(actual_blocks, |block, output| match block {
                0 => {
                    *output = encoded_superblock;
                    true
                }
                block if (2..2 + ROOT_COUNT as u64).contains(&block) => {
                    *output = nodes[(block - 2) as usize];
                    true
                }
                _ => false,
            }),
            Err(Error::InvalidRoot)
        );
    }

    #[test]
    fn every_power_cut_before_all_roots_are_durable_keeps_old_generation() {
        let old_nodes = make_root_nodes(1, 2);
        let new_nodes = make_root_nodes(2, 8);
        let old_superblock = superblock(1, 2).encode().unwrap();
        let new_superblock = superblock(2, 8).encode().unwrap();

        for durable_new_roots in 0..=ROOT_COUNT {
            let mut disk = vec![[0u8; BLOCK_SIZE]; 14];
            disk[0] = old_superblock;
            disk[1] = new_superblock;
            for (index, node) in old_nodes.iter().copied().enumerate() {
                disk[2 + index] = node;
            }
            for (index, node) in new_nodes
                .iter()
                .copied()
                .take(durable_new_roots)
                .enumerate()
            {
                disk[8 + index] = node;
            }
            let mounted = recover(VOLUME_BLOCKS, |block, output| {
                let Some(source) = disk.get(block as usize) else {
                    return false;
                };
                *output = *source;
                true
            })
            .unwrap();
            let expected = if durable_new_roots == ROOT_COUNT {
                2
            } else {
                1
            };
            assert_eq!(mounted.superblock.sequence, expected);
        }
    }

    #[test]
    fn sequence_comparison_handles_wraparound() {
        assert!(sequence_is_newer(11, 10));
        assert!(!sequence_is_newer(10, 11));
        assert!(sequence_is_newer(1, u64::MAX));
        assert!(!sequence_is_newer(u64::MAX, 1));
        assert!(!sequence_is_newer(7, 7));
    }

    #[test]
    fn capacity_error_does_not_publish_a_partially_valid_node() {
        let mut builder =
            NodeBuilder::new(TreeKind::Inode, 0, 1, 2, TEST_UUID, VOLUME_BLOCKS).unwrap();
        let value = inode_value(1);
        let mut object = 1u64;
        loop {
            match builder.push(&object_key(object), &value, VOLUME_BLOCKS) {
                Ok(()) => object += 1,
                Err(Error::Capacity) => break,
                other => panic!("unexpected builder result: {other:?}"),
            }
        }
        let block = builder.finish().unwrap();
        let node = NodeView::parse(&block, 2, TEST_UUID, VOLUME_BLOCKS).unwrap();
        assert!(node.header().item_count > 1);
        assert_eq!(node.item(usize::from(node.header().item_count)), None);
    }

    #[test]
    fn crc32c_has_stable_reference_vector_and_differs_from_v1_crc() {
        let mut block = [0u8; BLOCK_SIZE];
        block[..9].copy_from_slice(b"123456789");
        assert_ne!(
            crc32c_with_zeroed_u32(&block, 100),
            crate::crc32(b"123456789")
        );
        assert_eq!(crate::crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn checksum_and_reverse_map_records_have_strict_shapes() {
        let mut checksum =
            NodeBuilder::new(TreeKind::Checksum, 0, 1, 20, TEST_UUID, VOLUME_BLOCKS).unwrap();
        let mut digest = [0u8; 32];
        digest[..4].copy_from_slice(&0xe306_9283u32.to_le_bytes());
        let value = DataChecksumValue {
            blocks: 1,
            algorithm: ChecksumAlgorithm::Crc32c,
            digest_len: 4,
            digest,
        }
        .encode()
        .unwrap();
        checksum
            .push(&object_key(100), &value, VOLUME_BLOCKS)
            .unwrap();
        let block = checksum.finish().unwrap();
        assert!(NodeView::parse(&block, 20, TEST_UUID, VOLUME_BLOCKS).is_ok());

        let mut space =
            NodeBuilder::new(TreeKind::Space, 0, 1, 21, TEST_UUID, VOLUME_BLOCKS).unwrap();
        let reverse = ReverseMapValue {
            object_id: 7,
            logical_block: 3,
            blocks: 4,
            generation: 1,
        }
        .encode()
        .unwrap();
        space
            .push(&space_key(SPACE_KEY_REVERSE, 100), &reverse, VOLUME_BLOCKS)
            .unwrap();
        let block = space.finish().unwrap();
        assert!(NodeView::parse(&block, 21, TEST_UUID, VOLUME_BLOCKS).is_ok());

        let replicated = ExtentValue {
            physical: 100,
            mirror_physical: 200,
            blocks: 8,
            flags: EXTENT_FLAG_REPLICATED,
            reliability: ReliabilityClass::Replicated,
        };
        assert!(replicated.encode(VOLUME_BLOCKS).is_ok());
        assert_eq!(
            ExtentValue {
                mirror_physical: 104,
                ..replicated
            }
            .encode(VOLUME_BLOCKS),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            ExtentValue {
                mirror_physical: 200,
                flags: 0,
                reliability: ReliabilityClass::Checksummed,
                ..replicated
            }
            .encode(VOLUME_BLOCKS),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn empty_volume_is_mountable_by_the_same_recovery_parser() {
        let image = format_empty(VOLUME_BLOCKS, TEST_UUID).unwrap();
        let mounted = recover(VOLUME_BLOCKS, |block, output| match block {
            0 | 1 => {
                *output = image.superblock;
                true
            }
            block if (2..2 + ROOT_COUNT as u64).contains(&block) => {
                *output = image.roots[(block - 2) as usize];
                true
            }
            _ => false,
        })
        .unwrap();
        assert_eq!(mounted.superblock.sequence, 1);
        assert_eq!(mounted.superblock.next_object_id, ROOT_OBJECT_ID + 1);
        assert_eq!(mounted.superblock.roots.get(TreeKind::Snapshot).block, 7);
    }

    #[test]
    fn commit_order_never_publishes_or_reclaims_before_all_flushes() {
        let mut order = CommitOrder::new(7, 2, 2).unwrap();
        assert_eq!(order.superblock_target(), Err(Error::InvalidArgument));
        assert!(!order.can_reclaim_old_generation());
        order.data_write_completed().unwrap();
        assert_eq!(order.phase(), CommitPhase::DataWrites);
        order.data_write_completed().unwrap();
        assert_eq!(order.phase(), CommitPhase::DataFlush);
        assert_eq!(
            order.metadata_write_completed(),
            Err(Error::InvalidArgument)
        );
        order.data_flush_completed().unwrap();
        order.metadata_write_completed().unwrap();
        order.metadata_write_completed().unwrap();
        assert_eq!(order.phase(), CommitPhase::MetadataFlush);
        order.metadata_flush_completed().unwrap();
        assert_eq!(order.superblock_target(), Ok(1));
        order.superblock_write_completed().unwrap();
        assert!(!order.can_reclaim_old_generation());
        order.superblock_flush_completed().unwrap();
        assert!(order.can_reclaim_old_generation());
        assert_eq!(
            order.superblock_flush_completed(),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn failed_commit_cannot_be_resumed_or_reclaim_old_blocks() {
        let mut order = CommitOrder::new(8, 0, 1).unwrap();
        assert_eq!(order.phase(), CommitPhase::DataFlush);
        order.fail();
        assert_eq!(order.phase(), CommitPhase::Failed);
        assert_eq!(order.data_flush_completed(), Err(Error::InvalidArgument));
        assert_eq!(order.superblock_target(), Err(Error::InvalidArgument));
        assert!(!order.can_reclaim_old_generation());
    }
}

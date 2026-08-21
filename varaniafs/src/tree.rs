//! Рабочий copy-on-write B+tree и транзакционный writer VaraniaFS.
//!
//! Модуль не выделяет память динамически и поэтому одинаково пригоден для
//! `vfsd`, host-утилит и раннего recovery. Изменяемые узлы никогда не
//! перезаписываются на месте: транзакция строит новое поколение снизу вверх,
//! сбрасывает его на устройство и лишь затем публикует superblock.

use crate::{
    allocator::{BlockAllocator, Extent, MAX_CACHED_EXTENTS},
    format::{
        space_key, superblock_copy, Block, Error, FreeSpaceValue, NodeBuilder, NodeView,
        RootPointer, Superblock, TreeKind, FIRST_ALLOCATABLE_BLOCK, MAX_NAME_BYTES,
        MAX_TREE_HEIGHT, SPACE_KEY_FREE,
    },
    intent::IntentRecord,
    BLOCK_SIZE,
};

/// Максимальный ключ — `(parent object id, name)`.
pub const MAX_KEY_BYTES: usize = 8 + MAX_NAME_BYTES;
/// Самая крупная встроенная metadata-запись — manifest snapshot.
pub const MAX_VALUE_BYTES: usize = 256;
/// Один syscall VFS не должен порождать неограниченную metadata-транзакцию.
pub const MAX_PENDING_BLOCKS: usize = 64;
pub const MAX_RETIRED_BLOCKS: usize = 128;
const MIN_NODE_BYTES: usize = BLOCK_SIZE / 4;
const MAX_SPACE_UPDATES: usize = 8;

/// Минимальный block-device контракт. `flush` обязан завершаться только после
/// durable записи всех предыдущих команд либо возвращать ошибку.
pub trait BlockDevice {
    fn read(&mut self, block: u64, output: &mut Block) -> Result<(), Error>;
    fn write(&mut self, block: u64, input: &Block) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
}

#[derive(Clone, Copy)]
struct PendingBlock {
    number: u64,
    image: Block,
    live: bool,
    metadata: bool,
}

impl PendingBlock {
    const EMPTY: Self = Self {
        number: 0,
        image: [0; BLOCK_SIZE],
        live: false,
        metadata: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathEntry {
    block: u64,
    child_index: u16,
}

const EMPTY_PATH: PathEntry = PathEntry {
    block: 0,
    child_index: 0,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ChildRef {
    key: [u8; MAX_KEY_BYTES],
    key_len: u16,
    block: u64,
}

impl ChildRef {
    const EMPTY: Self = Self {
        key: [0; MAX_KEY_BYTES],
        key_len: 0,
        block: 0,
    };

    fn new(key: &[u8], block: u64) -> Result<Self, Error> {
        if key.is_empty() || key.len() > MAX_KEY_BYTES || block < FIRST_ALLOCATABLE_BLOCK {
            return Err(Error::InvalidArgument);
        }
        let mut result = Self::EMPTY;
        result.key[..key.len()].copy_from_slice(key);
        result.key_len = key.len() as u16;
        result.block = block;
        Ok(result)
    }

    fn key(&self) -> &[u8] {
        &self.key[..usize::from(self.key_len)]
    }

    fn value(self) -> [u8; 8] {
        self.block.to_le_bytes()
    }
}

#[derive(Clone, Copy)]
struct ChildUpdate {
    first: ChildRef,
    second: ChildRef,
    count: u8,
    remove: u8,
    start: u16,
    underfull: bool,
}

impl ChildUpdate {
    fn one(start: usize, remove: usize, child: ChildRef, underfull: bool) -> Self {
        Self {
            first: child,
            second: ChildRef::EMPTY,
            count: 1,
            remove: remove as u8,
            start: start as u16,
            underfull,
        }
    }

    fn two(start: usize, remove: usize, first: ChildRef, second: ChildRef) -> Self {
        Self {
            first,
            second,
            count: 2,
            remove: remove as u8,
            start: start as u16,
            underfull: false,
        }
    }
}

/// Результат точного поиска. Значение копируется в caller-owned buffer и не
/// зависит от времени жизни page cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lookup {
    pub length: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visit {
    Continue,
    Stop,
}

/// Переиспользуемая память для ещё не опубликованных блоков транзакции.
///
/// Буфер намеренно отделён от [`Transaction`]: долговечный `vfsd` держит один
/// экземпляр в своём BSS и не расходует небольшой пользовательский stack на
/// десятки 4-КиБ блоков. Host-инструменты могут создать его обычной локальной
/// переменной. Одновременно использовать workspace в двух транзакциях нельзя —
/// это гарантирует обычный mutable borrow Rust.
pub struct TransactionWorkspace {
    allocator: BlockAllocator,
    pending: [PendingBlock; MAX_PENDING_BLOCKS],
    pending_count: usize,
    retired: [u64; MAX_RETIRED_BLOCKS],
    retired_count: usize,
    loaded_free: [Extent; MAX_CACHED_EXTENTS],
    loaded_free_count: usize,
    deferred_data: [Extent; MAX_RETIRED_BLOCKS],
    deferred_data_count: usize,
    space_prepared: bool,
    updating_space: bool,
}

impl TransactionWorkspace {
    pub const fn new() -> Self {
        Self {
            allocator: BlockAllocator::empty(),
            pending: [PendingBlock::EMPTY; MAX_PENDING_BLOCKS],
            pending_count: 0,
            retired: [0; MAX_RETIRED_BLOCKS],
            retired_count: 0,
            loaded_free: [Extent::EMPTY; MAX_CACHED_EXTENTS],
            loaded_free_count: 0,
            deferred_data: [Extent::EMPTY; MAX_RETIRED_BLOCKS],
            deferred_data_count: 0,
            space_prepared: false,
            updating_space: false,
        }
    }

    fn reset(&mut self, mounted: Superblock) -> Result<(), Error> {
        self.allocator = BlockAllocator::new(
            mounted.volume_blocks,
            mounted.allocated_blocks.max(FIRST_ALLOCATABLE_BLOCK),
        )?;
        for pending in &mut self.pending[..self.pending_count] {
            *pending = PendingBlock::EMPTY;
        }
        self.pending_count = 0;
        self.retired[..self.retired_count].fill(0);
        self.retired_count = 0;
        self.loaded_free[..self.loaded_free_count].fill(Extent::EMPTY);
        self.loaded_free_count = 0;
        self.deferred_data[..self.deferred_data_count].fill(Extent::EMPTY);
        self.deferred_data_count = 0;
        self.space_prepared = false;
        self.updating_space = false;
        Ok(())
    }
}

impl Default for TransactionWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// Одна ограниченная COW-транзакция.
pub struct Transaction<'a, D: BlockDevice> {
    device: &'a mut D,
    workspace: &'a mut TransactionWorkspace,
    base: Superblock,
    roots: crate::format::RootSet,
    generation: u64,
    committed: bool,
}

impl<'a, D: BlockDevice> Transaction<'a, D> {
    pub fn begin(
        device: &'a mut D,
        mounted: Superblock,
        workspace: &'a mut TransactionWorkspace,
    ) -> Result<Self, Error> {
        let generation = mounted.sequence.wrapping_add(1).max(1);
        if generation == mounted.sequence {
            return Err(Error::Capacity);
        }
        workspace.reset(mounted)?;
        let mut transaction = Self {
            device,
            workspace,
            base: mounted,
            roots: mounted.roots,
            generation,
            committed: false,
        };
        transaction.load_free_space()?;
        Ok(transaction)
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn roots(&self) -> crate::format::RootSet {
        self.roots
    }

    pub const fn next_object_id(&self) -> u64 {
        self.base.next_object_id
    }

    pub fn allocate_object_id(&mut self) -> Result<u64, Error> {
        let object = self.base.next_object_id;
        self.base.next_object_id = object.checked_add(1).ok_or(Error::Capacity)?;
        Ok(object)
    }

    /// Точный поиск в текущем (включая ещё не опубликованные изменения)
    /// поколении дерева.
    pub fn lookup(
        &mut self,
        kind: TreeKind,
        key: &[u8],
        output: &mut [u8],
    ) -> Result<Option<Lookup>, Error> {
        let mut block_number = self.roots.get(kind).block;
        loop {
            let mut block = [0; BLOCK_SIZE];
            self.read_visible(block_number, &mut block)?;
            let node = NodeView::parse(
                &block,
                block_number,
                self.base.uuid,
                self.base.volume_blocks,
            )?;
            if node.header().kind != kind {
                return Err(Error::InvalidNode);
            }
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
                return Ok(Some(Lookup {
                    length: item.value.len() as u16,
                }));
            }
            let index = child_index(&node, key)?;
            block_number = child_block(node.item(index).ok_or(Error::InvalidNode)?.value)?;
        }
    }

    /// Ordered потоковый обход без materialization всего дерева. Callback
    /// получает slices только на время вызова и может остановить обход.
    pub fn for_each<F>(&mut self, kind: TreeKind, mut visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Visit,
    {
        #[derive(Clone, Copy)]
        struct Walk {
            block: u64,
            next: u16,
            entered: bool,
        }
        const EMPTY: Walk = Walk {
            block: 0,
            next: 0,
            entered: false,
        };
        let mut stack = [EMPTY; MAX_TREE_HEIGHT as usize + 1];
        stack[0].block = self.roots.get(kind).block;
        let mut depth = 1usize;
        while depth != 0 {
            let frame_index = depth - 1;
            let mut image = [0; BLOCK_SIZE];
            self.read_visible(stack[frame_index].block, &mut image)?;
            let node = NodeView::parse(
                &image,
                stack[frame_index].block,
                self.base.uuid,
                self.base.volume_blocks,
            )?;
            if node.header().kind != kind {
                return Err(Error::InvalidNode);
            }
            if node.header().level == 0 {
                let start = usize::from(stack[frame_index].next);
                for index in start..usize::from(node.header().item_count) {
                    let item = node.item(index).ok_or(Error::InvalidNode)?;
                    stack[frame_index].next = (index + 1) as u16;
                    if visit(item.key, item.value) == Visit::Stop {
                        return Ok(());
                    }
                }
                depth -= 1;
                continue;
            }
            if !stack[frame_index].entered {
                stack[frame_index].entered = true;
            }
            let next = usize::from(stack[frame_index].next);
            if next == usize::from(node.header().item_count) {
                depth -= 1;
                continue;
            }
            let item = node.item(next).ok_or(Error::InvalidNode)?;
            stack[frame_index].next += 1;
            if depth == stack.len() {
                return Err(Error::Capacity);
            }
            stack[depth] = Walk {
                block: child_block(item.value)?,
                next: 0,
                entered: false,
            };
            depth += 1;
        }
        Ok(())
    }

    pub fn insert(&mut self, kind: TreeKind, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.mutate(kind, key, Some(value), true)
    }

    pub fn upsert(&mut self, kind: TreeKind, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.mutate(kind, key, Some(value), false)
    }

    pub fn remove(&mut self, kind: TreeKind, key: &[u8]) -> Result<(), Error> {
        self.mutate(kind, key, None, true)
    }

    /// Выделяет и ставит в очередь один data block. Partial file writes должны
    /// сначала собрать полный новый блок в caller buffer: старый extent остаётся
    /// неизменным до публикации новой extent-записи.
    pub fn stage_data(&mut self, image: Block) -> Result<u64, Error> {
        let extent = self.workspace.allocator.allocate_data(1, 1)?;
        self.stage_pending(extent.start, image, false)?;
        Ok(extent.start)
    }

    /// Возвращает ещё не опубликованный data range allocator'у.
    pub fn abandon_data(&mut self, block: u64) -> Result<(), Error> {
        self.abandon(block);
        self.workspace.allocator.release(Extent {
            start: block,
            blocks: 1,
        })
    }

    /// Освобождение опубликованных data blocks откладывается до нового
    /// durable RootSet; snapshots при этом удерживают старые extents.
    pub fn defer_free_data(&mut self, extent: Extent) -> Result<(), Error> {
        if extent.blocks == 0 || self.workspace.deferred_data_count == MAX_RETIRED_BLOCKS {
            return Err(Error::Capacity);
        }
        self.workspace.deferred_data[self.workspace.deferred_data_count] = extent;
        self.workspace.deferred_data_count += 1;
        Ok(())
    }

    pub fn read_data(&mut self, block: u64, output: &mut Block) -> Result<(), Error> {
        if block < FIRST_ALLOCATABLE_BLOCK || block >= self.base.volume_blocks {
            return Err(Error::InvalidArgument);
        }
        if let Some(pending) = self.workspace.pending[..self.workspace.pending_count]
            .iter()
            .rev()
            .find(|pending| pending.live && !pending.metadata && pending.number == block)
        {
            *output = pending.image;
            Ok(())
        } else {
            self.device.read(block, output)
        }
    }

    pub const fn volume_blocks(&self) -> u64 {
        self.base.volume_blocks
    }

    /// Публикует поколение в crash-safe порядке: immutable blocks, flush,
    /// intent log, flush, superblock, flush. Старый superblock остаётся
    /// валидной точкой recovery на каждом промежуточном шаге.
    pub fn commit(mut self) -> Result<Superblock, Error> {
        let published = self.publish_intent()?;
        let encoded = published.encode()?;
        self.device
            .write(superblock_copy(self.generation), &encoded)?;
        self.device.flush()?;
        self.committed = true;
        Ok(published)
    }

    /// Быстрый `fsync`: после возврата поколение уже восстанавливается через
    /// intent log, но checkpoint superblock может сделать фоновый vfsd.
    pub fn fsync(mut self) -> Result<Superblock, Error> {
        let published = self.publish_intent()?;
        self.committed = true;
        Ok(published)
    }

    fn publish_intent(&mut self) -> Result<Superblock, Error> {
        self.prepare_space_tree()?;
        // User data раньше metadata: extent никогда не указывает на блок,
        // который ещё не прошёл durable barrier.
        let mut has_data = false;
        for index in 0..self.workspace.pending_count {
            let pending = &self.workspace.pending[index];
            if pending.live && !pending.metadata {
                self.device.write(pending.number, &pending.image)?;
                has_data = true;
            }
        }
        if has_data {
            self.device.flush()?;
        }
        for index in 0..self.workspace.pending_count {
            let pending = &self.workspace.pending[index];
            if pending.live && pending.metadata {
                self.device.write(pending.number, &pending.image)?;
                self.device.write(pending.number + 1, &pending.image)?;
            }
        }
        self.device.flush()?;
        let mut published = self.base;
        published.sequence = self.generation;
        published.roots = self.roots;
        published.allocated_blocks = self.workspace.allocator.high_water();
        let intent = IntentRecord {
            superblock: published,
        }
        .encode()?;
        let intent_block = IntentRecord::primary_block(self.generation);
        self.device.write(intent_block, &intent)?;
        self.device.write(intent_block + 1, &intent)?;
        self.device.flush()?;
        Ok(published)
    }

    fn mutate(
        &mut self,
        kind: TreeKind,
        key: &[u8],
        value: Option<&[u8]>,
        require_absent_or_present: bool,
    ) -> Result<(), Error> {
        if key.is_empty()
            || key.len() > MAX_KEY_BYTES
            || value.is_some_and(|bytes| bytes.len() > MAX_VALUE_BYTES)
        {
            return Err(Error::InvalidArgument);
        }
        let root = self.roots.get(kind);
        let mut path = [EMPTY_PATH; MAX_TREE_HEIGHT as usize + 1];
        let mut depth = 0usize;
        let mut block_number = root.block;
        loop {
            let mut image = [0; BLOCK_SIZE];
            self.read_visible(block_number, &mut image)?;
            let node = NodeView::parse(
                &image,
                block_number,
                self.base.uuid,
                self.base.volume_blocks,
            )?;
            if node.header().kind != kind {
                return Err(Error::InvalidNode);
            }
            if node.header().level == 0 {
                break;
            }
            if depth == path.len() {
                return Err(Error::Capacity);
            }
            let index = child_index(&node, key)?;
            path[depth] = PathEntry {
                block: block_number,
                child_index: index as u16,
            };
            depth += 1;
            block_number = child_block(node.item(index).ok_or(Error::InvalidNode)?.value)?;
        }

        let mut update =
            self.rebuild_leaf(kind, block_number, key, value, require_absent_or_present)?;
        while depth != 0 {
            depth -= 1;
            let entry = path[depth];
            // Результат дочерней перестройки относится к тому slot родителя,
            // по которому поиск спускался. Внутри дочернего узла `start=0`
            // имеет другой смысл и не должен протекать на следующий уровень.
            update.start = entry.child_index;
            if update.underfull {
                update =
                    self.try_merge_child(kind, entry.block, entry.child_index as usize, update)?;
            }
            update = self.rebuild_internal(kind, entry.block, update)?;
        }

        let new_root = if update.count == 1 {
            let mut image = [0; BLOCK_SIZE];
            self.read_visible(update.first.block, &mut image)?;
            let node = NodeView::parse(
                &image,
                update.first.block,
                self.base.uuid,
                self.base.volume_blocks,
            )?;
            if node.header().level != 0 && node.header().item_count == 1 {
                let only = node.item(0).ok_or(Error::InvalidNode)?;
                RootPointer::new(
                    child_block(only.value)?,
                    self.generation,
                    node.header().level - 1,
                    kind,
                )
            } else {
                RootPointer::new(
                    update.first.block,
                    self.generation,
                    node.header().level,
                    kind,
                )
            }
        } else {
            let block = self.allocate_block()?;
            let level = root.level.checked_add(1).ok_or(Error::Capacity)?;
            if level > MAX_TREE_HEIGHT {
                return Err(Error::Capacity);
            }
            let mut builder = NodeBuilder::new(
                kind,
                level,
                self.generation,
                block,
                self.base.uuid,
                self.base.volume_blocks,
            )?;
            builder.push(
                update.first.key(),
                &update.first.value(),
                self.base.volume_blocks,
            )?;
            builder.push(
                update.second.key(),
                &update.second.value(),
                self.base.volume_blocks,
            )?;
            self.stage(block, builder.finish()?)?;
            RootPointer::new(block, self.generation, level, kind)
        };
        self.roots = self.roots.with(kind, new_root);
        Ok(())
    }

    // Эти функции оперируют несколькими 4-КиБ scratch blocks. Запрещаем их
    // встраивание друг в друга: иначе оптимизатор объединяет все scratch
    // области в один stack frame, который превышает bounded ring-3 stack.
    #[inline(never)]
    fn rebuild_leaf(
        &mut self,
        kind: TreeKind,
        old_block: u64,
        key: &[u8],
        value: Option<&[u8]>,
        strict: bool,
    ) -> Result<ChildUpdate, Error> {
        let mut old_image = [0; BLOCK_SIZE];
        self.read_visible(old_block, &mut old_image)?;
        let old = NodeView::parse(
            &old_image,
            old_block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        let index = lower_bound(&old, key);
        let exists = old.item(index).is_some_and(|item| item.key == key);
        if strict && (value.is_some() == exists) {
            return Err(Error::InvalidArgument);
        }
        let new_count = usize::from(old.header().item_count)
            + usize::from(value.is_some() && !exists)
            - usize::from(value.is_none() && exists);
        let total_bytes = mutated_bytes(&old, key, value, exists)?;
        let split = total_bytes > BLOCK_SIZE;
        let pivot = if split {
            split_pivot(&old, key, value, exists)?
        } else {
            new_count
        };
        let left_block = self.allocate_block()?;
        let right_block = split.then(|| self.allocate_block()).transpose()?;
        let mut left = NodeBuilder::new(
            kind,
            0,
            self.generation,
            left_block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        let mut right = right_block
            .map(|number| {
                NodeBuilder::new(
                    kind,
                    0,
                    self.generation,
                    number,
                    self.base.uuid,
                    self.base.volume_blocks,
                )
            })
            .transpose()?;
        let mut first_left = ChildRef::EMPTY;
        let mut first_right = ChildRef::EMPTY;
        let mut ordinal = 0usize;
        for cursor in 0..=usize::from(old.header().item_count) {
            if cursor == index {
                if let Some(new_value) = value {
                    push_partitioned(
                        &mut left,
                        &mut right,
                        pivot,
                        ordinal,
                        key,
                        new_value,
                        self.base.volume_blocks,
                        &mut first_left,
                        &mut first_right,
                        left_block,
                        right_block,
                    )?;
                    ordinal += 1;
                }
                if exists {
                    continue;
                }
            }
            if cursor == usize::from(old.header().item_count) {
                break;
            }
            let item = old.item(cursor).ok_or(Error::InvalidNode)?;
            push_partitioned(
                &mut left,
                &mut right,
                pivot,
                ordinal,
                item.key,
                item.value,
                self.base.volume_blocks,
                &mut first_left,
                &mut first_right,
                left_block,
                right_block,
            )?;
            ordinal += 1;
        }
        if first_left.key_len == 0 {
            // Пустой leaf допустим только как корень или кратковременный
            // результат удаления перед merge. Ключ нужен родителю лишь как
            // стабильная граница до схлопывания уровня.
            first_left = ChildRef::new(key, left_block)?;
        }
        self.stage(left_block, left.finish()?)?;
        if let (Some(number), Some(builder)) = (right_block, right) {
            self.stage(number, builder.finish()?)?;
        }
        self.retire(old_block);
        if right_block.is_some() {
            Ok(ChildUpdate::two(0, 1, first_left, first_right))
        } else {
            Ok(ChildUpdate::one(
                0,
                1,
                first_left,
                total_bytes < MIN_NODE_BYTES,
            ))
        }
    }

    #[inline(never)]
    fn rebuild_internal(
        &mut self,
        kind: TreeKind,
        old_block: u64,
        update: ChildUpdate,
    ) -> Result<ChildUpdate, Error> {
        let mut old_image = [0; BLOCK_SIZE];
        self.read_visible(old_block, &mut old_image)?;
        let old = NodeView::parse(
            &old_image,
            old_block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        let count = usize::from(old.header().item_count) - usize::from(update.remove)
            + usize::from(update.count);
        let total = updated_internal_bytes(&old, update)?;
        let split = total > BLOCK_SIZE;
        let pivot = if split {
            internal_split_pivot(&old, update)?
        } else {
            count
        };
        let left_block = self.allocate_block()?;
        let right_block = split.then(|| self.allocate_block()).transpose()?;
        let mut left = NodeBuilder::new(
            kind,
            old.header().level,
            self.generation,
            left_block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        let mut right = right_block
            .map(|number| {
                NodeBuilder::new(
                    kind,
                    old.header().level,
                    self.generation,
                    number,
                    self.base.uuid,
                    self.base.volume_blocks,
                )
            })
            .transpose()?;
        let mut first_left = ChildRef::EMPTY;
        let mut first_right = ChildRef::EMPTY;
        let mut ordinal = 0usize;
        for cursor in 0..=usize::from(old.header().item_count) {
            if cursor == usize::from(update.start) {
                for child in [update.first, update.second]
                    .into_iter()
                    .take(usize::from(update.count))
                {
                    push_partitioned(
                        &mut left,
                        &mut right,
                        pivot,
                        ordinal,
                        child.key(),
                        &child.value(),
                        self.base.volume_blocks,
                        &mut first_left,
                        &mut first_right,
                        left_block,
                        right_block,
                    )?;
                    ordinal += 1;
                }
            }
            if cursor == usize::from(old.header().item_count) {
                break;
            }
            if cursor >= usize::from(update.start)
                && cursor < usize::from(update.start) + usize::from(update.remove)
            {
                continue;
            }
            let item = old.item(cursor).ok_or(Error::InvalidNode)?;
            push_partitioned(
                &mut left,
                &mut right,
                pivot,
                ordinal,
                item.key,
                item.value,
                self.base.volume_blocks,
                &mut first_left,
                &mut first_right,
                left_block,
                right_block,
            )?;
            ordinal += 1;
        }
        self.stage(left_block, left.finish()?)?;
        if let (Some(number), Some(builder)) = (right_block, right) {
            self.stage(number, builder.finish()?)?;
        }
        self.retire(old_block);
        if right_block.is_some() {
            Ok(ChildUpdate::two(0, 1, first_left, first_right))
        } else {
            Ok(ChildUpdate::one(0, 1, first_left, total < MIN_NODE_BYTES))
        }
    }

    /// Если после удаления узел мал, объединяем его с соседом. Это не условие
    /// корректности поиска, а ограничение фрагментации и высоты дерева.
    #[inline(never)]
    fn try_merge_child(
        &mut self,
        kind: TreeKind,
        parent_block: u64,
        child_index: usize,
        update: ChildUpdate,
    ) -> Result<ChildUpdate, Error> {
        if update.count != 1 {
            return Ok(update);
        }
        let mut parent_image = [0; BLOCK_SIZE];
        self.read_visible(parent_block, &mut parent_image)?;
        let parent = NodeView::parse(
            &parent_image,
            parent_block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        let parent_count = usize::from(parent.header().item_count);
        if parent_count < 2 {
            return Ok(update);
        }
        let (sibling_index, current_first) = if child_index + 1 < parent_count {
            (child_index + 1, true)
        } else {
            (child_index - 1, false)
        };
        let sibling_item = parent.item(sibling_index).ok_or(Error::InvalidNode)?;
        let sibling_block = child_block(sibling_item.value)?;
        let mut current_image = [0; BLOCK_SIZE];
        let mut sibling_image = [0; BLOCK_SIZE];
        self.read_visible(update.first.block, &mut current_image)?;
        self.read_visible(sibling_block, &mut sibling_image)?;
        let current = NodeView::parse(
            &current_image,
            update.first.block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        let sibling = NodeView::parse(
            &sibling_image,
            sibling_block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        if current.header().level != sibling.header().level
            || current.header().kind != kind
            || sibling.header().kind != kind
            || node_used_bytes(&current) + node_used_bytes(&sibling)
                - crate::format::NODE_HEADER_SIZE
                > BLOCK_SIZE
        {
            return Ok(update);
        }
        let merged_block = self.allocate_block()?;
        let mut builder = NodeBuilder::new(
            kind,
            current.header().level,
            self.generation,
            merged_block,
            self.base.uuid,
            self.base.volume_blocks,
        )?;
        let (first, second) = if current_first {
            (&current, &sibling)
        } else {
            (&sibling, &current)
        };
        for node in [first, second] {
            for index in 0..usize::from(node.header().item_count) {
                let item = node.item(index).ok_or(Error::InvalidNode)?;
                builder.push(item.key, item.value, self.base.volume_blocks)?;
            }
        }
        let first_key = first.item(0).ok_or(Error::InvalidNode)?.key;
        let merged_ref = ChildRef::new(first_key, merged_block)?;
        self.stage(merged_block, builder.finish()?)?;
        self.abandon(update.first.block);
        self.retire(sibling_block);
        let start = child_index.min(sibling_index);
        Ok(ChildUpdate::one(start, 2, merged_ref, false))
    }

    fn allocate_block(&mut self) -> Result<u64, Error> {
        if self.workspace.updating_space {
            self.workspace.allocator.allocate_metadata_pair_from_tail()
        } else {
            self.workspace.allocator.allocate_metadata_pair()
        }
    }

    fn stage(&mut self, number: u64, image: Block) -> Result<(), Error> {
        if number & 1 != 0 {
            return Err(Error::Capacity);
        }
        self.stage_pending(number, image, true)
    }

    fn stage_pending(&mut self, number: u64, image: Block, metadata: bool) -> Result<(), Error> {
        if let Some(slot) = self.workspace.pending[..self.workspace.pending_count]
            .iter_mut()
            .find(|pending| !pending.live)
        {
            *slot = PendingBlock {
                number,
                image,
                live: true,
                metadata,
            };
            return Ok(());
        }
        if self.workspace.pending_count == MAX_PENDING_BLOCKS {
            return Err(Error::Capacity);
        }
        self.workspace.pending[self.workspace.pending_count] = PendingBlock {
            number,
            image,
            live: true,
            metadata,
        };
        self.workspace.pending_count += 1;
        Ok(())
    }

    fn abandon(&mut self, number: u64) {
        if let Some(block) = self.workspace.pending[..self.workspace.pending_count]
            .iter_mut()
            .find(|pending| pending.number == number)
        {
            block.live = false;
        }
    }

    fn retire(&mut self, block: u64) {
        if self.workspace.pending[..self.workspace.pending_count]
            .iter()
            .any(|pending| pending.live && pending.number == block)
        {
            // Узел этого же неподтверждённого поколения никогда не попадал на
            // диск: его пару можно вернуть allocator'у немедленно.
            self.abandon(block);
            let _ = self.workspace.allocator.release(Extent {
                start: block,
                blocks: 2,
            });
            return;
        }
        if self.workspace.updating_space {
            // Перестройка space tree не может рекурсивно описывать собственные
            // retired nodes. Их подберёт offline reachability scan.
            return;
        }
        if self.workspace.retired_count < MAX_RETIRED_BLOCKS {
            self.workspace.retired[self.workspace.retired_count] = block;
            self.workspace.retired_count += 1;
        }
        if self.workspace.retired_count < MAX_RETIRED_BLOCKS {
            self.workspace.retired[self.workspace.retired_count] = block + 1;
            self.workspace.retired_count += 1;
        }
    }

    fn read_visible(&mut self, number: u64, output: &mut Block) -> Result<(), Error> {
        if let Some(pending) = self.workspace.pending[..self.workspace.pending_count]
            .iter()
            .rev()
            .find(|pending| pending.live && pending.number == number)
        {
            *output = pending.image;
            return Ok(());
        }
        self.device.read(number, output)?;
        if NodeView::parse(output, number, self.base.uuid, self.base.volume_blocks).is_ok() {
            return Ok(());
        }
        self.device.read(number + 1, output)?;
        NodeView::parse(output, number, self.base.uuid, self.base.volume_blocks).map(|_| ())
    }

    fn load_free_space(&mut self) -> Result<(), Error> {
        let sequence = self.base.sequence;
        let mut extents = [Extent::EMPTY; MAX_CACHED_EXTENTS];
        let mut count = 0usize;
        self.for_each(TreeKind::Space, |key, value| {
            if key.len() != 9 || key[0] != SPACE_KEY_FREE || count == MAX_SPACE_UPDATES {
                return Visit::Continue;
            }
            let Ok(record) = FreeSpaceValue::decode(value) else {
                return Visit::Continue;
            };
            if record.hints > sequence {
                return Visit::Continue;
            }
            let Ok(raw_start) = <[u8; 8]>::try_from(&key[1..9]) else {
                return Visit::Continue;
            };
            extents[count] = Extent {
                start: u64::from_be_bytes(raw_start),
                blocks: record.blocks,
            };
            count += 1;
            Visit::Continue
        })?;
        for extent in extents.into_iter().take(count) {
            self.workspace.allocator.add_free(extent)?;
            self.workspace.loaded_free[self.workspace.loaded_free_count] = extent;
            self.workspace.loaded_free_count += 1;
        }
        Ok(())
    }

    fn prepare_space_tree(&mut self) -> Result<(), Error> {
        if self.workspace.space_prepared {
            return Ok(());
        }
        self.workspace.space_prepared = true;
        let mut has_snapshot = false;
        self.for_each(TreeKind::Snapshot, |_, _| {
            has_snapshot = true;
            Visit::Stop
        })?;

        self.workspace.updating_space = true;
        // Удаляем записи, загруженные в allocator: после allocations их
        // остатки будут записаны заново одним согласованным снимком.
        for index in 0..self.workspace.loaded_free_count {
            let extent = self.workspace.loaded_free[index];
            let key = space_key(SPACE_KEY_FREE, extent.start);
            let mut value = [0; MAX_VALUE_BYTES];
            if self.lookup(TreeKind::Space, &key, &mut value)?.is_some() {
                self.remove(TreeKind::Space, &key)?;
            }
        }

        if !has_snapshot {
            let mut index = 0usize;
            while index + 1 < self.workspace.retired_count {
                let first = self.workspace.retired[index];
                let second = self.workspace.retired[index + 1];
                if second == first + 1 {
                    self.workspace.allocator.release(Extent {
                        start: first,
                        blocks: 2,
                    })?;
                }
                index += 2;
            }
            for index in 0..self.workspace.deferred_data_count {
                self.workspace
                    .allocator
                    .release(self.workspace.deferred_data[index])?;
            }
        }

        // Space nodes размещаются из high-water tail, поэтому следующий цикл
        // не изменяет снимок free cache под ногами.
        let free_count = self.workspace.allocator.free_count().min(MAX_SPACE_UPDATES);
        for index in 0..free_count {
            let extent = self
                .workspace
                .allocator
                .free_extent(index)
                .ok_or(Error::InvalidItem)?;
            let value = FreeSpaceValue {
                blocks: extent.blocks,
                // Следующая транзакция видит только уже durable поколение.
                hints: self.generation,
            }
            .encode()?;
            self.upsert(
                TreeKind::Space,
                &space_key(SPACE_KEY_FREE, extent.start),
                &value,
            )?;
        }
        self.workspace.updating_space = false;
        Ok(())
    }
}

impl<D: BlockDevice> Drop for Transaction<'_, D> {
    fn drop(&mut self) {
        // Отсутствие commit безопасно: superblock не указывает на новые узлы.
        // Освобождение orphan blocks выполняет mount-time orphan scan.
        let _ = self.committed;
    }
}

fn lower_bound(node: &NodeView<'_>, key: &[u8]) -> usize {
    let mut low = 0usize;
    let mut high = usize::from(node.header().item_count);
    while low < high {
        let middle = low + (high - low) / 2;
        let item = node.item(middle).expect("validated node item");
        if item.key < key {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn child_index(node: &NodeView<'_>, key: &[u8]) -> Result<usize, Error> {
    if node.header().item_count == 0 {
        return Err(Error::InvalidNode);
    }
    let lower = lower_bound(node, key);
    if lower == usize::from(node.header().item_count) {
        return Ok(lower - 1);
    }
    let item = node.item(lower).ok_or(Error::InvalidNode)?;
    Ok(if item.key == key || lower == 0 {
        lower
    } else {
        lower - 1
    })
}

fn child_block(value: &[u8]) -> Result<u64, Error> {
    let bytes: [u8; 8] = value.try_into().map_err(|_| Error::InvalidItem)?;
    let block = u64::from_le_bytes(bytes);
    (block >= FIRST_ALLOCATABLE_BLOCK)
        .then_some(block)
        .ok_or(Error::InvalidItem)
}

fn node_used_bytes(node: &NodeView<'_>) -> usize {
    let mut used = crate::format::NODE_HEADER_SIZE
        + usize::from(node.header().item_count) * crate::format::SLOT_SIZE;
    for index in 0..usize::from(node.header().item_count) {
        if let Some(item) = node.item(index) {
            used += item.key.len() + item.value.len();
        }
    }
    used
}

fn mutated_bytes(
    node: &NodeView<'_>,
    key: &[u8],
    value: Option<&[u8]>,
    exists: bool,
) -> Result<usize, Error> {
    let mut bytes = crate::format::NODE_HEADER_SIZE;
    for index in 0..usize::from(node.header().item_count) {
        let item = node.item(index).ok_or(Error::InvalidNode)?;
        if item.key == key {
            if let Some(new_value) = value {
                bytes += crate::format::SLOT_SIZE + key.len() + new_value.len();
            }
        } else {
            bytes += crate::format::SLOT_SIZE + item.key.len() + item.value.len();
        }
    }
    if !exists {
        if let Some(new_value) = value {
            bytes += crate::format::SLOT_SIZE + key.len() + new_value.len();
        }
    }
    Ok(bytes)
}

fn split_pivot(
    node: &NodeView<'_>,
    key: &[u8],
    value: Option<&[u8]>,
    exists: bool,
) -> Result<usize, Error> {
    let total = mutated_bytes(node, key, value, exists)?;
    let target = total / 2;
    let mut bytes = crate::format::NODE_HEADER_SIZE;
    let mut ordinal = 0usize;
    let insertion = lower_bound(node, key);
    for cursor in 0..=usize::from(node.header().item_count) {
        if cursor == insertion {
            if let Some(new_value) = value {
                bytes += crate::format::SLOT_SIZE + key.len() + new_value.len();
                ordinal += 1;
                if bytes >= target && ordinal != 0 {
                    return Ok(ordinal);
                }
            }
            if exists {
                continue;
            }
        }
        if cursor == usize::from(node.header().item_count) {
            break;
        }
        let item = node.item(cursor).ok_or(Error::InvalidNode)?;
        bytes += crate::format::SLOT_SIZE + item.key.len() + item.value.len();
        ordinal += 1;
        if bytes >= target && ordinal != 0 {
            return Ok(ordinal);
        }
    }
    Err(Error::Capacity)
}

fn updated_internal_bytes(node: &NodeView<'_>, update: ChildUpdate) -> Result<usize, Error> {
    let mut bytes = crate::format::NODE_HEADER_SIZE;
    for cursor in 0..=usize::from(node.header().item_count) {
        if cursor == usize::from(update.start) {
            for child in [update.first, update.second]
                .into_iter()
                .take(usize::from(update.count))
            {
                bytes += crate::format::SLOT_SIZE + child.key().len() + 8;
            }
        }
        if cursor == usize::from(node.header().item_count) {
            break;
        }
        if cursor >= usize::from(update.start)
            && cursor < usize::from(update.start) + usize::from(update.remove)
        {
            continue;
        }
        let item = node.item(cursor).ok_or(Error::InvalidNode)?;
        bytes += crate::format::SLOT_SIZE + item.key.len() + item.value.len();
    }
    Ok(bytes)
}

fn internal_split_pivot(node: &NodeView<'_>, update: ChildUpdate) -> Result<usize, Error> {
    let total = updated_internal_bytes(node, update)?;
    let target = total / 2;
    let mut bytes = crate::format::NODE_HEADER_SIZE;
    let mut ordinal = 0usize;
    for cursor in 0..=usize::from(node.header().item_count) {
        if cursor == usize::from(update.start) {
            for child in [update.first, update.second]
                .into_iter()
                .take(usize::from(update.count))
            {
                bytes += crate::format::SLOT_SIZE + child.key().len() + 8;
                ordinal += 1;
                if bytes >= target {
                    return Ok(ordinal);
                }
            }
        }
        if cursor == usize::from(node.header().item_count) {
            break;
        }
        if cursor >= usize::from(update.start)
            && cursor < usize::from(update.start) + usize::from(update.remove)
        {
            continue;
        }
        let item = node.item(cursor).ok_or(Error::InvalidNode)?;
        bytes += crate::format::SLOT_SIZE + item.key.len() + item.value.len();
        ordinal += 1;
        if bytes >= target {
            return Ok(ordinal);
        }
    }
    Err(Error::Capacity)
}

#[allow(clippy::too_many_arguments)]
fn push_partitioned(
    left: &mut NodeBuilder,
    right: &mut Option<NodeBuilder>,
    pivot: usize,
    ordinal: usize,
    key: &[u8],
    value: &[u8],
    volume_blocks: u64,
    first_left: &mut ChildRef,
    first_right: &mut ChildRef,
    left_block: u64,
    right_block: Option<u64>,
) -> Result<(), Error> {
    if ordinal < pivot {
        if first_left.key_len == 0 {
            *first_left = ChildRef::new(key, left_block)?;
        }
        left.push(key, value, volume_blocks)
    } else {
        let builder = right.as_mut().ok_or(Error::Capacity)?;
        let block = right_block.ok_or(Error::Capacity)?;
        if first_right.key_len == 0 {
            *first_right = ChildRef::new(key, block)?;
        }
        builder.push(key, value, volume_blocks)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::format::{format_empty, object_key, InodeKind, InodeValue, ROOT_COUNT};
    use std::vec;

    const BLOCKS: u64 = 8192;
    const UUID: [u8; 16] = *b"VaraniaTreeTest!";

    struct MemoryDisk {
        blocks: std::vec::Vec<Block>,
        writes: usize,
        flushes: usize,
    }

    impl MemoryDisk {
        fn formatted() -> (Self, Superblock) {
            let empty = format_empty(BLOCKS, UUID).unwrap();
            let mut blocks = vec![[0; BLOCK_SIZE]; BLOCKS as usize];
            blocks[0] = empty.superblock;
            blocks[1] = empty.superblock;
            for (index, root) in empty.roots.into_iter().enumerate() {
                let primary = FIRST_ALLOCATABLE_BLOCK as usize + index * 2;
                blocks[primary] = root;
                blocks[primary + 1] = root;
            }
            let superblock = Superblock::decode(&blocks[0], BLOCKS).unwrap();
            (
                Self {
                    blocks,
                    writes: 0,
                    flushes: 0,
                },
                superblock,
            )
        }
    }

    impl BlockDevice for MemoryDisk {
        fn read(&mut self, block: u64, output: &mut Block) -> Result<(), Error> {
            *output = *self.blocks.get(block as usize).ok_or(Error::Io)?;
            Ok(())
        }

        fn write(&mut self, block: u64, input: &Block) -> Result<(), Error> {
            *self.blocks.get_mut(block as usize).ok_or(Error::Io)? = *input;
            self.writes += 1;
            Ok(())
        }

        fn flush(&mut self) -> Result<(), Error> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn value(generation: u64, size: u64) -> [u8; InodeValue::ENCODED_SIZE] {
        InodeValue {
            generation,
            size,
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

    #[test]
    fn lookup_insert_replace_remove_survive_commit() {
        let (mut disk, initial) = MemoryDisk::formatted();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        transaction
            .insert(TreeKind::Inode, &object_key(2), &value(2, 10))
            .unwrap();
        transaction
            .upsert(TreeKind::Inode, &object_key(2), &value(2, 99))
            .unwrap();
        let mut output = [0; MAX_VALUE_BYTES];
        let found = transaction
            .lookup(TreeKind::Inode, &object_key(2), &mut output)
            .unwrap()
            .unwrap();
        assert_eq!(found.length as usize, InodeValue::ENCODED_SIZE);
        assert_eq!(u64::from_le_bytes(output[8..16].try_into().unwrap()), 99);
        let published = transaction.commit().unwrap();
        assert_eq!(disk.flushes, 3);

        let mut transaction = Transaction::begin(&mut disk, published, &mut workspace).unwrap();
        transaction.remove(TreeKind::Inode, &object_key(2)).unwrap();
        assert_eq!(
            transaction
                .lookup(TreeKind::Inode, &object_key(2), &mut output)
                .unwrap(),
            None
        );
    }

    #[test]
    fn thousands_of_records_split_and_merge_without_losing_order() {
        let (mut disk, mut mounted) = MemoryDisk::formatted();
        let mut workspace = TransactionWorkspace::new();
        for batch in 0..40u64 {
            let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
            for offset in 0..20u64 {
                let object = 2 + batch * 20 + offset;
                transaction
                    .insert(
                        TreeKind::Inode,
                        &object_key(object),
                        &value(batch + 2, object),
                    )
                    .unwrap();
            }
            mounted = transaction.commit().unwrap();
        }
        let mut output = [0; MAX_VALUE_BYTES];
        {
            let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
            for object in 2..802u64 {
                assert!(transaction
                    .lookup(TreeKind::Inode, &object_key(object), &mut output)
                    .unwrap()
                    .is_some());
            }
        }
        for batch in 0..81u64 {
            let begin = 2 + batch * 8;
            let end = (begin + 8).min(650);
            let mut transaction = Transaction::begin(&mut disk, mounted, &mut workspace).unwrap();
            for object in begin..end {
                transaction
                    .remove(TreeKind::Inode, &object_key(object))
                    .unwrap();
            }
            mounted = transaction.commit().unwrap();
        }
        let published = mounted;
        let mut transaction = Transaction::begin(&mut disk, published, &mut workspace).unwrap();
        for object in 2..650u64 {
            assert_eq!(
                transaction
                    .lookup(TreeKind::Inode, &object_key(object), &mut output)
                    .unwrap(),
                None
            );
        }
        for object in 650..802u64 {
            assert!(transaction
                .lookup(TreeKind::Inode, &object_key(object), &mut output)
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn strict_insert_and_remove_report_conflicts() {
        let (mut disk, initial) = MemoryDisk::formatted();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        assert_eq!(
            transaction.remove(TreeKind::Inode, &object_key(77)),
            Err(Error::InvalidArgument)
        );
        transaction
            .insert(TreeKind::Inode, &object_key(77), &value(2, 1))
            .unwrap();
        assert_eq!(
            transaction.insert(TreeKind::Inode, &object_key(77), &value(2, 1)),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn tree_count_constant_stays_part_of_formatted_contract() {
        assert_eq!(ROOT_COUNT, 6);
    }

    #[test]
    fn corrupt_primary_metadata_is_read_from_mirror() {
        let (mut disk, initial) = MemoryDisk::formatted();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        transaction
            .insert(TreeKind::Inode, &object_key(2), &value(2, 42))
            .unwrap();
        let published = transaction.commit().unwrap();
        let primary = published.roots.get(TreeKind::Inode).block as usize;
        disk.blocks[primary][300] ^= 0x40;

        let mut output = [0; MAX_VALUE_BYTES];
        let mut transaction = Transaction::begin(&mut disk, published, &mut workspace).unwrap();
        assert!(transaction
            .lookup(TreeKind::Inode, &object_key(2), &mut output)
            .unwrap()
            .is_some());
        assert_eq!(u64::from_le_bytes(output[8..16].try_into().unwrap()), 42);
    }

    #[test]
    fn fsync_generation_recovers_before_superblock_checkpoint() {
        let (mut disk, initial) = MemoryDisk::formatted();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        transaction
            .insert(TreeKind::Inode, &object_key(2), &value(2, 7))
            .unwrap();
        let durable = transaction.fsync().unwrap();
        assert_eq!(durable.sequence, initial.sequence + 1);
        assert_eq!(disk.flushes, 2);
        let recovered = crate::format::recover_latest(BLOCKS, |number, output| {
            let Some(source) = disk.blocks.get(number as usize) else {
                return false;
            };
            *output = *source;
            true
        })
        .unwrap();
        assert_eq!(recovered.superblock.sequence, durable.sequence);
        assert!(matches!(
            recovered.source,
            crate::format::RecoverySource::IntentLog(_)
        ));
    }
}

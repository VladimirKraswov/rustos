//! Иерархическое namespace API поверх inode/directory B+tree.
//!
//! VFS policy (capabilities, handles, current directory) остаётся в `vfsd`.
//! Здесь нет Unix permissions и строковых абсолютных путей на диске: rename
//! каталога меняет одну directory-запись независимо от размера поддерева.

use crate::{
    format::{
        directory_key, object_key, DirectoryValue, Error, InodeKind, InodeValue, TreeKind,
        MAX_NAME_BYTES, ROOT_OBJECT_ID,
    },
    tree::{BlockDevice, Transaction, Visit, MAX_VALUE_BYTES},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceError {
    Storage(Error),
    InvalidPath,
    NotFound,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
}

impl From<Error> for NamespaceError {
    fn from(value: Error) -> Self {
        Self::Storage(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry {
    pub object_id: u64,
    pub inode: InodeValue,
    pub name_len: u16,
    pub name: [u8; MAX_NAME_BYTES],
}

impl Entry {
    pub const EMPTY: Self = Self {
        object_id: 0,
        inode: InodeValue {
            generation: 1,
            size: 0,
            allocated_blocks: 0,
            created_ns: 0,
            modified_ns: 0,
            content_generation: 1,
            flags: 0,
            kind: InodeKind::File,
        },
        name_len: 0,
        name: [0; MAX_NAME_BYTES],
    };

    pub fn name(&self) -> &[u8] {
        &self.name[..usize::from(self.name_len)]
    }
}

pub fn resolve<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    path: &[u8],
) -> Result<u64, NamespaceError> {
    validate_path(path)?;
    if path == b"/" {
        return Ok(ROOT_OBJECT_ID);
    }
    let mut parent = ROOT_OBJECT_ID;
    for component in path[1..].split(|byte| *byte == b'/') {
        let mut key = [0; 8 + MAX_NAME_BYTES];
        let key = directory_key(parent, component, &mut key)?;
        let mut value = [0; MAX_VALUE_BYTES];
        let found = transaction
            .lookup(TreeKind::Directory, key, &mut value)?
            .ok_or(NamespaceError::NotFound)?;
        parent = DirectoryValue::decode(&value[..usize::from(found.length)])?.object_id;
    }
    Ok(parent)
}

pub fn inode<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    object_id: u64,
) -> Result<InodeValue, NamespaceError> {
    let mut value = [0; MAX_VALUE_BYTES];
    let found = transaction
        .lookup(TreeKind::Inode, &object_key(object_id), &mut value)?
        .ok_or(NamespaceError::NotFound)?;
    Ok(InodeValue::decode(
        &value[..usize::from(found.length)],
        transaction.volume_blocks(),
    )?)
}

pub fn create<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    path: &[u8],
    kind: InodeKind,
    now_ns: u64,
) -> Result<u64, NamespaceError> {
    let (parent_path, name) = split_parent(path)?;
    let parent = resolve(transaction, parent_path)?;
    if inode(transaction, parent)?.kind != InodeKind::Directory {
        return Err(NamespaceError::NotDirectory);
    }
    let mut key_buffer = [0; 8 + MAX_NAME_BYTES];
    let key = directory_key(parent, name, &mut key_buffer)?;
    let mut existing = [0; MAX_VALUE_BYTES];
    if transaction
        .lookup(TreeKind::Directory, key, &mut existing)?
        .is_some()
    {
        return Err(NamespaceError::AlreadyExists);
    }
    let object_id = transaction.allocate_object_id()?;
    let generation = transaction.generation();
    let inode_value = InodeValue {
        generation,
        size: 0,
        allocated_blocks: 0,
        created_ns: now_ns,
        modified_ns: now_ns,
        content_generation: generation,
        flags: 0,
        kind,
    }
    .encode()?;
    transaction.insert(TreeKind::Inode, &object_key(object_id), &inode_value)?;
    let directory_value = DirectoryValue {
        object_id,
        generation,
    }
    .encode()?;
    transaction.insert(TreeKind::Directory, key, &directory_value)?;
    Ok(object_id)
}

pub fn read_dir_at<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    directory: u64,
    ordinal: u64,
) -> Result<Option<Entry>, NamespaceError> {
    if inode(transaction, directory)?.kind != InodeKind::Directory {
        return Err(NamespaceError::NotDirectory);
    }
    let prefix = directory.to_be_bytes();
    let mut current = 0u64;
    let mut found = Entry::EMPTY;
    let mut matched = false;
    transaction.for_each(TreeKind::Directory, |key, value| {
        if key.get(..8) != Some(prefix.as_slice()) {
            return Visit::Continue;
        }
        if current != ordinal {
            current += 1;
            return Visit::Continue;
        }
        let Ok(directory_value) = DirectoryValue::decode(value) else {
            return Visit::Stop;
        };
        let name = &key[8..];
        found.object_id = directory_value.object_id;
        found.name_len = name.len() as u16;
        found.name[..name.len()].copy_from_slice(name);
        matched = true;
        Visit::Stop
    })?;
    if !matched {
        return Ok(None);
    }
    found.inode = inode(transaction, found.object_id)?;
    Ok(Some(found))
}

pub fn rename<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    source: &[u8],
    destination: &[u8],
) -> Result<(), NamespaceError> {
    let (source_parent_path, source_name) = split_parent(source)?;
    let (destination_parent_path, destination_name) = split_parent(destination)?;
    let source_parent = resolve(transaction, source_parent_path)?;
    let destination_parent = resolve(transaction, destination_parent_path)?;
    if inode(transaction, destination_parent)?.kind != InodeKind::Directory {
        return Err(NamespaceError::NotDirectory);
    }
    let mut source_buffer = [0; 8 + MAX_NAME_BYTES];
    let source_key = directory_key(source_parent, source_name, &mut source_buffer)?;
    let mut value = [0; MAX_VALUE_BYTES];
    let found = transaction
        .lookup(TreeKind::Directory, source_key, &mut value)?
        .ok_or(NamespaceError::NotFound)?;
    let directory_value = DirectoryValue::decode(&value[..usize::from(found.length)])?;
    if directory_value.object_id == destination_parent {
        return Err(NamespaceError::InvalidPath);
    }
    let mut destination_buffer = [0; 8 + MAX_NAME_BYTES];
    let destination_key = directory_key(
        destination_parent,
        destination_name,
        &mut destination_buffer,
    )?;
    if transaction
        .lookup(TreeKind::Directory, destination_key, &mut value)?
        .is_some()
    {
        return Err(NamespaceError::AlreadyExists);
    }
    transaction.remove(TreeKind::Directory, source_key)?;
    transaction.insert(
        TreeKind::Directory,
        destination_key,
        &directory_value.encode()?,
    )?;
    Ok(())
}

pub fn unlink<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    path: &[u8],
) -> Result<(), NamespaceError> {
    let (parent_path, name) = split_parent(path)?;
    let parent = resolve(transaction, parent_path)?;
    let mut key_buffer = [0; 8 + MAX_NAME_BYTES];
    let key = directory_key(parent, name, &mut key_buffer)?;
    let mut value = [0; MAX_VALUE_BYTES];
    let found = transaction
        .lookup(TreeKind::Directory, key, &mut value)?
        .ok_or(NamespaceError::NotFound)?;
    let object_id = DirectoryValue::decode(&value[..usize::from(found.length)])?.object_id;
    let target = inode(transaction, object_id)?;
    if target.kind == InodeKind::Directory && read_dir_at(transaction, object_id, 0)?.is_some() {
        return Err(NamespaceError::DirectoryNotEmpty);
    }
    transaction.remove(TreeKind::Directory, key)?;
    transaction.remove(TreeKind::Inode, &object_key(object_id))?;
    Ok(())
}

fn split_parent(path: &[u8]) -> Result<(&[u8], &[u8]), NamespaceError> {
    validate_path(path)?;
    if path == b"/" {
        return Err(NamespaceError::InvalidPath);
    }
    let separator = path
        .iter()
        .rposition(|byte| *byte == b'/')
        .ok_or(NamespaceError::InvalidPath)?;
    let parent = if separator == 0 {
        b"/"
    } else {
        &path[..separator]
    };
    let name = &path[separator + 1..];
    if name.is_empty() {
        return Err(NamespaceError::InvalidPath);
    }
    Ok((parent, name))
}

fn validate_path(path: &[u8]) -> Result<(), NamespaceError> {
    if path == b"/" {
        return Ok(());
    }
    if path.is_empty()
        || path[0] != b'/'
        || path.len() > 4096
        || (path.len() > 1 && path[path.len() - 1] == b'/')
        || path.windows(2).any(|pair| pair == b"//")
        || path[1..].split(|byte| *byte == b'/').any(|component| {
            component.is_empty()
                || component.len() > MAX_NAME_BYTES
                || component == b"."
                || component == b".."
                || component.contains(&0)
        })
    {
        Err(NamespaceError::InvalidPath)
    } else {
        Ok(())
    }
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

    const UUID: [u8; 16] = *b"VaraniaNamesTest";

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
    fn create_resolve_readdir_rename_and_unlink_are_object_based() {
        let (mut disk, initial) = disk();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        let docs = create(
            &mut transaction,
            "/Документы".as_bytes(),
            InodeKind::Directory,
            1,
        )
        .unwrap();
        let file = create(
            &mut transaction,
            "/Документы/заметка.txt".as_bytes(),
            InodeKind::File,
            2,
        )
        .unwrap();
        assert_eq!(
            resolve(&mut transaction, "/Документы/заметка.txt".as_bytes()).unwrap(),
            file
        );
        assert_eq!(
            read_dir_at(&mut transaction, docs, 0)
                .unwrap()
                .unwrap()
                .object_id,
            file
        );
        rename(
            &mut transaction,
            "/Документы/заметка.txt".as_bytes(),
            "/заметка.txt".as_bytes(),
        )
        .unwrap();
        assert_eq!(
            resolve(&mut transaction, "/заметка.txt".as_bytes()).unwrap(),
            file
        );
        unlink(&mut transaction, "/заметка.txt".as_bytes()).unwrap();
        unlink(&mut transaction, "/Документы".as_bytes()).unwrap();
        assert_eq!(
            resolve(&mut transaction, "/Документы".as_bytes()),
            Err(NamespaceError::NotFound)
        );
    }

    #[test]
    fn non_empty_directory_and_noncanonical_paths_are_rejected() {
        let (mut disk, initial) = disk();
        let mut workspace = TransactionWorkspace::new();
        let mut transaction = Transaction::begin(&mut disk, initial, &mut workspace).unwrap();
        create(&mut transaction, b"/a", InodeKind::Directory, 0).unwrap();
        create(&mut transaction, b"/a/b", InodeKind::File, 0).unwrap();
        assert_eq!(
            unlink(&mut transaction, b"/a"),
            Err(NamespaceError::DirectoryNotEmpty)
        );
        assert_eq!(
            resolve(&mut transaction, b"/a//b"),
            Err(NamespaceError::InvalidPath)
        );
        assert_eq!(
            resolve(&mut transaction, b"/a/../b"),
            Err(NamespaceError::InvalidPath)
        );
    }
}

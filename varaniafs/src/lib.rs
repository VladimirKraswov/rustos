//! Надёжная 64-битная файловая система VaraniaFS.
//!
//! Дисковая граница не зависит от Rust layout и декодируется явно. Metadata —
//! mirrored copy-on-write B+tree, данные checksummed, публикация поколения
//! использует intent log и два superblock. Capability policy находится в VFS,
//! поэтому filesystem не дублирует Unix permissions.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub const BLOCK_SIZE: usize = 4096;
pub const MIN_VOLUME_BLOCKS: u64 = 4096;

pub mod allocator;
pub mod file;
pub mod format;
pub mod integrity;
pub mod intent;
pub mod namespace;
pub mod snapshot;
pub mod tree;

#[doc(hidden)]
pub mod experimental_import;

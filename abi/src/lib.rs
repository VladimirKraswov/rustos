//! # rustos-abi
//!
//! Стабильный ABI между загрузчиком, ядром, пользовательским runtime и
//! host-инструментами.
//!
//! ## Правила
//!
//! * Все структуры, пересекающие границу «процесс/ядро» или «загрузчик/ядро»,
//!   помечены `#[repr(C)]` и состоят только из типов фиксированной ширины
//!   (`u8..u64`, `usize` запрещён в ABI).
//! * Каждая структура содержит поле `version` и проходит compile-time
//!   проверки размера (`const _: () = assert!(...)`).
//! * Изменения поля или размера = новая версия ABI; ядро и runtime обязаны
//!   согласовывать версию до первого системного вызова.
//!
//! Crate не имеет зависимостей и компилируется под `no_std` и host.

#![no_std]
#![warn(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod block;
pub mod bootinfo;
pub mod dll;
pub mod handle;
pub mod input;
pub mod ipc;
pub mod memmap;
pub mod memory;
pub mod pipe;
pub mod process;
pub mod syscall;
pub mod ui;
pub mod vfs;
pub mod window;

pub use bootinfo::{BootInfo, BOOT_INFO_MAGIC, BOOT_INFO_VERSION};
pub use handle::{Handle, Rights};
pub use memmap::{MemRegion, MemRegionKind, MEMMAP_MAX_REGIONS};
pub use memory::{SharedMemoryCreate, SharedMemoryMap, VmFlags, VmMapRequest};
pub use process::{ExitReason, PriorityClass, ProcessId, ThreadId};

/// Базовая 4-KiB granule, общая для текущих AMD64 и AArch64 targets.
pub const PAGE_SIZE: u64 = 4096;
/// Размер «крупной» страницы (2 MiB) — единица identity-маппинга ядра.
pub const HUGEPAGE_SIZE: u64 = 2 * 1024 * 1024;

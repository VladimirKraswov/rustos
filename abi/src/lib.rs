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

pub mod bootinfo;
pub mod memmap;

pub use bootinfo::{BootInfo, BOOT_INFO_MAGIC, BOOT_INFO_VERSION};
pub use memmap::{MemRegion, MemRegionKind, MEMMAP_MAX_REGIONS};

/// Размер страницы x86-64 в байтах. Базовая единица виртуальной памяти.
pub const PAGE_SIZE: u64 = 4096;
/// Размер «крупной» страницы (2 MiB) — единица identity-маппинга ядра.
pub const HUGEPAGE_SIZE: u64 = 2 * 1024 * 1024;

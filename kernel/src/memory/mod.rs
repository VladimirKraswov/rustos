//! Управление физическими кадрами и пользовательскими address spaces.
//!
//! Allocator хранит свободные extent'ы, а не bitmap на всю RAM: служебная
//! память зависит от числа фрагментов UEFI map, поэтому конфигурация с
//! терабайтами RAM не создаёт гигантскую таблицу в kernel.

mod address_space;
mod frame;

pub use address_space::{AddressSpace, AddressSpaceError, UserPageBacking, UserPageFlags};
pub use frame::{allocate, free, initialize, stats, FrameAllocatorError, FrameBlock};

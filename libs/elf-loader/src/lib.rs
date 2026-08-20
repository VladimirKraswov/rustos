//! Пользовательский ELF64/DLL loader RustOS.
//!
//! Kernel запускает только маленький статически самодостаточный `ld-rustos`.
//! Всё сложное и потенциально ошибочное — разбор `DT_NEEDED`, таблиц
//! символов, TLS и relocations — остаётся в ring 3. Код не использует heap:
//! ранняя система может загрузить базовые библиотеки до появления allocator.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

mod elf;
mod loader;

pub use elf::{ElfError, ElfView, ProgramFlags, Relocation, Symbol};
pub use loader::{
    DynamicLoader, LoadError, LoadedProgram, Memory, ModuleSource, RuntimeMemory, SearchPolicy,
    SharedRegion, TlsLayout,
};

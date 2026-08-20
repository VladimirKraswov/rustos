//! ISA-specific переход из UEFI в ядро и ранний port I/O.

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(not(target_arch = "x86_64"))]
compile_error!("UEFI boot backend пока реализован только для x86_64; kernel/userspace уже собираются для AArch64");

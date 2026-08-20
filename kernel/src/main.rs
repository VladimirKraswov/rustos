//! Ядро RustOS.
//!
//! Точка входа — [`_start`], которую вызывает platform bootstrap после
//! установки identity-страниц и загрузки стека. До первого прерывания ядро
//! работает на одном CPU без preemption: это допустимо для ранней инициализации
//! (контракт загрузки — docs/ARCHITECTURE.md, раздел «Загрузка»).

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod apps;
mod arch;
mod block;
mod boot;
mod font;
mod fs;
mod graphics;
mod gui;
mod input;
mod memory;
mod panic;
mod process;
mod serial;

use rustos_abi::BootInfo;

/// Входная точка ELF-образа ядра.
///
/// Вызывается загрузчиком в 64-битном kernel privilege level на boot CPU.
/// К моменту вызова гарантировано:
/// * установлен identity-маппинг всей физической памяти и окна MMIO
///   (bootstrap установил CR3 либо TTBR/TCR);
/// * `RSP` указывает на верх boot-стека (см. `BootInfo.boot_stack`);
/// * аргумент `boot_info` — указатель на структуру в памяти, которая
///   находится в зарезервированном диапазоне ядра.
///
/// # Safety
///
/// Контракт устанавливает platform bootstrap (см. `docs/GRUB.md` и
/// `rustos_abi::bootinfo`): указатель валиден до конца
/// работы системы, выравнивание `align_of::<BootInfo>()`, 64-битный режим.
#[no_mangle]
pub unsafe extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    // SAFETY: см. контракт функции: указатель валиден и выровнен.
    let info = unsafe { &*boot_info };
    // UART задаётся BootInfo: COM1 на PC, PL011/MMIO-16550 на ARM-платах.
    serial::init(info);
    // Диагностический маркер: управление передано ядру (до serial::init
    // и чтения BootInfo). Маркер в логе без banner'а — проблема в начале
    // `kernel_main`; отсутствие маркера — проблема на стороне загрузчика
    // (jump/address-space root/стек/entry).
    serial::early_put_str("K0: kernel _start\n");
    boot::kernel_main(info)
}

//! Ядро RustOS.
//!
//! Точка входа — [`_start`], которую вызывает UEFI-загрузчик после
//! установки identity-страниц и загрузки стека. До первого прерывания ядро
//! работает на одном CPU без preemption: это допустимо для ранней инициализации
//! (контракт загрузки — docs/ARCHITECTURE.md, раздел «Загрузка»).

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod apps;
mod arch;
mod boot;
mod font;
mod fs;
mod graphics;
mod gui;
mod input;
mod panic;
mod serial;

use rustos_abi::BootInfo;

/// Входная точка ELF-образа ядра.
///
/// Вызывается загрузчиком в long mode на CPU0. К моменту вызова гарантировано:
/// * установлен identity-маппинг всей физической памяти и окна MMIO
///   (загрузчик записал PGD в CR3);
/// * `RSP` указывает на верх boot-стека (см. `BootInfo.boot_stack`);
/// * аргумент `boot_info` — указатель на структуру в памяти, которая
///   переживает `ExitBootServices` (выделена в резерве ядра).
///
/// # Safety
///
/// Контракт устанавливает загрузчик (см. модуль `rustos-boot`, раздел
/// «Контракт ядра», и `rustos_abi::bootinfo`): указатель валиден до конца
/// работы системы, выравнивание `align_of::<BootInfo>()`, long mode.
#[no_mangle]
pub unsafe extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    // Диагностический маркер: управление передано ядру (до serial::init
    // и чтения BootInfo). Маркер в логе без banner'а — проблема в начале
    // `kernel_main`; отсутствие маркера — проблема на стороне загрузчика
    // (jmp/CR3/стек/entry).
    serial::early_put_str("K0: kernel _start\n");
    // SAFETY: см. контракт функции: указатель валиден, выровнен,
    // long mode, одно CPU на этом этапе (без aliasing-конфликтов).
    let info = unsafe { &*boot_info };
    boot::kernel_main(info)
}

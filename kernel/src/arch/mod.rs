//! Стабильная граница между переносимым микроядром и конкретным CPU.
//!
//! Код выше этого модуля не должен упоминать APIC/GIC, CR3/TTBR, IDT,
//! регистры x86 или system registers Arm. Для каждой поддерживаемой ISA
//! backend экспортирует один и тот же небольшой контракт: ранний запуск,
//! адресные пространства, trap frame, таймер и запуск вторичных ядер.
//!
//! Оба backend'а проходят настоящий boot integration: x86-64 использует
//! APIC/Multiboot2, AArch64 — AAVMF, GICv3, Generic Timer и PSCI. AP на обеих
//! ISA пока запускаются и безопасно паркуются; распределение user threads по
//! нескольким CPU остаётся отдельным per-CPU scheduler milestone.

#![allow(dead_code)]

use core::sync::atomic::{AtomicU64, Ordering};
use rustos_abi::BootInfo;

// Эти C builtins не зависят от ISA и нужны freestanding `core` на каждом
// kernel target. Держим их один раз, вне backend'ов.
mod mem;

/// Архитектурно-независимая классификация входа в ядро.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrapKind {
    /// Системный вызов пользовательского процесса.
    Syscall,
    /// Прерывание планировщика.
    Timer,
    /// Spurious interrupt, который можно безопасно проигнорировать.
    Spurious,
    /// Обычное архитектурное исключение.
    Exception {
        /// Стабильный номер причины внутри конкретной ISA.
        number: u16,
        /// Дополнительные флаги/код ошибки ISA.
        code: u16,
        /// Адрес инструкции, вызвавшей исключение.
        instruction_pointer: u64,
        /// Fault address, когда ISA его предоставляет.
        fault_address: u64,
    },
}

/// Результат ранней настройки privilege levels и таблицы исключений.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EarlyInit {
    /// Вершина стека, используемого при входе из user mode.
    pub kernel_stack_top: u64,
    /// Читаемое имя установленного backend'а исключений.
    pub exception_backend: &'static str,
}

/// Возможности interrupt controller и аппаратного таймера boot CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerHardware {
    pub boot_cpu_id: u32,
    pub counter_hz: u64,
    pub interrupt_controller: &'static str,
    pub timer: &'static str,
}

/// Результат запуска вторичных процессоров.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmpInfo {
    pub online_cpus: usize,
    pub discovered_cpus: usize,
    pub discovery: &'static str,
}

/// Ошибка аппаратного backend'а, пригодная для общего boot path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchError {
    Unsupported,
    InterruptController,
    FirmwareDescription,
    SecondaryCpuStartup,
}

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("RustOS kernel currently supports only x86_64 and aarch64");

/// Частота timer counter, сохранённая при инициализации.
static COUNTER_HZ: AtomicU64 = AtomicU64::new(1);

/// Возвращает частоту timer counter (Гц).
pub fn counter_frequency() -> u64 {
    COUNTER_HZ.load(Ordering::Acquire)
}

/// Сохраняет частоту timer counter при инициализации.
pub fn set_counter_frequency(hz: u64) {
    COUNTER_HZ.store(hz.max(1), Ordering::Release);
}

/// Единая сигнатура запуска CPU backend'а.
pub type EarlyInitializer = fn(&BootInfo) -> Result<EarlyInit, ArchError>;

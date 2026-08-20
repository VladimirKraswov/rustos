//! Стабильная граница между переносимым микроядром и конкретным CPU.
//!
//! Код выше этого модуля не должен упоминать APIC/GIC, CR3/TTBR, IDT,
//! регистры x86 или system registers Arm. Для каждой поддерживаемой ISA
//! backend экспортирует один и тот же небольшой контракт: ранний запуск,
//! адресные пространства, trap frame, таймер и запуск вторичных ядер.
//!
//! Сейчас x86-64 backend полностью работает и проходит boot/GUI-тесты.
//! AArch64 backend является собираемым porting target: в нём уже определены
//! ABI контекста, системный вызов, MMU-примитивы и PSCI shutdown; подключение
//! GIC/Generic Timer и конкретного загрузчика выполняется следующим этапом.

#![allow(dead_code)]

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

/// Единая сигнатура запуска CPU backend'а.
pub type EarlyInitializer = fn(&BootInfo) -> Result<EarlyInit, ArchError>;

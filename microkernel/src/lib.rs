#![no_std]

//! Независимая от ISA часть микроядра.
//!
//! Здесь намеренно нет allocator'а и платформенного кода. Таблицы имеют
//! фиксированную ёмкость, поэтому те же state machines можно гонять обычными
//! unit-тестами на macOS/Linux и использовать из freestanding kernel.

mod ipc;
mod process_table;
mod scheduler;
mod supervisor;

pub use ipc::{
    derive_capability_rights, prepare_message, CapabilityTransferError, EndpointQueue,
    IpcQueueError,
};
pub use process_table::{ProcessError, ProcessInfo, ProcessState, ProcessTable};
pub use scheduler::{Scheduler, SchedulerError, ThreadInfo, ThreadState, DRIVER_BURST_LIMIT};
pub use supervisor::{RestartDecision, RestartPolicy, SupervisorState};

use rustos_abi::{ExitReason, PriorityClass};

/// Короткий инвариантный тест, выполняемый ещё и при каждой загрузке ядра.
/// Полные варианты находятся в host unit tests ниже.
pub fn boot_self_test() -> bool {
    let mut processes = ProcessTable::<8>::new();
    let Ok(driver_process) = processes.create(rustos_abi::ProcessId::KERNEL) else {
        return false;
    };
    let Ok(application_process) = processes.create(rustos_abi::ProcessId::KERNEL) else {
        return false;
    };
    let mut scheduler = Scheduler::<16, 2>::new();
    let Ok(driver) = scheduler.spawn(driver_process, PriorityClass::Driver, 0b11) else {
        return false;
    };
    let Ok(application) = scheduler.spawn(application_process, PriorityClass::Interactive, 0b01)
    else {
        return false;
    };

    for _ in 0..DRIVER_BURST_LIMIT {
        if scheduler.schedule(0) != Ok(Some(driver)) {
            return false;
        }
    }
    if scheduler.schedule(0) != Ok(Some(application)) {
        return false;
    }

    let fault = ExitReason {
        status: -1,
        exception: 14,
        flags: 0,
        fault_address: 0xdead_beef,
    };
    if scheduler.terminate_process(driver_process, fault) != 1 {
        return false;
    }
    scheduler.schedule(0) == Ok(Some(application))
}

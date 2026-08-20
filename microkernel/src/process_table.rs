//! Жизненный цикл процессов без привязки к allocator'у и архитектуре CPU.

use rustos_abi::{ExitReason, ProcessId};

const NO_EXIT: ExitReason = ExitReason {
    status: 0,
    exception: 0,
    flags: 0,
    fault_address: 0,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessState {
    Free,
    Alive,
    Zombie,
}

#[derive(Clone, Copy)]
struct ProcessSlot {
    generation: u32,
    state: ProcessState,
    parent: ProcessId,
    exit_reason: ExitReason,
}

const EMPTY_SLOT: ProcessSlot = ProcessSlot {
    generation: 1,
    state: ProcessState::Free,
    parent: ProcessId::KERNEL,
    exit_reason: NO_EXIT,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessInfo {
    pub id: ProcessId,
    pub parent: ProcessId,
    pub state: ProcessState,
    pub exit_reason: ExitReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessError {
    TableFull,
    InvalidProcess,
    InvalidState,
}

/// Slot 0 зарезервирован kernel'у. После reap generation увеличивается, так
/// что запоздавший IPC/restart event со старым PID безопасно отклоняется.
pub struct ProcessTable<const N: usize> {
    slots: [ProcessSlot; N],
}

impl<const N: usize> Default for ProcessTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ProcessTable<N> {
    pub const fn new() -> Self {
        Self {
            slots: [EMPTY_SLOT; N],
        }
    }

    pub fn create(&mut self, parent: ProcessId) -> Result<ProcessId, ProcessError> {
        for index in 1..N {
            let slot = &mut self.slots[index];
            if slot.state == ProcessState::Free {
                slot.state = ProcessState::Alive;
                slot.parent = parent;
                slot.exit_reason = NO_EXIT;
                return Ok(ProcessId::new(index as u32, slot.generation));
            }
        }
        Err(ProcessError::TableFull)
    }

    pub fn exit(&mut self, id: ProcessId, reason: ExitReason) -> Result<(), ProcessError> {
        let index = self.validate(id)?;
        if self.slots[index].state != ProcessState::Alive {
            return Err(ProcessError::InvalidState);
        }
        self.slots[index].state = ProcessState::Zombie;
        self.slots[index].exit_reason = reason;
        Ok(())
    }

    /// Возвращает exit status supervisor'у и делает slot доступным повторно.
    pub fn reap(&mut self, id: ProcessId) -> Result<ExitReason, ProcessError> {
        let index = self.validate(id)?;
        let slot = &mut self.slots[index];
        if slot.state != ProcessState::Zombie {
            return Err(ProcessError::InvalidState);
        }
        let reason = slot.exit_reason;
        slot.state = ProcessState::Free;
        slot.parent = ProcessId::KERNEL;
        slot.exit_reason = NO_EXIT;
        slot.generation = next_generation(slot.generation);
        Ok(reason)
    }

    pub fn info(&self, id: ProcessId) -> Result<ProcessInfo, ProcessError> {
        let index = self.validate(id)?;
        let slot = self.slots[index];
        Ok(ProcessInfo {
            id,
            parent: slot.parent,
            state: slot.state,
            exit_reason: slot.exit_reason,
        })
    }

    fn validate(&self, id: ProcessId) -> Result<usize, ProcessError> {
        let index = id.slot() as usize;
        let Some(slot) = self.slots.get(index) else {
            return Err(ProcessError::InvalidProcess);
        };
        if index == 0 || slot.state == ProcessState::Free || slot.generation != id.generation() {
            return Err(ProcessError::InvalidProcess);
        }
        Ok(index)
    }
}

const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_pid_is_rejected_after_reap() {
        let mut table = ProcessTable::<3>::new();
        let first = table.create(ProcessId::KERNEL).unwrap();
        table.exit(first, NO_EXIT).unwrap();
        table.reap(first).unwrap();
        let second = table.create(ProcessId::KERNEL).unwrap();
        assert_eq!(first.slot(), second.slot());
        assert_ne!(first.generation(), second.generation());
        assert_eq!(table.info(first), Err(ProcessError::InvalidProcess));
    }
}

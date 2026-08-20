//! Приоритетный SMP-aware scheduler state machine.
//!
//! x86 layer вызывает `schedule()` из local APIC timer, syscall yield и
//! block/exit paths. Сама state machine не зависит от механизма сохранения
//! регистров, поэтому отдельно проверяется быстрыми host unit-тестами.

use rustos_abi::{ExitReason, PriorityClass, ProcessId, ThreadId};

/// После стольких driver quanta готовый поток менее срочного класса получает
/// один квант. Это сохраняет низкую latency драйверов, но не позволяет
/// неисправному driver worker навсегда остановить supervisor или GUI.
pub const DRIVER_BURST_LIMIT: u8 = 8;
const NO_CPU: u16 = u16::MAX;
const NO_EXIT: ExitReason = ExitReason {
    status: 0,
    exception: 0,
    flags: 0,
    fault_address: 0,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    Free,
    Ready,
    Running,
    Blocked,
    Exited,
}

#[derive(Clone, Copy)]
struct ThreadSlot {
    generation: u32,
    process: ProcessId,
    state: ThreadState,
    priority: PriorityClass,
    affinity: u64,
    running_cpu: u16,
    last_run: u64,
    exit_reason: ExitReason,
}

const EMPTY_THREAD: ThreadSlot = ThreadSlot {
    generation: 1,
    process: ProcessId::KERNEL,
    state: ThreadState::Free,
    priority: PriorityClass::Idle,
    affinity: 0,
    running_cpu: NO_CPU,
    last_run: 0,
    exit_reason: NO_EXIT,
};

#[derive(Clone, Copy)]
struct CoreState {
    current: ThreadId,
    driver_quanta: u8,
}

const EMPTY_CORE: CoreState = CoreState {
    current: ThreadId::INVALID,
    driver_quanta: 0,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub process: ProcessId,
    pub state: ThreadState,
    pub priority: PriorityClass,
    pub affinity: u64,
    pub running_cpu: Option<u16>,
    pub exit_reason: ExitReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerError {
    TableFull,
    InvalidThread,
    InvalidState,
    InvalidCore,
    InvalidAffinity,
}

/// `N` задаёт максимум потоков, `C` — максимум логических CPU. Реальная
/// конфигурация первой версии ограничена 64 CPU одной affinity mask; большие
/// машины позже получат иерархические CPU sets без изменения ThreadId.
pub struct Scheduler<const N: usize, const C: usize> {
    threads: [ThreadSlot; N],
    cores: [CoreState; C],
    epoch: u64,
}

impl<const N: usize, const C: usize> Default for Scheduler<N, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const C: usize> Scheduler<N, C> {
    pub const fn new() -> Self {
        Self {
            threads: [EMPTY_THREAD; N],
            cores: [EMPTY_CORE; C],
            epoch: 0,
        }
    }

    pub fn spawn(
        &mut self,
        process: ProcessId,
        priority: PriorityClass,
        affinity: u64,
    ) -> Result<ThreadId, SchedulerError> {
        if !valid_affinity::<C>(affinity) {
            return Err(SchedulerError::InvalidAffinity);
        }
        for index in 0..N {
            let slot = &mut self.threads[index];
            if slot.state == ThreadState::Free {
                slot.process = process;
                slot.state = ThreadState::Ready;
                slot.priority = priority;
                slot.affinity = affinity;
                slot.running_cpu = NO_CPU;
                slot.last_run = 0;
                slot.exit_reason = NO_EXIT;
                return Ok(ThreadId::new(index as u32, slot.generation));
            }
        }
        Err(SchedulerError::TableFull)
    }

    /// Возвращает текущий поток в Ready, выбирает следующий с учётом CPU
    /// affinity и помечает его Running. На одном классе работает round-robin
    /// по монотонному `last_run`.
    pub fn schedule(&mut self, cpu: usize) -> Result<Option<ThreadId>, SchedulerError> {
        if cpu >= C || cpu >= 64 {
            return Err(SchedulerError::InvalidCore);
        }
        self.preempt_current(cpu);

        let Some(priority) = self.choose_priority(cpu) else {
            self.cores[cpu].current = ThreadId::INVALID;
            return Ok(None);
        };
        let Some(index) = self.pick_oldest(cpu, priority) else {
            return Ok(None);
        };

        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.epoch = 1;
        }
        let id = ThreadId::new(index as u32, self.threads[index].generation);
        let slot = &mut self.threads[index];
        slot.state = ThreadState::Running;
        slot.running_cpu = cpu as u16;
        slot.last_run = self.epoch;
        self.cores[cpu].current = id;
        if priority == PriorityClass::Driver {
            self.cores[cpu].driver_quanta = self.cores[cpu].driver_quanta.saturating_add(1);
        } else if priority != PriorityClass::Kernel {
            self.cores[cpu].driver_quanta = 0;
        }
        Ok(Some(id))
    }

    pub fn block(&mut self, id: ThreadId) -> Result<(), SchedulerError> {
        let index = self.validate(id)?;
        match self.threads[index].state {
            ThreadState::Ready | ThreadState::Running => {
                self.clear_from_core(id);
                self.threads[index].state = ThreadState::Blocked;
                self.threads[index].running_cpu = NO_CPU;
                Ok(())
            }
            _ => Err(SchedulerError::InvalidState),
        }
    }

    pub fn wake(&mut self, id: ThreadId) -> Result<(), SchedulerError> {
        let index = self.validate(id)?;
        if self.threads[index].state != ThreadState::Blocked {
            return Err(SchedulerError::InvalidState);
        }
        self.threads[index].state = ThreadState::Ready;
        Ok(())
    }

    pub fn exit(&mut self, id: ThreadId, reason: ExitReason) -> Result<(), SchedulerError> {
        let index = self.validate(id)?;
        self.finish(index, reason)
    }

    /// Завершает только потоки указанного процесса. Остальные процессы и
    /// scheduler продолжают работу — это основная граница fault containment.
    pub fn terminate_process(&mut self, process: ProcessId, reason: ExitReason) -> usize {
        let mut terminated = 0;
        for index in 0..N {
            let state = self.threads[index].state;
            if self.threads[index].process == process
                && matches!(
                    state,
                    ThreadState::Ready | ThreadState::Running | ThreadState::Blocked
                )
            {
                let _ = self.finish(index, reason);
                terminated += 1;
            }
        }
        terminated
    }

    /// Reap отделён от exit: supervisor успевает прочитать причину завершения.
    pub fn reap(&mut self, id: ThreadId) -> Result<ExitReason, SchedulerError> {
        let index = self.validate(id)?;
        let slot = &mut self.threads[index];
        if slot.state != ThreadState::Exited {
            return Err(SchedulerError::InvalidState);
        }
        let reason = slot.exit_reason;
        let generation = next_generation(slot.generation);
        *slot = EMPTY_THREAD;
        slot.generation = generation;
        Ok(reason)
    }

    pub fn set_affinity(&mut self, id: ThreadId, affinity: u64) -> Result<(), SchedulerError> {
        if !valid_affinity::<C>(affinity) {
            return Err(SchedulerError::InvalidAffinity);
        }
        let index = self.validate(id)?;
        let running_cpu = self.threads[index].running_cpu;
        self.threads[index].affinity = affinity;
        if running_cpu != NO_CPU && affinity & (1u64 << running_cpu) == 0 {
            self.clear_from_core(id);
            self.threads[index].state = ThreadState::Ready;
            self.threads[index].running_cpu = NO_CPU;
        }
        Ok(())
    }

    pub fn set_priority(
        &mut self,
        id: ThreadId,
        priority: PriorityClass,
    ) -> Result<(), SchedulerError> {
        let index = self.validate(id)?;
        self.threads[index].priority = priority;
        Ok(())
    }

    pub fn info(&self, id: ThreadId) -> Result<ThreadInfo, SchedulerError> {
        let index = self.validate(id)?;
        let slot = self.threads[index];
        Ok(ThreadInfo {
            id,
            process: slot.process,
            state: slot.state,
            priority: slot.priority,
            affinity: slot.affinity,
            running_cpu: (slot.running_cpu != NO_CPU).then_some(slot.running_cpu),
            exit_reason: slot.exit_reason,
        })
    }

    fn preempt_current(&mut self, cpu: usize) {
        let current = self.cores[cpu].current;
        let index = current.slot() as usize;
        if let Some(slot) = self.threads.get_mut(index) {
            if current != ThreadId::INVALID
                && slot.generation == current.generation()
                && slot.state == ThreadState::Running
                && slot.running_cpu == cpu as u16
            {
                slot.state = ThreadState::Ready;
                slot.running_cpu = NO_CPU;
            }
        }
        self.cores[cpu].current = ThreadId::INVALID;
    }

    fn choose_priority(&self, cpu: usize) -> Option<PriorityClass> {
        if self.has_ready(cpu, PriorityClass::Kernel) {
            return Some(PriorityClass::Kernel);
        }
        let has_driver = self.has_ready(cpu, PriorityClass::Driver);
        let lower = [
            PriorityClass::System,
            PriorityClass::Interactive,
            PriorityClass::Batch,
            PriorityClass::Idle,
        ]
        .into_iter()
        .find(|priority| self.has_ready(cpu, *priority));
        if has_driver && (self.cores[cpu].driver_quanta < DRIVER_BURST_LIMIT || lower.is_none()) {
            Some(PriorityClass::Driver)
        } else {
            lower
        }
    }

    fn has_ready(&self, cpu: usize, priority: PriorityClass) -> bool {
        self.threads.iter().any(|slot| {
            slot.state == ThreadState::Ready
                && slot.priority == priority
                && slot.affinity & (1u64 << cpu) != 0
        })
    }

    fn pick_oldest(&self, cpu: usize, priority: PriorityClass) -> Option<usize> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.state == ThreadState::Ready
                    && slot.priority == priority
                    && slot.affinity & (1u64 << cpu) != 0
            })
            .min_by_key(|(index, slot)| (slot.last_run, *index))
            .map(|(index, _)| index)
    }

    fn finish(&mut self, index: usize, reason: ExitReason) -> Result<(), SchedulerError> {
        if !matches!(
            self.threads[index].state,
            ThreadState::Ready | ThreadState::Running | ThreadState::Blocked
        ) {
            return Err(SchedulerError::InvalidState);
        }
        let id = ThreadId::new(index as u32, self.threads[index].generation);
        self.clear_from_core(id);
        self.threads[index].state = ThreadState::Exited;
        self.threads[index].running_cpu = NO_CPU;
        self.threads[index].exit_reason = reason;
        Ok(())
    }

    fn clear_from_core(&mut self, id: ThreadId) {
        for core in &mut self.cores {
            if core.current == id {
                core.current = ThreadId::INVALID;
            }
        }
    }

    fn validate(&self, id: ThreadId) -> Result<usize, SchedulerError> {
        let index = id.slot() as usize;
        let Some(slot) = self.threads.get(index) else {
            return Err(SchedulerError::InvalidThread);
        };
        if id == ThreadId::INVALID
            || slot.state == ThreadState::Free
            || slot.generation != id.generation()
        {
            return Err(SchedulerError::InvalidThread);
        }
        Ok(index)
    }
}

fn valid_affinity<const C: usize>(affinity: u64) -> bool {
    if affinity == 0 || C == 0 || C > 64 {
        return false;
    }
    let valid_bits = if C == 64 { u64::MAX } else { (1u64 << C) - 1 };
    affinity & !valid_bits == 0
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

    const P1: ProcessId = ProcessId::new(1, 1);
    const P2: ProcessId = ProcessId::new(2, 1);

    fn fault() -> ExitReason {
        ExitReason {
            status: -1,
            exception: 14,
            flags: 0,
            fault_address: 0x1234,
        }
    }

    #[test]
    fn driver_has_priority_but_cannot_starve_system_service() {
        let mut scheduler = Scheduler::<8, 1>::new();
        let driver = scheduler.spawn(P1, PriorityClass::Driver, 1).unwrap();
        let system = scheduler.spawn(P2, PriorityClass::System, 1).unwrap();
        for _ in 0..DRIVER_BURST_LIMIT {
            assert_eq!(scheduler.schedule(0), Ok(Some(driver)));
        }
        assert_eq!(scheduler.schedule(0), Ok(Some(system)));
        assert_eq!(scheduler.schedule(0), Ok(Some(driver)));
    }

    #[test]
    fn round_robin_and_affinity_work_on_two_cores() {
        let mut scheduler = Scheduler::<8, 2>::new();
        let first = scheduler
            .spawn(P1, PriorityClass::Interactive, 0b01)
            .unwrap();
        let second = scheduler
            .spawn(P2, PriorityClass::Interactive, 0b10)
            .unwrap();
        assert_eq!(scheduler.schedule(0), Ok(Some(first)));
        assert_eq!(scheduler.schedule(1), Ok(Some(second)));
        assert_eq!(scheduler.set_affinity(first, 0b10), Ok(()));
        assert_eq!(scheduler.schedule(0), Ok(None));
    }

    #[test]
    fn process_fault_does_not_stop_another_process() {
        let mut scheduler = Scheduler::<8, 1>::new();
        let broken = scheduler.spawn(P1, PriorityClass::Interactive, 1).unwrap();
        let healthy = scheduler.spawn(P2, PriorityClass::Interactive, 1).unwrap();
        assert_eq!(scheduler.schedule(0), Ok(Some(broken)));
        assert_eq!(scheduler.terminate_process(P1, fault()), 1);
        assert_eq!(scheduler.schedule(0), Ok(Some(healthy)));
    }

    #[test]
    fn stale_tid_is_rejected_after_reap() {
        let mut scheduler = Scheduler::<1, 1>::new();
        let old = scheduler.spawn(P1, PriorityClass::Batch, 1).unwrap();
        scheduler.exit(old, fault()).unwrap();
        scheduler.reap(old).unwrap();
        let new = scheduler.spawn(P2, PriorityClass::Batch, 1).unwrap();
        assert_ne!(old.generation(), new.generation());
        assert_eq!(scheduler.info(old), Err(SchedulerError::InvalidThread));
    }
}

//! Стабильные идентификаторы и состояния процессов/потоков.
//!
//! PID/TID состоят из slot и generation. Повторное использование slot не
//! делает старый идентификатор валидным — это важно для supervisor и IPC:
//! отложенное сообщение не должно случайно попасть новому процессу.

/// Идентификатор процесса: младшие 32 бита — slot, старшие — generation.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProcessId(pub u64);

impl ProcessId {
    /// Системный bootstrap/отсутствующий процесс.
    pub const KERNEL: Self = Self(0);

    /// Создаёт идентификатор. Generation 0 зарезервирован kernel'у.
    pub const fn new(slot: u32, generation: u32) -> Self {
        Self(((generation as u64) << 32) | slot as u64)
    }

    /// Индекс записи process table.
    pub const fn slot(self) -> u32 {
        self.0 as u32
    }

    /// Поколение записи, меняющееся при повторном использовании slot.
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

/// Идентификатор потока с той же защитой от stale references, что и PID.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ThreadId(pub u64);

impl ThreadId {
    /// Невалидный/отсутствующий поток.
    pub const INVALID: Self = Self(0);

    /// Создаёт TID из slot и generation.
    pub const fn new(slot: u32, generation: u32) -> Self {
        Self(((generation as u64) << 32) | slot as u64)
    }

    /// Индекс записи thread table.
    pub const fn slot(self) -> u32 {
        self.0 as u32
    }

    /// Поколение записи thread table.
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

/// Класс планирования. Меньшее числовое значение означает более срочную
/// работу, но каждый класс имеет бюджет: driver не может навсегда вытеснить
/// supervisor и системные сервисы.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PriorityClass {
    /// Короткие kernel critical sections; пользовательским потокам недоступен.
    Kernel = 0,
    /// IRQ/DMA workers user-space драйверов.
    Driver = 1,
    /// VFS, display, supervisor и другие системные серверы.
    System = 2,
    /// Активное GUI/terminal приложение.
    Interactive = 3,
    /// Компиляция, indexer и прочая фоновая работа.
    Batch = 4,
    /// Работа только при отсутствии остальных runnable потоков.
    Idle = 5,
}

/// Причина завершения, которую kernel передаёт supervisor'у.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitReason {
    /// Обычный код `process_exit` либо отрицательный системный status.
    pub status: i32,
    /// Ноль при обычном exit; иначе номер exception/Exception Class текущей ISA.
    pub exception: u16,
    /// Зарезервировано для совместимого расширения.
    pub flags: u16,
    /// Fault address (#PF CR2) либо instruction pointer для других fault.
    pub fault_address: u64,
}

const _: () = assert!(core::mem::size_of::<ProcessId>() == 8);
const _: () = assert!(core::mem::size_of::<ThreadId>() == 8);
const _: () = assert!(core::mem::size_of::<ExitReason>() == 16);

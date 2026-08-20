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

/// Версия структур запуска процесса и потока.
pub const PROCESS_ABI_VERSION: u32 = 1;
/// Фиксированный адрес read-only bootstrap-блока в новом процессе.
pub const PROCESS_START_INFO_ADDRESS: u64 = 0x0000_3fff_ffff_0000;
/// Максимум capabilities, явно передаваемых одним `process_spawn`.
pub const PROCESS_SPAWN_MAX_CAPABILITIES: usize = 8;

/// Описание производного capability для нового процесса.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpawnCapability {
    /// Handle в таблице родителя.
    pub source: crate::Handle,
    /// Желаемый slot в таблице дочернего процесса; ноль запрещён.
    pub target_slot: u32,
    /// Права дочернего capability, являющиеся подмножеством исходных.
    pub rights: crate::Rights,
}

/// Запрос динамического запуска ELF64-процесса.
///
/// `arguments` и `environment` — UTF-8 строки, разделённые `NUL`. Последний
/// элемент также обязан завершаться `NUL`; пустая таблица имеет длину ноль.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ProcessSpawnRequest {
    /// [`PROCESS_ABI_VERSION`].
    pub version: u32,
    /// Зарезервированные флаги; пока должны быть нулём.
    pub flags: u32,
    /// Адрес UTF-8 пути в памяти родителя.
    pub path_address: u64,
    /// Длина пути без завершающего `NUL`.
    pub path_length: u32,
    /// [`PriorityClass`] первого потока.
    pub priority: u8,
    /// Зарезервировано, должно быть нулём.
    pub reserved0: [u8; 3],
    /// Адрес NUL-разделённой таблицы аргументов.
    pub arguments_address: u64,
    /// Размер таблицы аргументов в байтах.
    pub arguments_length: u32,
    /// Число аргументов в таблице.
    pub argument_count: u32,
    /// Адрес NUL-разделённой таблицы environment.
    pub environment_address: u64,
    /// Размер таблицы environment в байтах.
    pub environment_length: u32,
    /// Число переменных environment.
    pub environment_count: u32,
    /// Адрес массива [`SpawnCapability`] в памяти родителя.
    pub capabilities_address: u64,
    /// Число элементов массива capabilities.
    pub capability_count: u32,
    /// VFS namespace capability с правами READ и EXECUTE.
    pub namespace: crate::Handle,
}

/// Результат успешного `process_spawn`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ProcessSpawnResult {
    /// Capability процесса с правами WAIT и DESTROY.
    pub process: crate::Handle,
    /// Зарезервировано для выравнивания.
    pub reserved: u32,
    /// Диагностический PID; управление всегда выполняется через `process`.
    pub pid: ProcessId,
}

/// Read-only описание среды, которое kernel отображает новому процессу.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ProcessStartInfo {
    /// [`PROCESS_ABI_VERSION`].
    pub version: u32,
    /// Полный размер заголовка для совместимого расширения.
    pub size: u32,
    /// Идентификатор процесса.
    pub pid: ProcessId,
    /// Идентификатор начального потока.
    pub tid: ThreadId,
    /// Размер базовой страницы target-платформы.
    pub page_size: u64,
    /// Частота аппаратного монотонного counter.
    pub monotonic_hz: u64,
    /// Адрес таблицы аргументов внутри этого address space.
    pub arguments_address: u64,
    /// Размер таблицы аргументов в байтах.
    pub arguments_length: u32,
    /// Количество аргументов.
    pub argument_count: u32,
    /// Адрес таблицы environment внутри этого address space.
    pub environment_address: u64,
    /// Размер таблицы environment в байтах.
    pub environment_length: u32,
    /// Количество переменных environment.
    pub environment_count: u32,
}

/// Запрос создания потока в текущем address space.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ThreadCreateRequest {
    /// [`PROCESS_ABI_VERSION`].
    pub version: u32,
    /// Зарезервировано, должно быть нулём.
    pub flags: u32,
    /// Исполняемый user address точки входа.
    pub entry: u64,
    /// Вершина заранее отображённого writable стека.
    pub stack_pointer: u64,
    /// Первый машинный аргумент новой точки входа.
    pub argument: u64,
    /// Thread pointer: FS base на AMD64, TPIDR_EL0 на AArch64.
    pub thread_pointer: u64,
    /// [`PriorityClass`] потока.
    pub priority: u8,
    /// Зарезервировано, должно быть нулём.
    pub reserved: [u8; 7],
}

/// Результат успешного `thread_create`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ThreadCreateResult {
    /// Capability потока с правом WAIT.
    pub thread: crate::Handle,
    /// Зарезервировано для выравнивания.
    pub reserved: u32,
    /// Диагностический TID; join выполняется через `thread`.
    pub tid: ThreadId,
}

const _: () = assert!(core::mem::size_of::<ProcessId>() == 8);
const _: () = assert!(core::mem::size_of::<ThreadId>() == 8);
const _: () = assert!(core::mem::size_of::<ExitReason>() == 16);
const _: () = assert!(core::mem::size_of::<SpawnCapability>() == 16);
const _: () = assert!(core::mem::size_of::<ProcessSpawnRequest>() == 72);
const _: () = assert!(core::mem::size_of::<ProcessSpawnResult>() == 16);
const _: () = assert!(core::mem::size_of::<ProcessStartInfo>() == 72);
const _: () = assert!(core::mem::size_of::<ThreadCreateRequest>() == 48);
const _: () = assert!(core::mem::size_of::<ThreadCreateResult>() == 16);

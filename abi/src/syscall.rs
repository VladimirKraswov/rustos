//! Минимальный syscall ABI между ring 3 и микроядром.
//!
//! Номера операций и структуры общие для всех ISA. AMD64 bootstrap использует
//! `int 0x80` (RAX + RDI/RSI/RDX), AArch64 — `svc #0` (x8 + x0/x1/x2).
//! Различие скрывает `rustos-runtime`, поэтому исходный код приложений общий.

/// Номер software interrupt AMD64; на AArch64 непосредственное значение
/// инструкции `svc` равно нулю.
pub const INTERRUPT_VECTOR: u8 = 0x80;
/// Версия syscall ABI, передаваемая вторым bootstrap-аргументом.
pub const ABI_VERSION: u64 = 8;

/// Номера операций (`RAX`).
pub mod number {
    /// Добровольно отдать остаток quantum.
    pub const THREAD_YIELD: u64 = 0;
    /// Завершить текущий процесс (`RDI = status`).
    pub const PROCESS_EXIT: u64 = 1;
    /// Получить размер объекта относительно VFS capability.
    /// `RDI=handle, RSI=path, RDX=len`; результат — размер либо status < 0.
    pub const VFS_STAT: u64 = 2;
    /// Отправить [`crate::ipc::Message`] в endpoint.
    /// `RDI=endpoint handle, RSI=*const Message`.
    pub const IPC_SEND: u64 = 3;
    /// Получить сообщение или блокировать текущий поток до его появления.
    /// `RDI=endpoint handle, RSI=*mut Message`.
    pub const IPC_RECEIVE: u64 = 4;
    /// Создать ELF64-процесс из доступного вызывающему VFS namespace.
    /// `arg0=*const ProcessSpawnRequest, arg1=*mut ProcessSpawnResult`.
    pub const PROCESS_SPAWN: u64 = 5;
    /// Ждать завершения процесса. `arg0=process handle, arg1=*mut ExitReason`.
    pub const PROCESS_WAIT: u64 = 6;
    /// Завершить процесс. `arg0=process handle, arg1=status`.
    pub const PROCESS_KILL: u64 = 7;
    /// Создать поток в текущем address space.
    /// `arg0=*const ThreadCreateRequest, arg1=*mut ThreadCreateResult`.
    pub const THREAD_CREATE: u64 = 8;
    /// Завершить только вызывающий поток (`arg0=status`).
    pub const THREAD_EXIT: u64 = 9;
    /// Ждать завершения потока. `arg0=thread handle, arg1=*mut ExitReason`.
    pub const THREAD_JOIN: u64 = 10;
    /// Изменить thread pointer/TLS текущего потока (`arg0=address`).
    pub const THREAD_SET_TLS: u64 = 11;
    /// Отобразить новые анонимные zero-filled страницы.
    /// `arg0=*const VmMapRequest`; результат — virtual address.
    pub const VM_MAP: u64 = 12;
    /// Удалить отображение (`arg0=address, arg1=length`).
    pub const VM_UNMAP: u64 = 13;
    /// Изменить права отображения (`arg0=address, arg1=length, arg2=VmFlags`).
    pub const VM_PROTECT: u64 = 14;
    /// Создать объект разделяемой памяти.
    /// `arg0=*const SharedMemoryCreate`; результат — capability handle.
    pub const SHARED_MEMORY_CREATE: u64 = 15;
    /// Отобразить shared-memory capability.
    /// `arg0=handle, arg1=*const SharedMemoryMap`; результат — virtual address.
    pub const SHARED_MEMORY_MAP: u64 = 16;
    /// Закрыть capability handle (`arg0=handle`).
    pub const HANDLE_CLOSE: u64 = 17;
    /// Монотонное время в наносекундах от аппаратной эпохи.
    pub const CLOCK_MONOTONIC: u64 = 18;
    /// Размер block device в логических 4-KiB блоках (`arg0=handle`).
    pub const BLOCK_GET_SIZE: u64 = 19;
    /// Прочитать один или несколько блоков (`arg0=handle, arg1=*BlockIoRequest`).
    pub const BLOCK_READ: u64 = 20;
    /// Записать блоки (`arg0=handle, arg1=*BlockIoRequest`).
    pub const BLOCK_WRITE: u64 = 21;
    /// Принудительно завершить данные на носителе (`arg0=handle`).
    pub const BLOCK_FLUSH: u64 = 22;
    /// Необратимо превращает RW shared object в RO/RX объект.
    /// `arg0=handle, arg1=VmFlags`; после seal новые права расширить нельзя.
    pub const SHARED_MEMORY_SEAL: u64 = 23;
    /// Усыпить поток, пока futex равен expected.
    /// `arg0=*const AtomicU32, arg1=expected, arg2=timeout_ns|u64::MAX`.
    pub const FUTEX_WAIT: u64 = 24;
    /// Разбудить до `count` ожидающих тот же process-local futex.
    /// `arg0=*const AtomicU32, arg1=count`; результат — число потоков.
    pub const FUTEX_WAKE: u64 = 25;
    /// Отказаться от join-capability; последний владелец делает поток detached.
    /// `arg0=thread handle`.
    pub const THREAD_DETACH: u64 = 26;
    /// Создать однонаправленный pipe. `arg0=*mut PipeCreateResult`.
    pub const PIPE_CREATE: u64 = 27;
    /// Прочитать pipe. `arg0=handle, arg1=buffer, arg2=length`.
    pub const PIPE_READ: u64 = 28;
    /// Записать pipe. `arg0=handle, arg1=buffer, arg2=length`.
    pub const PIPE_WRITE: u64 = 29;
    /// Дублировать capability с attenuation прав. `arg0=handle, arg1=Rights`.
    pub const HANDLE_DUPLICATE: u64 = 30;
    /// Неблокирующая проверка процесса. `arg0=handle, arg1=*mut ExitReason`.
    pub const PROCESS_TRY_WAIT: u64 = 31;
    /// Создать graphics-buffer object из проверенного descriptor.
    /// `arg0=*const GraphicsBufferDesc`; результат — capability handle.
    pub const GRAPHICS_BUFFER_CREATE: u64 = 32;
    /// Отобразить graphics-buffer object. `arg0=handle, arg1=*SharedMemoryMap`.
    pub const GRAPHICS_BUFFER_MAP: u64 = 33;
    /// Получить неизменяемый descriptor импортированного buffer'а.
    /// `arg0=handle, arg1=*mut GraphicsBufferDesc`.
    pub const GRAPHICS_BUFFER_GET_INFO: u64 = 34;
    /// Создать монотонный timeline. `arg0=*const SyncTimelineCreate`.
    pub const SYNC_TIMELINE_CREATE: u64 = 35;
    /// Продвинуть timeline. `arg0=*const SyncTimelineSignal`.
    pub const SYNC_TIMELINE_SIGNAL: u64 = 36;
    /// Блокировать поток до одной sync point. `arg0=*const SyncTimelineWait`.
    pub const SYNC_TIMELINE_WAIT: u64 = 37;
    /// Блокировать поток до bounded wait-set. `arg0=*const SyncWaitMany`.
    pub const SYNC_TIMELINE_WAIT_MANY: u64 = 38;
    /// Получить active output/mode. `arg0=scanout, arg1=*mut DisplayScanoutInfo`.
    pub const DISPLAY_GET_INFO: u64 = 39;
    /// Атомарно опубликовать graphics buffer.
    /// `arg0=scanout, arg1=buffer, arg2=*const DisplayAtomicPresent`;
    /// положительный результат — display sequence.
    pub const DISPLAY_ATOMIC_PRESENT: u64 = 40;
    /// Блокироваться до predicted/hardware vblank sequence.
    /// `arg0=scanout, arg1=*const DisplayVblankWait`.
    pub const DISPLAY_WAIT_VBLANK: u64 = 41;
    /// Получить render-device capabilities. `arg0=GpuRender, arg1=*mut GpuDeviceInfo`.
    pub const GPU_GET_INFO: u64 = 42;
    /// Создать изолированный context. `arg0=GpuRender, arg1=*const GpuContextCreate`.
    pub const GPU_CONTEXT_CREATE: u64 = 43;
    /// Импортировать GraphicsBuffer. `arg0=context, arg1=buffer, arg2=*GpuResourceImport`.
    pub const GPU_RESOURCE_IMPORT: u64 = 44;
    /// Создать context-local resource. `arg0=context, arg1=*GpuResourceCreate`.
    pub const GPU_RESOURCE_CREATE: u64 = 45;
    /// Асинхронно отправить VirGL stream. `arg0=context, arg1=*GpuSubmit`.
    pub const GPU_SUBMIT: u64 = 46;
    /// Прочитать status завершённого fence. `arg0=context, arg1=fence`.
    pub const GPU_COMPLETION_STATUS: u64 = 47;
    /// Создать process-owned capability IPC endpoint. Результат — handle с
    /// SEND/RECEIVE/TRANSFER либо отрицательный status.
    pub const ENDPOINT_CREATE: u64 = 48;
    /// Уничтожить context-local GPU resource после завершения использующих
    /// его fences. `arg0=context, arg1=resource`.
    pub const GPU_RESOURCE_DESTROY: u64 = 49;
}

/// Отрицательные результаты syscall. Не совпадают с Unix errno намеренно:
/// ABI RustOS мал и версионируется независимо.
pub mod status {
    /// Операция выполнена.
    pub const OK: i64 = 0;
    /// Аргумент, диапазон памяти или строка невалидны.
    pub const INVALID_ARGUMENT: i64 = -1;
    /// Handle отсутствует в таблице текущего процесса.
    pub const BAD_HANDLE: i64 = -2;
    /// Capability существует, но не содержит необходимых прав.
    pub const ACCESS_DENIED: i64 = -3;
    /// Объект не найден.
    pub const NOT_FOUND: i64 = -4;
    /// Номер syscall/операция пока не поддерживаются.
    pub const NOT_SUPPORTED: i64 = -5;
    /// Процесс завершён аппаратным exception.
    pub const FAULT: i64 = -6;
    /// Очередь endpoint заполнена; отправитель может повторить операцию позже.
    pub const QUEUE_FULL: i64 = -7;
    /// Все потоки заблокированы и продолжение текущего запуска невозможно.
    pub const DEADLOCK: i64 = -8;
    /// Не удалось выделить физическую память или служебный объект.
    pub const OUT_OF_MEMORY: i64 = -9;
    /// Достигнут фиксированный защитный лимит объектов текущей сборки.
    pub const LIMIT_REACHED: i64 = -10;
    /// Объект существует, но находится в несовместимом состоянии.
    pub const BUSY: i64 = -11;
    /// Ошибка ввода-вывода физического устройства.
    pub const IO_ERROR: i64 = -12;
    /// Время блокирующей операции истекло.
    pub const TIMED_OUT: i64 = -13;
}

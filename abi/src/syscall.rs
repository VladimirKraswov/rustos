//! Минимальный syscall ABI между ring 3 и микроядром.
//!
//! Номера операций и структуры общие для всех ISA. AMD64 bootstrap использует
//! `int 0x80` (RAX + RDI/RSI/RDX), AArch64 — `svc #0` (x8 + x0/x1/x2).
//! Различие скрывает `rustos-runtime`, поэтому исходный код приложений общий.

/// Номер software interrupt AMD64; на AArch64 непосредственное значение
/// инструкции `svc` равно нулю.
pub const INTERRUPT_VECTOR: u8 = 0x80;
/// Версия syscall ABI, передаваемая вторым bootstrap-аргументом.
pub const ABI_VERSION: u64 = 1;

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
}

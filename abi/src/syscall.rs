//! Минимальный syscall ABI между ring 3 и микроядром.
//!
//! Первая реализация использует `int 0x80`: она медленнее `SYSCALL`, зато
//! одинаково сохраняет полный user context и позволяет сначала проверить
//! fault containment. После включения per-CPU GS/TSS тот же ABI регистров
//! переводится на `SYSCALL/SYSRET` без изменения приложений.

/// Номер software interrupt системных вызовов.
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
}

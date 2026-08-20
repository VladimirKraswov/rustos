//! Приложения первого среза. Пока в ядре нет ring-3 процессов, они
//! компилируются в него; после IPC-milestone станут отдельными ELF-образами
//! (docs/ARCHITECTURE.md, «Путь к микроядру»).
pub mod shell_ui;
pub mod terminal;
pub mod ui_showcase;

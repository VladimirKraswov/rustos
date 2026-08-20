//! Стабильный wire ABI между приложением и системным UI-runtime.
//!
//! Внутренние Rust-типы UI намеренно не пересекают границу процесса: версия
//! компилятора может менять их layout. Приложение передаёт скомпилированный UI
//! IR в read-only shared memory, а дальнейшие изменения и события идут
//! компактными пакетами фиксированного размера через capability IPC.

use crate::{window::WindowId, Handle};

/// Первая версия UI ABI.
pub const UI_ABI_VERSION: u16 = 1;
/// Версия типизированного декларативного представления `.rui`.
pub const UI_IR_VERSION: u16 = 1;
/// Первые 128 бит domain-separated SHA-256 Interface ID
/// `org.rustos.system-ui/1` в RUNE package graph.
pub const UI_INTERFACE_ID: [u8; 16] = [
    0x49, 0xe7, 0xa0, 0x14, 0x4e, 0x81, 0x8c, 0xab, 0x25, 0xef, 0xb0, 0x92, 0x0b, 0x02, 0x11, 0xc7,
];

/// Флаги открытия UI-сессии.
pub mod session_flags {
    /// Runtime должен публиковать inspector counters.
    pub const DEVELOPMENT: u32 = 1 << 0;
    /// Приложение просит отключить необязательные анимации.
    pub const REDUCED_MOTION: u32 = 1 << 1;
    /// Headless surface для тестов, окно на экране не создаётся.
    pub const HEADLESS: u32 = 1 << 2;
}

/// Запрос связывает один UI tree с surface уже созданного окна.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiSessionOpen {
    /// Должно равняться [`UI_ABI_VERSION`].
    pub version: u16,
    /// Должно равняться [`UI_IR_VERSION`].
    pub ir_version: u16,
    /// Набор `session_flags`.
    pub flags: u32,
    /// Окно, содержимое которого принадлежит сессии.
    pub window: WindowId,
    /// Read-only shared-memory capability с проверенным UI IR.
    pub ir_memory: Handle,
    /// Смещение IR внутри shared memory.
    pub ir_offset: u32,
    /// Размер IR; runtime обязан проверить все внутренние диапазоны.
    pub ir_bytes: u32,
    /// Версия системной темы, с которой собран application fallback.
    pub minimum_theme_version: u32,
}

impl UiSessionOpen {
    /// Полностью инициализированный production-запрос.
    pub const fn new(window: WindowId, ir_memory: Handle, ir_bytes: u32) -> Self {
        Self {
            version: UI_ABI_VERSION,
            ir_version: UI_IR_VERSION,
            flags: 0,
            window,
            ir_memory,
            ir_offset: 0,
            ir_bytes,
            minimum_theme_version: 1,
        }
    }
}

/// Типы изменения свойства после загрузки UI IR.
pub mod update_kind {
    /// Изменить логическое/целое значение свойства.
    pub const SET_INTEGER: u16 = 1;
    /// Изменить строку на resource ID из package resource table.
    pub const SET_RESOURCE: u16 = 2;
    /// Изменить state flags (`checked`, `disabled`, `selected` и т. п.).
    pub const SET_STATE: u16 = 3;
    /// Передать результат асинхронной команды.
    pub const COMPLETE_COMMAND: u16 = 4;
}

/// Одно bounded-обновление. Массив таких записей можно отправить одной IPC
/// транзакцией или записать в shared ring, не сериализуя Rust-объекты.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiUpdate {
    /// [`UI_ABI_VERSION`].
    pub version: u16,
    /// Один из кодов [`update_kind`].
    pub kind: u16,
    /// Стабильный индекс узла из UI IR.
    pub node: u32,
    /// Типизированный ID свойства из schema конкретного компонента.
    pub property: u32,
    /// Зарезервировано; отправитель заполняет нулём.
    pub flags: u32,
    /// Значение или младшая половина составного значения.
    pub value_lo: u64,
    /// Старшая половина составного значения.
    pub value_hi: u64,
}

/// Типы событий UI-runtime -> приложение.
pub mod event_kind {
    /// Активирована команда кнопкой, меню или shortcut.
    pub const COMMAND: u16 = 1;
    /// Изменилось значение редактируемого компонента.
    pub const VALUE_CHANGED: u16 = 2;
    /// Компонент получил или потерял фокус.
    pub const FOCUS_CHANGED: u16 = 3;
    /// Runtime просит подгрузить диапазон виртуализированной коллекции.
    pub const COLLECTION_RANGE: u16 = 4;
    /// Оконная геометрия вызвала смену container-query variant.
    pub const ADAPTIVE_VARIANT: u16 = 5;
    /// Development performance budget превышен.
    pub const BUDGET_EXCEEDED: u16 = 6;
}

/// Событие не содержит указателей и может безопасно пережить обновление
/// runtime. Неизвестные `kind` игнорируются согласно negotiated ABI version.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiEvent {
    /// [`UI_ABI_VERSION`].
    pub version: u16,
    /// Один из кодов [`event_kind`].
    pub kind: u16,
    /// Узел-источник.
    pub node: u32,
    /// Command/property ID.
    pub subject: u32,
    /// Modifier/state flags.
    pub flags: u32,
    /// Event-specific payload.
    pub value_lo: u64,
    /// Event-specific payload.
    pub value_hi: u64,
    /// Монотонный serial UI-сессии.
    pub serial: u64,
}

const _: () = assert!(core::mem::size_of::<UiSessionOpen>() == 32);
const _: () = assert!(core::mem::size_of::<UiUpdate>() == 32);
const _: () = assert!(core::mem::size_of::<UiEvent>() == 40);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_request_has_explicit_versions_and_no_pointer() {
        let request = UiSessionOpen::new(WindowId::new(7), Handle(3), 4096);
        assert_eq!(request.version, UI_ABI_VERSION);
        assert_eq!(request.ir_version, UI_IR_VERSION);
        assert_eq!(request.ir_bytes, 4096);
        assert_eq!(request.flags, 0);
    }
}

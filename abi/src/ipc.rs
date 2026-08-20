//! Базовый формат IPC-сообщений RustOS.
//!
//! Маленькие запросы передаются inline. Большие данные, например содержимое
//! файла или исходный код компилятора, передаются через capability на shared
//! memory и поэтому не копируются целиком через kernel.

use crate::{Handle, Rights};

/// Версия общего IPC ABI.
pub const IPC_ABI_VERSION: u16 = 1;
/// Число байт, помещающихся непосредственно в сообщение.
pub const IPC_INLINE_BYTES: usize = 64;
/// Максимальное число capabilities в одном сообщении.
pub const IPC_MAX_HANDLES: usize = 4;

/// Заголовок IPC-запроса или ответа.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MessageHeader {
    /// Версия [`IPC_ABI_VERSION`].
    pub abi_version: u16,
    /// Код операции конкретного протокола.
    pub opcode: u16,
    /// Флаги из модуля [`flags`].
    pub flags: u32,
    /// Выбираемый клиентом идентификатор запроса для сопоставления ответа.
    pub request_id: u64,
    /// PID отправителя; заполняется ядром, поэтому user-space не может
    /// подделать его, читая собственную память.
    pub sender_pid: u64,
    /// Число значимых байт в inline payload.
    pub payload_len: u32,
    /// Число значимых элементов `handles`.
    pub handle_count: u16,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u16,
}

/// Capability, передаваемый вместе с IPC-сообщением.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TransferredHandle {
    /// Handle в таблице отправителя; ядро заменит его handle'ом получателя.
    pub handle: Handle,
    /// Зарезервировано для выравнивания, должно быть нулём.
    pub reserved: u32,
    /// Права производного capability; ядро разрешает только их ослабление.
    pub rights: Rights,
}

/// Полное небольшое IPC-сообщение фиксированного размера.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Message {
    /// Общий заголовок.
    pub header: MessageHeader,
    /// Inline payload протокола.
    pub payload: [u8; IPC_INLINE_BYTES],
    /// Передаваемые capabilities.
    pub handles: [TransferredHandle; IPC_MAX_HANDLES],
}

/// Флаги [`MessageHeader::flags`].
pub mod flags {
    /// Сообщение является ответом.
    pub const REPLY: u32 = 1 << 0;
    /// Отправитель не ожидает ответ.
    pub const ONE_WAY: u32 = 1 << 1;
    /// Запрос можно безопасно повторить после перезапуска сервиса.
    pub const IDEMPOTENT: u32 = 1 << 2;
}

const _: () = assert!(core::mem::size_of::<MessageHeader>() == 32);
const _: () = assert!(core::mem::size_of::<TransferredHandle>() == 16);
const _: () = assert!(core::mem::size_of::<Message>() == 160);

//! Независимая от адресных пространств часть endpoint IPC.

use rustos_abi::{
    ipc::{Message, IPC_ABI_VERSION, IPC_INLINE_BYTES, IPC_MAX_HANDLES},
    ProcessId, Rights,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcQueueError {
    InvalidMessage,
    QueueFull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityTransferError {
    MissingTransferRight,
    EmptyRights,
    RightsAmplification,
}

/// Проверяет fixed ABI и заменяет недоверенное поле sender PID значением,
/// известным kernel. Зарезервированные поля обязаны быть нулевыми.
pub fn prepare_message(sender: ProcessId, message: &mut Message) -> Result<(), IpcQueueError> {
    if message.header.abi_version != IPC_ABI_VERSION
        || message.header.sender_pid != 0
        || message.header.payload_len as usize > IPC_INLINE_BYTES
        || message.header.handle_count as usize > IPC_MAX_HANDLES
        || message.header.reserved != 0
    {
        return Err(IpcQueueError::InvalidMessage);
    }
    message.header.sender_pid = sender.0;
    Ok(())
}

/// Capability можно только ослабить; право TRANSFER само по себе не
/// наследуется, если отправитель явно его не запросил и не имел.
pub fn derive_capability_rights(
    source: Rights,
    requested: Rights,
) -> Result<Rights, CapabilityTransferError> {
    if !source.contains(Rights::TRANSFER) {
        return Err(CapabilityTransferError::MissingTransferRight);
    }
    if requested == Rights::NONE {
        return Err(CapabilityTransferError::EmptyRights);
    }
    if source.attenuate(requested) != requested {
        return Err(CapabilityTransferError::RightsAmplification);
    }
    Ok(requested)
}

/// Bounded FIFO. Отсутствие heap делает стоимость и предел памяти endpoint'а
/// явными; блокировку/wake выполняет scheduler вокруг этой структуры.
#[derive(Clone, Copy)]
pub struct EndpointQueue<const N: usize> {
    messages: [Message; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Default for EndpointQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> EndpointQueue<N> {
    pub const fn new() -> Self {
        Self {
            messages: [Message::EMPTY; N],
            head: 0,
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    pub fn push(&mut self, message: Message) -> Result<(), IpcQueueError> {
        if self.is_full() {
            return Err(IpcQueueError::QueueFull);
        }
        if message.header.abi_version != IPC_ABI_VERSION
            || message.header.sender_pid == 0
            || message.header.payload_len as usize > IPC_INLINE_BYTES
            || message.header.handle_count as usize > IPC_MAX_HANDLES
            || message.header.reserved != 0
        {
            return Err(IpcQueueError::InvalidMessage);
        }
        if N == 0 {
            return Err(IpcQueueError::QueueFull);
        }
        let tail = (self.head + self.len) % N;
        self.messages[tail] = message;
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Message> {
        if self.is_empty() || N == 0 {
            return None;
        }
        let message = self.messages[self.head];
        self.messages[self.head] = Message::EMPTY;
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_fifo_and_bounded() {
        let mut queue = EndpointQueue::<2>::new();
        let mut first = Message::EMPTY;
        first.header.request_id = 1;
        let mut second = Message::EMPTY;
        second.header.request_id = 2;
        prepare_message(ProcessId::new(1, 1), &mut first).unwrap();
        prepare_message(ProcessId::new(2, 1), &mut second).unwrap();
        queue.push(first).unwrap();
        queue.push(second).unwrap();
        assert_eq!(queue.push(Message::EMPTY), Err(IpcQueueError::QueueFull));
        assert_eq!(queue.pop().unwrap().header.request_id, 1);
        assert_eq!(queue.pop().unwrap().header.request_id, 2);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn sender_pid_is_kernel_supplied() {
        let mut message = Message::EMPTY;
        let sender = ProcessId::new(7, 3);
        prepare_message(sender, &mut message).unwrap();
        assert_eq!(message.header.sender_pid, sender.0);
        assert_eq!(
            prepare_message(sender, &mut message),
            Err(IpcQueueError::InvalidMessage)
        );
    }

    #[test]
    fn capability_rights_can_only_be_reduced() {
        let source = Rights::READ.union(Rights::TRANSFER);
        assert_eq!(
            derive_capability_rights(source, Rights::READ),
            Ok(Rights::READ)
        );
        assert_eq!(
            derive_capability_rights(source, Rights::WRITE),
            Err(CapabilityTransferError::RightsAmplification)
        );
        assert_eq!(
            derive_capability_rights(Rights::READ, Rights::READ),
            Err(CapabilityTransferError::MissingTransferRight)
        );
    }
}

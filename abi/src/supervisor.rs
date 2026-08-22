//! IPC-протокол постоянного пользовательского supervisor.
//!
//! Запрос специально помещается в один inline message: kernel не разбирает
//! командную строку и не принимает пользовательские pointers. Большие launch
//! manifests позднее передаются sealed shared-memory capability, не меняя
//! базовый endpoint ABI.

use crate::{ipc::IPC_INLINE_BYTES, process::PriorityClass, ExitReason, ProcessId};

/// Версия всех supervisor payload текущего протокола.
pub const SUPERVISOR_ABI_VERSION: u16 = 1;
/// Opcode запроса запуска.
pub const LAUNCH_OPCODE: u16 = 1;
/// Opcode окончательного lifecycle-ответа.
pub const LAUNCH_REPLY_OPCODE: u16 = 2;
/// Жёсткий предел повторов одного launch request.
pub const MAX_RESTARTS: u8 = 3;
/// Число inline bytes NUL-разделённого command/argv.
pub const COMMAND_BYTES: usize = 48;

/// Флаги политики одного запроса.
pub mod launch_flags {
    /// Повторять только abnormal/non-zero завершение до `restart_limit`.
    pub const RESTART_ON_FAILURE: u16 = 1 << 0;
}

/// Причина отклонения pointer-free launch record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchError {
    /// Версия не поддерживается.
    InvalidVersion,
    /// Присутствуют неизвестные флаги.
    InvalidFlags,
    /// Пользователь запросил системный/driver priority.
    InvalidPriority,
    /// Restart policy превышает bounded предел.
    InvalidRestartLimit,
    /// Inline-диапазон пуст или выходит за record.
    InvalidLength,
    /// NUL-таблица либо UTF-8 аргументов некорректны.
    InvalidArguments,
    /// Путь не абсолютный или содержит `.`/`..`.
    InvalidPath,
    /// Зарезервированное поле не равно нулю.
    ReservedNotZero,
}

/// Bounded NUL-разделённые argv. Первый элемент — абсолютный VFS path RUNE,
/// остальные — аргументы приложения; supervisor сам добавляет `argv[0]`
/// доверенного `rune-runner`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LaunchRequest {
    /// [`SUPERVISOR_ABI_VERSION`].
    pub version: u16,
    /// Маска [`launch_flags`].
    pub flags: u16,
    /// Один из непривилегированных [`PriorityClass`].
    pub priority: u8,
    /// Число дополнительных попыток после первой.
    pub restart_limit: u8,
    /// Зарезервировано, должно быть нулём.
    pub reserved0: u16,
    /// Значимые bytes в `command`.
    pub command_length: u16,
    /// Число NUL-завершённых полей в `command`.
    pub argument_count: u16,
    /// Зарезервировано, должно быть нулём.
    pub reserved1: u32,
    /// Абсолютный путь и аргументы, разделённые NUL.
    pub command: [u8; COMMAND_BYTES],
}

impl LaunchRequest {
    /// Пустая заготовка текущей версии.
    pub const EMPTY: Self = Self {
        version: SUPERVISOR_ABI_VERSION,
        flags: 0,
        priority: PriorityClass::Interactive as u8,
        restart_limit: 0,
        reserved0: 0,
        command_length: 0,
        argument_count: 0,
        reserved1: 0,
        command: [0; COMMAND_BYTES],
    };

    /// Строит bounded request из привычной ASCII-whitespace command line.
    pub fn from_command(command: &str, flags: u16, restart_limit: u8) -> Result<Self, LaunchError> {
        let mut request = Self {
            flags,
            restart_limit,
            ..Self::EMPTY
        };
        let mut cursor = 0usize;
        for word in command.split_ascii_whitespace() {
            let end = cursor
                .checked_add(word.len() + 1)
                .ok_or(LaunchError::InvalidLength)?;
            if end > COMMAND_BYTES || word.as_bytes().contains(&0) {
                return Err(LaunchError::InvalidLength);
            }
            request.command[cursor..end - 1].copy_from_slice(word.as_bytes());
            cursor = end;
            request.argument_count = request
                .argument_count
                .checked_add(1)
                .ok_or(LaunchError::InvalidArguments)?;
        }
        request.command_length = cursor as u16;
        request.validate()?;
        Ok(request)
    }

    /// Проверяет record целиком до применения какой-либо policy.
    pub fn validate(&self) -> Result<(), LaunchError> {
        if self.version != SUPERVISOR_ABI_VERSION {
            return Err(LaunchError::InvalidVersion);
        }
        if self.flags & !launch_flags::RESTART_ON_FAILURE != 0 {
            return Err(LaunchError::InvalidFlags);
        }
        if !matches!(
            self.priority,
            value if value == PriorityClass::Interactive as u8
                || value == PriorityClass::Batch as u8
                || value == PriorityClass::Idle as u8
        ) {
            return Err(LaunchError::InvalidPriority);
        }
        if self.restart_limit > MAX_RESTARTS
            || self.flags & launch_flags::RESTART_ON_FAILURE == 0 && self.restart_limit != 0
        {
            return Err(LaunchError::InvalidRestartLimit);
        }
        if self.reserved0 != 0 || self.reserved1 != 0 {
            return Err(LaunchError::ReservedNotZero);
        }
        let length = usize::from(self.command_length);
        if length == 0
            || length > COMMAND_BYTES
            || self.argument_count == 0
            || self.command[length..].iter().any(|byte| *byte != 0)
        {
            return Err(LaunchError::InvalidLength);
        }
        let mut fields = 0u16;
        let mut cursor = 0usize;
        while cursor < length {
            let tail = &self.command[cursor..length];
            let field_length = tail
                .iter()
                .position(|byte| *byte == 0)
                .ok_or(LaunchError::InvalidArguments)?;
            if field_length == 0 || core::str::from_utf8(&tail[..field_length]).is_err() {
                return Err(LaunchError::InvalidArguments);
            }
            if fields == 0 {
                validate_executable_path(&tail[..field_length])?;
            }
            fields = fields.checked_add(1).ok_or(LaunchError::InvalidArguments)?;
            cursor = cursor
                .checked_add(field_length + 1)
                .ok_or(LaunchError::InvalidLength)?;
        }
        if fields != self.argument_count {
            return Err(LaunchError::InvalidArguments);
        }
        Ok(())
    }

    /// Кодирует точное little-endian wire представление.
    pub fn encode_inline(&self) -> [u8; IPC_INLINE_BYTES] {
        let mut output = [0u8; IPC_INLINE_BYTES];
        output[0..2].copy_from_slice(&self.version.to_le_bytes());
        output[2..4].copy_from_slice(&self.flags.to_le_bytes());
        output[4] = self.priority;
        output[5] = self.restart_limit;
        output[6..8].copy_from_slice(&self.reserved0.to_le_bytes());
        output[8..10].copy_from_slice(&self.command_length.to_le_bytes());
        output[10..12].copy_from_slice(&self.argument_count.to_le_bytes());
        output[12..16].copy_from_slice(&self.reserved1.to_le_bytes());
        output[16..].copy_from_slice(&self.command);
        output
    }

    /// Декодирует и одновременно валидирует весь inline payload.
    pub fn decode_inline(bytes: &[u8; IPC_INLINE_BYTES]) -> Result<Self, LaunchError> {
        let mut request = Self {
            version: u16::from_le_bytes(bytes[0..2].try_into().unwrap()),
            flags: u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
            priority: bytes[4],
            restart_limit: bytes[5],
            reserved0: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            command_length: u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
            argument_count: u16::from_le_bytes(bytes[10..12].try_into().unwrap()),
            reserved1: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            command: [0; COMMAND_BYTES],
        };
        request.command.copy_from_slice(&bytes[16..]);
        request.validate()?;
        Ok(request)
    }
}

/// Результат всего lifecycle, а не одной попытки. `attempts` включает первый
/// запуск; `reason` никогда не подменяется общим supervisor status.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LaunchReply {
    /// [`SUPERVISOR_ABI_VERSION`].
    pub version: u16,
    /// Фактически обработанное число попыток; ноль допустим до начала spawn.
    pub attempts: u8,
    /// Зарезервировано, должно быть нулём.
    pub reserved0: u8,
    /// Результат supervisor mechanism (`spawn`/`wait`), отдельно от child exit.
    pub supervisor_status: i32,
    /// Диагностический PID последней попытки.
    pub pid: ProcessId,
    /// Точная причина завершения последнего child.
    pub reason: ExitReason,
}

impl LaunchReply {
    /// Кодирует ответ в общий inline payload без pointers и padding.
    pub const fn encode_inline(self) -> [u8; IPC_INLINE_BYTES] {
        let mut output = [0u8; IPC_INLINE_BYTES];
        let version = self.version.to_le_bytes();
        output[0] = version[0];
        output[1] = version[1];
        output[2] = self.attempts;
        output[3] = self.reserved0;
        let status = self.supervisor_status.to_le_bytes();
        output[4] = status[0];
        output[5] = status[1];
        output[6] = status[2];
        output[7] = status[3];
        let pid = self.pid.0.to_le_bytes();
        let mut index = 0;
        while index < 8 {
            output[8 + index] = pid[index];
            index += 1;
        }
        let reason_status = self.reason.status.to_le_bytes();
        output[16] = reason_status[0];
        output[17] = reason_status[1];
        output[18] = reason_status[2];
        output[19] = reason_status[3];
        let exception = self.reason.exception.to_le_bytes();
        output[20] = exception[0];
        output[21] = exception[1];
        let flags = self.reason.flags.to_le_bytes();
        output[22] = flags[0];
        output[23] = flags[1];
        let fault = self.reason.fault_address.to_le_bytes();
        index = 0;
        while index < 8 {
            output[24 + index] = fault[index];
            index += 1;
        }
        output
    }

    /// Декодирует ответ и проверяет версию, padding и bounded attempts.
    pub fn decode_inline(bytes: &[u8; IPC_INLINE_BYTES]) -> Option<Self> {
        let reply = Self {
            version: u16::from_le_bytes(bytes[0..2].try_into().ok()?),
            attempts: bytes[2],
            reserved0: bytes[3],
            supervisor_status: i32::from_le_bytes(bytes[4..8].try_into().ok()?),
            pid: ProcessId(u64::from_le_bytes(bytes[8..16].try_into().ok()?)),
            reason: ExitReason {
                status: i32::from_le_bytes(bytes[16..20].try_into().ok()?),
                exception: u16::from_le_bytes(bytes[20..22].try_into().ok()?),
                flags: u16::from_le_bytes(bytes[22..24].try_into().ok()?),
                fault_address: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
            },
        };
        (reply.version == SUPERVISOR_ABI_VERSION
            && reply.reserved0 == 0
            && reply.attempts <= MAX_RESTARTS + 1
            && bytes[32..].iter().all(|byte| *byte == 0))
        .then_some(reply)
    }
}

fn validate_executable_path(path: &[u8]) -> Result<(), LaunchError> {
    if path.first() != Some(&b'/')
        || path.contains(&b'\\')
        || path
            .split(|byte| *byte == b'/')
            .skip(1)
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(LaunchError::InvalidPath);
    }
    Ok(())
}

const _: () = assert!(core::mem::size_of::<LaunchRequest>() == IPC_INLINE_BYTES);
const _: () = assert!(core::mem::size_of::<LaunchReply>() == 32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_request_round_trips_and_is_bounded() {
        let request = LaunchRequest::from_command(
            "/apps/hello.rune --verbose",
            launch_flags::RESTART_ON_FAILURE,
            2,
        )
        .unwrap();
        let decoded = LaunchRequest::decode_inline(&request.encode_inline()).unwrap();
        assert_eq!(decoded.argument_count, 2);
        assert_eq!(decoded.restart_limit, 2);
    }

    #[test]
    fn launch_request_rejects_parent_path_and_unbounded_restart() {
        assert!(LaunchRequest::from_command("/apps/../bad.rune", 0, 0).is_err());
        assert!(LaunchRequest::from_command(
            "/apps/good.rune",
            launch_flags::RESTART_ON_FAILURE,
            MAX_RESTARTS + 1
        )
        .is_err());
    }

    #[test]
    fn lifecycle_reply_rejects_unbounded_attempts_and_trailing_bytes() {
        let reply = LaunchReply {
            version: SUPERVISOR_ABI_VERSION,
            attempts: MAX_RESTARTS + 1,
            reserved0: 0,
            supervisor_status: 0,
            pid: ProcessId(42),
            reason: ExitReason {
                status: 0,
                exception: 0,
                flags: 0,
                fault_address: 0,
            },
        };
        let mut encoded = reply.encode_inline();
        assert!(LaunchReply::decode_inline(&encoded).is_some());
        encoded[2] = 0;
        assert!(LaunchReply::decode_inline(&encoded).is_some());
        encoded = reply.encode_inline();
        encoded[2] = MAX_RESTARTS + 2;
        assert!(LaunchReply::decode_inline(&encoded).is_none());
        encoded = reply.encode_inline();
        encoded[63] = 1;
        assert!(LaunchReply::decode_inline(&encoded).is_none());
    }
}

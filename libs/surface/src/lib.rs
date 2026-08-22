//! Клиентская системная библиотека `surface.dll`.
//!
//! Приложение владеет buffer queue и рисует только в свободный slot. Эта
//! библиотека скрывает capability IPC, создаёт приватный event endpoint и
//! превращает wire-сообщения compositord в типизированные события. Пиксели
//! никогда не копируются через IPC.

#![no_std]

use rustos_abi::{
    ipc::{flags as ipc_flags, TransferredHandle},
    surface::{
        BufferReleased, PresentationFeedback, SurfaceCommit, SurfaceCreateRequest, SurfaceCreated,
        SurfaceDestroyRequest, SurfaceId, SurfaceMetrics, SURFACE_BUFFER_RELEASED_OPCODE,
        SURFACE_COMMIT_OPCODE, SURFACE_CREATED_OPCODE, SURFACE_CREATE_HANDLE_COUNT,
        SURFACE_CREATE_OPCODE, SURFACE_DESTROY_OPCODE, SURFACE_PRESENTATION_FEEDBACK_OPCODE,
        SURFACE_RELEASE_HANDLE_COUNT,
    },
};
use rustos_runtime::{
    endpoint_create, handle_close, ipc_receive, ipc_send, syscall, Handle, Message, Rights,
};

/// Ошибки facade. Syscall status сохраняется, чтобы приложение могло отличить
/// backpressure/restart сервиса от некорректного локального состояния.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceError {
    /// Локальные metrics/queue/commit не проходят ABI validation.
    InvalidArgument,
    /// Kernel не смог создать приватный event endpoint.
    Endpoint(i64),
    /// Отправка в compositord не выполнена.
    Send(i64),
    /// Получение события не выполнено.
    Receive(i64),
    /// Ответ имеет чужой opcode/request ID/owner или неверный wire record.
    Protocol,
}

/// Типизированное асинхронное событие одной surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceEvent {
    /// Buffer slot снова разрешено передать renderer'у.
    BufferReleased {
        info: BufferReleased,
        release_timeline: Handle,
    },
    /// Судьба кадра на физическом output.
    Presentation(PresentationFeedback),
}

/// Одно соединение приложения с compositord.
///
/// `server` выдаёт supervisor, `events` принадлежит клиенту. Числовые handles
/// process-local и намеренно не являются частью публичного persistent ABI.
pub struct SurfaceClient {
    server: Handle,
    events: Handle,
    surface: SurfaceId,
    metrics: SurfaceMetrics,
    queue_depth: u16,
    generation: u64,
    next_request_id: u64,
}

impl SurfaceClient {
    /// Создаёт независимую surface. Compositord получает только `SEND` к
    /// приватной очереди событий; единственное право `RECEIVE` остаётся здесь.
    pub fn connect(
        server: Handle,
        metrics: SurfaceMetrics,
        queue_depth: u16,
    ) -> Result<Self, SurfaceError> {
        let request = SurfaceCreateRequest::new(metrics, queue_depth);
        request
            .validate()
            .map_err(|_| SurfaceError::InvalidArgument)?;
        let endpoint_value = endpoint_create();
        if endpoint_value <= 0 {
            return Err(SurfaceError::Endpoint(endpoint_value));
        }
        let events = Handle(endpoint_value as u32);
        let mut message = Message::EMPTY;
        message.header.opcode = SURFACE_CREATE_OPCODE;
        message.header.request_id = 1;
        message.header.payload_len = core::mem::size_of::<SurfaceCreateRequest>() as u32;
        message.header.handle_count = SURFACE_CREATE_HANDLE_COUNT;
        message.payload = request.encode_inline();
        message.handles[0] = TransferredHandle {
            handle: events,
            reserved: 0,
            rights: Rights::SEND,
        };
        let status = ipc_send(server, &message);
        if status != syscall::status::OK {
            let _ = handle_close(events);
            return Err(SurfaceError::Send(status));
        }
        let mut response = Message::EMPTY;
        let status = ipc_receive(events, &mut response);
        if status != syscall::status::OK {
            let _ = handle_close(events);
            return Err(SurfaceError::Receive(status));
        }
        let created = match SurfaceCreated::decode_inline(&response.payload) {
            Ok(created) => created,
            Err(_) => {
                let _ = handle_close(events);
                return Err(SurfaceError::Protocol);
            }
        };
        if response.header.flags & ipc_flags::REPLY == 0
            || response.header.opcode != SURFACE_CREATED_OPCODE
            || response.header.request_id != 1
            || response.header.sender_pid == 0
            || response.header.payload_len != 64
            || response.header.handle_count != 0
            || created.queue_depth > queue_depth
        {
            let _ = handle_close(events);
            return Err(SurfaceError::Protocol);
        }
        Ok(Self {
            server,
            events,
            surface: created.surface,
            metrics,
            queue_depth: created.queue_depth,
            generation: created.generation,
            next_request_id: 2,
        })
    }

    /// Назначенный server-local ID.
    pub const fn id(&self) -> SurfaceId {
        self.surface
    }

    /// Фактическая глубина очереди.
    pub const fn queue_depth(&self) -> u16 {
        self.queue_depth
    }

    /// Generation меняется при будущей атомарной resize/recreate операции.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Публикует целиком перерисованный buffer. Владение исходными handles
    /// остаётся у клиента; kernel создаёт compositor'у ослабленные copies.
    pub fn commit_full(
        &mut self,
        buffer_slot: u16,
        frame_id: u64,
        buffer: Handle,
        acquire_timeline: Handle,
        request_feedback: bool,
    ) -> Result<(), SurfaceError> {
        if buffer_slot >= self.queue_depth {
            return Err(SurfaceError::InvalidArgument);
        }
        let mut commit =
            SurfaceCommit::full_damage(self.surface, self.metrics, frame_id, buffer_slot);
        if request_feedback {
            commit.flags |= rustos_abi::surface::commit_flags::REQUEST_FEEDBACK;
        }
        commit
            .validate()
            .map_err(|_| SurfaceError::InvalidArgument)?;
        let request_id = self.take_request_id();
        let mut message = Message::EMPTY;
        message.header.opcode = SURFACE_COMMIT_OPCODE;
        message.header.request_id = request_id;
        message.header.payload_len = 64;
        message.header.handle_count = commit.handle_count();
        message.payload = commit.encode_inline();
        message.handles[0] = TransferredHandle {
            handle: buffer,
            reserved: 0,
            rights: Rights::READ.union(Rights::TRANSFER),
        };
        message.handles[1] = TransferredHandle {
            handle: acquire_timeline,
            reserved: 0,
            rights: Rights::WAIT.union(Rights::TRANSFER),
        };
        let status = ipc_send(self.server, &message);
        if status == syscall::status::OK {
            Ok(())
        } else {
            Err(SurfaceError::Send(status))
        }
    }

    /// Блокируется до release или presentation feedback. Несвязанные и
    /// повреждённые сообщения не пропускаются приложению.
    pub fn receive_event(&self) -> Result<SurfaceEvent, SurfaceError> {
        let mut message = Message::EMPTY;
        let status = ipc_receive(self.events, &mut message);
        if status != syscall::status::OK {
            return Err(SurfaceError::Receive(status));
        }
        if message.header.sender_pid == 0 || message.header.payload_len != 64 {
            return Err(SurfaceError::Protocol);
        }
        match message.header.opcode {
            SURFACE_BUFFER_RELEASED_OPCODE
                if message.header.handle_count == SURFACE_RELEASE_HANDLE_COUNT =>
            {
                let info = BufferReleased::decode_inline(&message.payload)
                    .map_err(|_| SurfaceError::Protocol)?;
                if info.surface != self.surface {
                    return Err(SurfaceError::Protocol);
                }
                Ok(SurfaceEvent::BufferReleased {
                    info,
                    release_timeline: message.handles[0].handle,
                })
            }
            SURFACE_PRESENTATION_FEEDBACK_OPCODE if message.header.handle_count == 0 => {
                let feedback = PresentationFeedback::decode_inline(&message.payload)
                    .map_err(|_| SurfaceError::Protocol)?;
                if feedback.surface != self.surface {
                    return Err(SurfaceError::Protocol);
                }
                Ok(SurfaceEvent::Presentation(feedback))
            }
            _ => Err(SurfaceError::Protocol),
        }
    }

    /// Закрывает surface после того, как приложение получило release всех
    /// опубликованных slots. `self` потребляется, исключая commit после close.
    pub fn disconnect(mut self) -> Result<(), SurfaceError> {
        let request_id = self.take_request_id();
        let destroy = SurfaceDestroyRequest::new(self.surface);
        let mut message = Message::EMPTY;
        message.header.opcode = SURFACE_DESTROY_OPCODE;
        message.header.flags = rustos_abi::ipc::flags::ONE_WAY;
        message.header.request_id = request_id;
        message.header.payload_len = 64;
        message.payload = destroy.encode_inline();
        let status = ipc_send(self.server, &message);
        let close_status = handle_close(self.events);
        self.events = Handle::INVALID;
        if status != syscall::status::OK {
            Err(SurfaceError::Send(status))
        } else if close_status != syscall::status::OK {
            Err(SurfaceError::Endpoint(close_status))
        } else {
            Ok(())
        }
    }

    fn take_request_id(&mut self) -> u64 {
        let value = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(2);
        value
    }
}

impl Drop for SurfaceClient {
    fn drop(&mut self) {
        if self.events != Handle::INVALID {
            let _ = handle_close(self.events);
        }
    }
}

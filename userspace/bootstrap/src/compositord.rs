//! Постоянный ring-3 `compositord` bootstrap service.
//!
//! Сервис владеет политикой кадров, но не scanout и не GPU. Обычный CPU
//! surface и GPU-only `GraphicsBuffer` проходят один atomic-present protocol.
//! Системное `gpu-demo` отправляет только bounded request; compositor pacing
//! выполняется vblank feedback'ом от изолированного displayd.

#![no_std]
#![no_main]

use core::{
    panic::PanicInfo,
    sync::atomic::{AtomicU64, Ordering},
};

use rustos_abi::{
    display::{
        DisplayPresentFeedback, DisplayPresentRequest, DisplayScanoutInfo, DISPLAY_FEEDBACK_OPCODE,
        DISPLAY_INFO_OPCODE, DISPLAY_PRESENT_HANDLE_COUNT, DISPLAY_PRESENT_OPCODE,
        DISPLAY_QUERY_HANDLE_COUNT, DISPLAY_QUERY_OPCODE,
    },
    gpu::{
        demo_flag, GpuDemoRequest, GpuRenderFrame, GpuUiFrameHeader, GpuUiMailboxSlotHeader,
        GPU_DEMO_START_OPCODE, GPU_RENDERED_FRAME_HANDLE_COUNT, GPU_RENDERED_FRAME_OPCODE,
        GPU_RENDER_REQUEST_OPCODE, GPU_UI_MAILBOX_FRAME_OFFSET, GPU_UI_MAILBOX_SLOTS,
        GPU_UI_STREAM_BYTES,
    },
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain, PixelFormatCode},
    ipc::{flags as ipc_flags, TransferredHandle},
    memory::MEMORY_ABI_VERSION,
    surface::{
        commit_flags, feedback_flags, BufferReleased, PresentationFeedback, PresentationStatus,
        SurfaceCommit, SurfaceCreateRequest, SurfaceCreated, SurfaceDestroyRequest, SurfaceId,
        SurfaceMetrics, SURFACE_ABI_VERSION, SURFACE_BUFFER_RELEASED_OPCODE,
        SURFACE_COMMIT_FULL_HANDLE_COUNT, SURFACE_COMMIT_OPCODE, SURFACE_CREATED_OPCODE,
        SURFACE_CREATE_HANDLE_COUNT, SURFACE_CREATE_OPCODE, SURFACE_DESTROY_OPCODE,
        SURFACE_PRESENTATION_FEEDBACK_OPCODE, SURFACE_RELEASE_HANDLE_COUNT,
    },
    sync::{SyncTimelineCreate, SyncTimelineSignal, SyncTimelineWait, SYNC_TIMEOUT_INFINITE},
};
use rustos_compositor::{
    select_newest_scene, FocusRouter, FrameClock, InputTarget, SceneCandidate,
};
use rustos_runtime::{
    graphics_buffer_create, graphics_buffer_get_info, graphics_buffer_map, handle_close,
    handle_duplicate, ipc_receive, ipc_send, process_exit, shared_memory_map, sync_timeline_create,
    sync_timeline_signal, sync_timeline_wait, syscall, vm_unmap, Handle, Message, Rights,
    SharedMemoryMap, VmFlags,
};
use rustos_video::{Rect, SurfaceQueue};

const FRAME_MAGIC: u64 = 0x5255_5354_4f53_4758;
const GPU_MODE_FLAG: u64 = 1 << 63;
const GPU_FRAME_ENDPOINT: Handle = Handle(4);
const GPU_CONTROL_ENDPOINT: Handle = Handle(5);
const SURFACE_CONTROL_ENDPOINT: Handle = Handle(6);
const UI_MAILBOX_HANDLES: [Handle; GPU_UI_MAILBOX_SLOTS] = [Handle(7), Handle(8)];
const MAX_CLIENT_SURFACES: usize = 16;

#[derive(Clone, Copy)]
struct PreparedFrame {
    descriptor: GraphicsBufferDesc,
    buffer: Handle,
    acquire: Handle,
    acquire_value: u64,
}

/// Постоянная release timeline внутреннего desktop swapchain.
///
/// Создание/уничтожение kernel object на каждый vblank было чистой служебной
/// нагрузкой. Значение монотонно растёт, а displayd получает обычную
/// capability-копию того же timeline вместе с каждым present.
struct PresentClock {
    timeline: Handle,
    value: u64,
    frame: FrameClock,
}

impl PresentClock {
    fn new(refresh_millihertz: u32) -> Self {
        let value = sync_timeline_create(&SyncTimelineCreate::new(0));
        if value <= 0 {
            process_exit(187);
        }
        Self {
            timeline: Handle(value as u32),
            value: 0,
            frame: FrameClock::new(1_000_000_000_000u64 / u64::from(refresh_millihertz.max(1))),
        }
    }

    fn advance(&mut self) -> u64 {
        self.frame.request_frame();
        self.value = self.value.saturating_add(1);
        self.value
    }
}

#[derive(Clone, Copy)]
struct ClientSurface {
    owner_pid: u64,
    id: SurfaceId,
    events: Handle,
    metrics: SurfaceMetrics,
    queue_depth: u16,
    generation: u64,
    last_frame_id: u64,
}

impl ClientSurface {
    const EMPTY: Self = Self {
        owner_pid: 0,
        id: SurfaceId::INVALID,
        events: Handle::INVALID,
        metrics: SurfaceMetrics::new(1, 1, 1, 1, 1000),
        queue_depth: 0,
        generation: 0,
        last_frame_id: 0,
    };

    const fn is_empty(self) -> bool {
        self.owner_pid == 0
    }
}

/// Отображает два immutable scene slot только на чтение. Compositor владеет
/// политикой выбора newest frame; renderd получает те же capabilities, но не
/// может публиковать новое состояние сцены.
fn map_ui_mailbox() -> [*const u8; GPU_UI_MAILBOX_SLOTS] {
    let mut slots = [core::ptr::null(); GPU_UI_MAILBOX_SLOTS];
    for (index, handle) in UI_MAILBOX_HANDLES.into_iter().enumerate() {
        let mapping = SharedMemoryMap {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            address: 0,
            offset: 0,
            length: GPU_UI_STREAM_BYTES as u64,
            flags: VmFlags::READ,
        };
        let address = shared_memory_map(handle, &mapping);
        if address <= 0 {
            process_exit(208);
        }
        let base = address as *const u8;
        let metadata = unsafe { base.cast::<GpuUiMailboxSlotHeader>().read_volatile() };
        if metadata.validate(index as u16).is_err() {
            process_exit(208);
        }
        slots[index] = base;
    }
    slots
}

/// Возвращает newest полностью опубликованный кадр. Один acquire-load на slot
/// достаточно для выбора: renderd повторно проверяет generation вокруг
/// private snapshot и никогда не отправляет GPU частично записанную сцену.
fn newest_ui_frame(
    slots: [*const u8; GPU_UI_MAILBOX_SLOTS],
    last_presented: u64,
    scene_initialized: bool,
) -> Option<(u64, u16, bool)> {
    let mut candidates = [SceneCandidate {
        frame_id: 0,
        slot: 0,
        full: false,
    }; GPU_UI_MAILBOX_SLOTS];
    for (index, base) in slots.into_iter().enumerate() {
        let published = unsafe { &*base.cast::<AtomicU64>() }.load(Ordering::Acquire);
        if published <= last_presented {
            continue;
        }
        let frame = unsafe {
            base.add(GPU_UI_MAILBOX_FRAME_OFFSET)
                .cast::<GpuUiFrameHeader>()
                .read_volatile()
        };
        if frame.validate().is_err()
            || frame.frame_id != published
            || unsafe { &*base.cast::<AtomicU64>() }.load(Ordering::Acquire) != published
        {
            continue;
        }
        candidates[index] = SceneCandidate {
            frame_id: published,
            slot: index as u16,
            full: frame.flags == GpuUiFrameHeader::FULL_FRAME,
        };
    }
    select_newest_scene(&candidates, last_presented, scene_initialized)
        .map(|candidate| (candidate.frame_id, candidate.slot, candidate.full))
}

#[no_mangle]
pub extern "C" fn _start(display_endpoint: u64, feedback_endpoint: u64, abi_version: u64) -> ! {
    let gpu_mode = abi_version & GPU_MODE_FLAG != 0;
    if abi_version & !GPU_MODE_FLAG != syscall::ABI_VERSION {
        process_exit(181);
    }
    let display_endpoint = Handle(display_endpoint as u32);
    let feedback_endpoint = Handle(feedback_endpoint as u32);
    let info = query_display(display_endpoint, feedback_endpoint);
    let mut present_clock = PresentClock::new(info.refresh_millihertz);
    let mut frame_id = 1u64;
    let ui_mailbox = gpu_mode.then(map_ui_mailbox);
    let mut last_system_ui_frame = 0u64;
    let mut system_ui_scene_initialized = false;
    let mut next_surface_id = 1u64;
    let mut surfaces = [ClientSurface::EMPTY; MAX_CLIENT_SURFACES];
    let mut focus = FocusRouter::<MAX_CLIENT_SURFACES>::new();
    // Стартовый desktop остаётся обычным CPU buffer. Иначе первый GPU кадр
    // навсегда зафиксировал бы размер renderd target равным scanout и не дал
    // бы приложениям создавать компактные оконные surfaces.
    let first = prepare_cpu_frame(info);
    present_frame(
        display_endpoint,
        feedback_endpoint,
        info,
        first,
        frame_id,
        &mut present_clock,
    );

    loop {
        let mut control = Message::EMPTY;
        if ipc_receive(SURFACE_CONTROL_ENDPOINT, &mut control) != syscall::status::OK
            || control.header.sender_pid == 0
        {
            process_exit(210);
        }
        match control.header.opcode {
            SURFACE_CREATE_OPCODE => {
                handle_surface_create(&control, &mut surfaces, &mut next_surface_id, &mut focus);
                continue;
            }
            SURFACE_COMMIT_OPCODE => {
                handle_surface_commit(
                    display_endpoint,
                    feedback_endpoint,
                    info,
                    &control,
                    &mut surfaces,
                );
                continue;
            }
            SURFACE_DESTROY_OPCODE => {
                handle_surface_destroy(&control, &mut surfaces, &mut focus);
                continue;
            }
            GPU_DEMO_START_OPCODE if gpu_mode => {}
            _ => {
                close_transferred_handles(&control);
                continue;
            }
        }
        if control.header.payload_len != 64 || control.header.handle_count != 0 {
            close_transferred_handles(&control);
            continue;
        }
        let request = match GpuDemoRequest::decode_inline(&control.payload) {
            Ok(request) => request,
            Err(_) => continue,
        };
        if request.flags & demo_flag::SYSTEM_UI != 0 {
            let Some(mailbox) = ui_mailbox else {
                continue;
            };
            let Some((scene_frame, mailbox_slot, full_frame)) =
                newest_ui_frame(mailbox, last_system_ui_frame, system_ui_scene_initialized)
            else {
                continue;
            };
            // Несколько wakeups могут лежать в bounded endpoint queue, но
            // каждый из них смотрит на один newest mailbox. Поэтому старые
            // mouse frames не рендерятся после уже показанного кадра.
            if scene_frame <= last_system_ui_frame {
                continue;
            }
            let prepared = prepare_gpu_frame(
                info.width,
                info.height,
                scene_frame,
                None,
                false,
                true,
                Some(mailbox_slot),
            );
            present_frame(
                display_endpoint,
                feedback_endpoint,
                info,
                prepared,
                scene_frame,
                &mut present_clock,
            );
            last_system_ui_frame = scene_frame;
            system_ui_scene_initialized |= full_frame;
            continue;
        }
        let windowed = request.flags & demo_flag::WINDOWED != 0;
        let compositor_probe = request.flags & demo_flag::COMPOSITOR_PROBE != 0;
        let render_width = if windowed { request.width } else { info.width };
        let render_height = if windowed {
            request.height
        } else {
            info.height
        };
        if windowed {
            // Bounded windowed запрос здесь остаётся диагностикой отдельного
            // render target. Рабочая Aurora приходит в обычном SystemUI
            // stream как semantic Canvas и смешивается renderd без readback.
            frame_id = frame_id.saturating_add(1);
            let frame = prepare_gpu_frame(
                render_width,
                render_height,
                frame_id,
                Some(request.first_frame),
                compositor_probe,
                false,
                None,
            );
            wait_prepared(frame);
            discard_frame(frame);
        } else {
            present_swapchain(
                display_endpoint,
                feedback_endpoint,
                info,
                request,
                render_width,
                render_height,
                &mut frame_id,
                &mut present_clock,
            );
        }
    }
}

fn handle_surface_create(
    message: &Message,
    surfaces: &mut [ClientSurface; MAX_CLIENT_SURFACES],
    next_surface_id: &mut u64,
    focus: &mut FocusRouter<MAX_CLIENT_SURFACES>,
) {
    if message.header.payload_len as usize != core::mem::size_of::<SurfaceCreateRequest>()
        || message.header.handle_count != SURFACE_CREATE_HANDLE_COUNT
    {
        close_transferred_handles(message);
        return;
    }
    let request = match SurfaceCreateRequest::decode_inline(
        &message.payload[..core::mem::size_of::<SurfaceCreateRequest>()],
    ) {
        Ok(request) => request,
        Err(_) => {
            close_transferred_handles(message);
            return;
        }
    };
    let events = message.handles[0].handle;
    // После аварийного завершения клиента kernel уже отозвал event endpoint.
    // Ленивая проверка освобождает server metadata без глобального PID lookup.
    for surface in surfaces.iter_mut().filter(|surface| !surface.is_empty()) {
        let probe = handle_duplicate(surface.events, Rights::SEND);
        if probe > 0 {
            let _ = handle_close(Handle(probe as u32));
        } else {
            let _ = focus.remove(InputTarget {
                owner_pid: surface.owner_pid,
                surface: surface.id,
            });
            *surface = ClientSurface::EMPTY;
        }
    }
    let Some(slot) = surfaces.iter_mut().find(|surface| surface.is_empty()) else {
        let _ = handle_close(events);
        return;
    };
    let id = SurfaceId(*next_surface_id);
    *next_surface_id = next_surface_id.wrapping_add(1).max(1);
    let created = SurfaceCreated::new(id, request.queue_depth, 1);
    let mut response = Message::EMPTY;
    response.header.opcode = SURFACE_CREATED_OPCODE;
    response.header.flags = ipc_flags::REPLY;
    response.header.request_id = message.header.request_id;
    response.header.payload_len = 64;
    response.payload = created.encode_inline();
    if ipc_send(events, &response) != syscall::status::OK {
        let _ = handle_close(events);
        return;
    }
    *slot = ClientSurface {
        owner_pid: message.header.sender_pid,
        id,
        events,
        metrics: request.metrics,
        queue_depth: request.queue_depth,
        generation: 1,
        last_frame_id: 0,
    };
    let _ = focus.insert(
        InputTarget {
            owner_pid: message.header.sender_pid,
            surface: id,
        },
        Rect::new(
            0,
            0,
            request.metrics.physical_width,
            request.metrics.physical_height,
        ),
        i32::try_from(id.0).unwrap_or(i32::MAX),
    );
}

fn handle_surface_destroy(
    message: &Message,
    surfaces: &mut [ClientSurface; MAX_CLIENT_SURFACES],
    focus: &mut FocusRouter<MAX_CLIENT_SURFACES>,
) {
    if message.header.payload_len != 64 || message.header.handle_count != 0 {
        close_transferred_handles(message);
        return;
    }
    let request = match SurfaceDestroyRequest::decode_inline(&message.payload) {
        Ok(request) => request,
        Err(_) => return,
    };
    let Some(slot) = surfaces.iter_mut().find(|surface| {
        surface.owner_pid == message.header.sender_pid && surface.id == request.surface
    }) else {
        return;
    };
    let _ = handle_close(slot.events);
    let _ = focus.remove(InputTarget {
        owner_pid: slot.owner_pid,
        surface: slot.id,
    });
    *slot = ClientSurface::EMPTY;
}

fn handle_surface_commit(
    display: Handle,
    feedback_endpoint: Handle,
    info: DisplayScanoutInfo,
    message: &Message,
    surfaces: &mut [ClientSurface; MAX_CLIENT_SURFACES],
) {
    if message.header.payload_len != 64
        || message.header.handle_count != SURFACE_COMMIT_FULL_HANDLE_COUNT
    {
        close_transferred_handles(message);
        return;
    }
    let commit = match SurfaceCommit::decode_inline(&message.payload) {
        Ok(commit) if commit.flags & commit_flags::FULL_DAMAGE != 0 => commit,
        _ => {
            close_transferred_handles(message);
            return;
        }
    };
    let Some(surface_index) = surfaces.iter().position(|surface| {
        surface.owner_pid == message.header.sender_pid && surface.id == commit.surface
    }) else {
        close_transferred_handles(message);
        return;
    };
    let surface = surfaces[surface_index];
    if surface.generation == 0
        || commit.metrics != surface.metrics
        || commit.buffer_slot >= surface.queue_depth
        || commit.frame_id <= surface.last_frame_id
    {
        close_transferred_handles(message);
        return;
    }
    let buffer = message.handles[0].handle;
    let acquire = message.handles[1].handle;
    let mut descriptor = empty_descriptor();
    if graphics_buffer_get_info(buffer, &mut descriptor) != syscall::status::OK
        || descriptor.validate().is_err()
        || descriptor.width != commit.metrics.physical_width
        || descriptor.height != commit.metrics.physical_height
        || !matches!(
            descriptor.format,
            PixelFormatCode::B8G8R8A8_UNORM | PixelFormatCode::B8G8R8X8_UNORM
        )
        || !descriptor.usage.contains(BufferUsage::RENDER_TARGET)
        || sync_timeline_wait(&SyncTimelineWait::new(acquire, 1, 250_000_000))
            != syscall::status::OK
    {
        close_transferred_handles(message);
        return;
    }
    surfaces[surface_index].last_frame_id = commit.frame_id;
    let prepared = PreparedFrame {
        descriptor,
        buffer,
        acquire,
        acquire_value: 1,
    };
    // До включения multi-layer render pass только surface, совпадающая со
    // scanout, получает честный zero-copy fast path. Оконный buffer никогда
    // не скачивается на CPU: он будет принят новым GPU compositor stage.
    if descriptor.width != info.width || descriptor.height != info.height {
        release_dropped_surface(surface, commit, prepared);
        return;
    }
    let (display_feedback, release) =
        present_frame_raw(display, feedback_endpoint, info, prepared, commit.frame_id);
    send_surface_release(surface, commit, release);
    if commit.flags & commit_flags::REQUEST_FEEDBACK != 0 {
        let feedback = PresentationFeedback {
            version: SURFACE_ABI_VERSION,
            size: core::mem::size_of::<PresentationFeedback>() as u16,
            status: PresentationStatus::PRESENTED,
            flags: feedback_flags::DIRECT_SCANOUT,
            surface: surface.id,
            frame_id: commit.frame_id,
            sequence: display_feedback.sequence,
            actual_time_ns: display_feedback.actual_time_ns,
            refresh_interval_ns: display_feedback.refresh_interval_ns,
            output: display_feedback.output,
            reserved_tail: 0,
        };
        send_surface_feedback(surface.events, message.header.request_id, feedback);
    }
    close_prepared_and_release(prepared, release);
}

fn release_dropped_surface(surface: ClientSurface, commit: SurfaceCommit, prepared: PreparedFrame) {
    let release_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if release_value <= 0 {
        discard_frame(prepared);
        return;
    }
    let release = Handle(release_value as u32);
    if sync_timeline_signal(&SyncTimelineSignal::new(release, 1)) == syscall::status::OK {
        send_surface_release(surface, commit, release);
        if commit.flags & commit_flags::REQUEST_FEEDBACK != 0 {
            let dropped = PresentationFeedback {
                version: SURFACE_ABI_VERSION,
                size: core::mem::size_of::<PresentationFeedback>() as u16,
                status: PresentationStatus::DROPPED,
                flags: 0,
                surface: surface.id,
                frame_id: commit.frame_id,
                sequence: 0,
                actual_time_ns: 0,
                refresh_interval_ns: 0,
                output: rustos_abi::surface::OutputId::NONE,
                reserved_tail: 0,
            };
            send_surface_feedback(surface.events, commit.frame_id, dropped);
        }
    }
    close_prepared_and_release(prepared, release);
}

fn send_surface_release(surface: ClientSurface, commit: SurfaceCommit, release: Handle) {
    let released = BufferReleased {
        version: SURFACE_ABI_VERSION,
        size: core::mem::size_of::<BufferReleased>() as u16,
        flags: 0,
        surface: surface.id,
        frame_id: commit.frame_id,
        release_value: 1,
        buffer_slot: commit.buffer_slot,
        reserved_header: [0; 3],
        reserved: [0; 3],
    };
    let mut response = Message::EMPTY;
    response.header.opcode = SURFACE_BUFFER_RELEASED_OPCODE;
    response.header.request_id = commit.frame_id;
    response.header.payload_len = 64;
    response.header.handle_count = SURFACE_RELEASE_HANDLE_COUNT;
    response.payload = released.encode_inline();
    response.handles[0] = TransferredHandle {
        handle: release,
        reserved: 0,
        rights: Rights::WAIT,
    };
    let _ = ipc_send(surface.events, &response);
}

fn send_surface_feedback(events: Handle, request_id: u64, feedback: PresentationFeedback) {
    let mut response = Message::EMPTY;
    response.header.opcode = SURFACE_PRESENTATION_FEEDBACK_OPCODE;
    response.header.request_id = request_id;
    response.header.payload_len = 64;
    response.payload = feedback.encode_inline();
    let _ = ipc_send(events, &response);
}

fn close_transferred_handles(message: &Message) {
    for transferred in message
        .handles
        .iter()
        .take(message.header.handle_count as usize)
    {
        let _ = handle_close(transferred.handle);
    }
}

fn close_prepared_and_release(prepared: PreparedFrame, release: Handle) {
    let _ = handle_close(prepared.buffer);
    let _ = handle_close(prepared.acquire);
    let _ = handle_close(release);
}

/// Держит до трёх GPU кадров впереди displayd. Если более свежий frame уже
/// готов, а старый опоздал к refresh boundary, stale buffer отбрасывается —
/// это mailbox semantics, а не растущая FIFO задержка.
#[allow(clippy::too_many_arguments)]
fn present_swapchain(
    display: Handle,
    feedback: Handle,
    info: DisplayScanoutInfo,
    request: GpuDemoRequest,
    width: u32,
    height: u32,
    frame_id: &mut u64,
    present_clock: &mut PresentClock,
) {
    const IMAGE_COUNT: usize = 3;
    let mut queue = SurfaceQueue::<PreparedFrame, IMAGE_COUNT>::new(IMAGE_COUNT)
        .unwrap_or_else(|_| process_exit(206));
    let mut submitted = 0u32;
    let mut consumed = 0u32;
    while consumed < request.frame_count {
        while submitted < request.frame_count {
            let Ok(slot) = queue.acquire() else {
                break;
            };
            *frame_id = frame_id.saturating_add(1);
            let scene = request.first_frame.saturating_add(submitted);
            let prepared = prepare_gpu_frame(
                width,
                height,
                *frame_id,
                Some(scene),
                false,
                request.flags & demo_flag::SYSTEM_UI != 0,
                None,
            );
            queue
                .publish(slot, *frame_id, prepared)
                .unwrap_or_else(|_| process_exit(206));
            submitted = submitted.saturating_add(1);
        }
        let selection = queue
            .select_mailbox(prepared_ready)
            .unwrap_or_else(|_| process_exit(206));
        for stale in selection
            .dropped
            .iter()
            .take(selection.dropped_count)
            .flatten()
        {
            discard_frame(*stale);
            consumed = consumed.saturating_add(1);
        }
        let (slot, id, prepared) = selection.selected;
        present_frame(display, feedback, info, prepared, id, present_clock);
        queue.release(slot).unwrap_or_else(|_| process_exit(206));
        consumed = consumed.saturating_add(1);
    }
    // Более новые GPU frames могли остаться ready после последнего requested
    // present. Отменяем их явно и закрываем полученные capabilities.
    while let Ok(selection) = queue.select_mailbox(|_| true) {
        for stale in selection
            .dropped
            .iter()
            .take(selection.dropped_count)
            .flatten()
        {
            discard_frame(*stale);
        }
        discard_frame(selection.selected.2);
        queue
            .release(selection.selected.0)
            .unwrap_or_else(|_| process_exit(206));
    }
}

fn present_frame(
    display: Handle,
    feedback_endpoint: Handle,
    info: DisplayScanoutInfo,
    prepared: PreparedFrame,
    frame_id: u64,
    clock: &mut PresentClock,
) {
    let release_value = clock.advance();
    let feedback = present_frame_on_timeline(
        display,
        feedback_endpoint,
        info,
        prepared,
        frame_id,
        clock.timeline,
        release_value,
    );
    clock.frame.presented(
        feedback.sequence,
        feedback.actual_time_ns,
        feedback.refresh_interval_ns,
    );
    let _ = handle_close(prepared.buffer);
    let _ = handle_close(prepared.acquire);
}

fn present_frame_raw(
    display: Handle,
    feedback_endpoint: Handle,
    info: DisplayScanoutInfo,
    prepared: PreparedFrame,
    frame_id: u64,
) -> (DisplayPresentFeedback, Handle) {
    let release_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if release_value <= 0 {
        process_exit(187);
    }
    let release = Handle(release_value as u32);
    let feedback = present_frame_on_timeline(
        display,
        feedback_endpoint,
        info,
        prepared,
        frame_id,
        release,
        1,
    );
    (feedback, release)
}

#[allow(clippy::too_many_arguments)]
fn present_frame_on_timeline(
    display: Handle,
    feedback_endpoint: Handle,
    info: DisplayScanoutInfo,
    prepared: PreparedFrame,
    frame_id: u64,
    release: Handle,
    release_value: u64,
) -> DisplayPresentFeedback {
    let metrics = SurfaceMetrics::new(info.width, info.height, info.width, info.height, 1000);
    let surface = SurfaceCreateRequest::new(metrics, 3);
    let commit = SurfaceCommit::full_damage(SurfaceId(0x7fff), metrics, frame_id, 0);
    if surface.validate().is_err() || commit.validate().is_err() {
        process_exit(189);
    }
    let present = DisplayPresentRequest::from_buffer(
        frame_id,
        &prepared.descriptor,
        prepared.acquire_value,
        release_value,
    );
    let mut message = Message::EMPTY;
    message.header.opcode = DISPLAY_PRESENT_OPCODE;
    message.header.request_id = frame_id;
    message.header.payload_len = 64;
    message.header.handle_count = DISPLAY_PRESENT_HANDLE_COUNT;
    message.payload = present.encode_inline();
    message.handles[0] = TransferredHandle {
        handle: prepared.buffer,
        reserved: 0,
        rights: Rights::READ,
    };
    message.handles[1] = TransferredHandle {
        handle: prepared.acquire,
        reserved: 0,
        rights: Rights::WAIT,
    };
    message.handles[2] = TransferredHandle {
        handle: release,
        reserved: 0,
        rights: Rights::WRITE,
    };
    message.handles[3] = TransferredHandle {
        handle: feedback_endpoint,
        reserved: 0,
        rights: Rights::SEND,
    };
    if ipc_send(display, &message) != syscall::status::OK
        || sync_timeline_wait(&SyncTimelineWait::new(
            release,
            release_value,
            SYNC_TIMEOUT_INFINITE,
        )) != syscall::status::OK
    {
        process_exit(190);
    }

    let mut feedback_message = Message::EMPTY;
    if ipc_receive(feedback_endpoint, &mut feedback_message) != syscall::status::OK
        || feedback_message.header.opcode != DISPLAY_FEEDBACK_OPCODE
        || feedback_message.header.request_id != frame_id
        || feedback_message.header.payload_len != 64
        || feedback_message.header.handle_count != 0
    {
        process_exit(194);
    }
    let feedback = match DisplayPresentFeedback::decode_inline(&feedback_message.payload) {
        Ok(feedback) => feedback,
        Err(_) => process_exit(195),
    };
    if feedback.frame_id != frame_id || feedback.output != info.output {
        process_exit(196);
    }
    feedback
}

fn prepare_cpu_frame(info: DisplayScanoutInfo) -> PreparedFrame {
    let usage = BufferUsage::CPU_READ
        .union(BufferUsage::CPU_WRITE)
        .union(BufferUsage::RENDER_TARGET)
        .union(BufferUsage::SCANOUT);
    let domains = MemoryDomain::SYSTEM
        .union(MemoryDomain::HOST_VISIBLE)
        .union(MemoryDomain::SHARED);
    let descriptor = match GraphicsBufferDesc::linear(
        info.width,
        info.height,
        PixelFormatCode::B8G8R8A8_UNORM,
        usage,
        domains,
    ) {
        Ok(descriptor) => descriptor,
        Err(_) => process_exit(182),
    };
    let buffer_value = graphics_buffer_create(&descriptor);
    if buffer_value <= 0 {
        process_exit(183);
    }
    let buffer = Handle(buffer_value as u32);
    let mapped_length = descriptor.byte_size.div_ceil(4096) * 4096;
    let mapping = SharedMemoryMap {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        offset: 0,
        length: mapped_length,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };
    if rustos_runtime::shared_memory_map(buffer, &mapping) != syscall::status::ACCESS_DENIED {
        process_exit(184);
    }
    let address = graphics_buffer_map(buffer, &mapping);
    if address <= 0 {
        process_exit(185);
    }
    unsafe {
        (address as *mut u64).write_volatile(FRAME_MAGIC);
        ((address as *mut u32).add((info.width * info.height - 1) as usize))
            .write_volatile(0xff_24_80_ff);
    }
    if vm_unmap(address as u64, mapped_length) != syscall::status::OK {
        process_exit(186);
    }
    let acquire_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if acquire_value <= 0 {
        process_exit(187);
    }
    let acquire = Handle(acquire_value as u32);
    if sync_timeline_signal(&SyncTimelineSignal::new(acquire, 1)) != syscall::status::OK
        || sync_timeline_signal(&SyncTimelineSignal::new(acquire, 0))
            != syscall::status::INVALID_ARGUMENT
    {
        process_exit(188);
    }
    PreparedFrame {
        descriptor,
        buffer,
        acquire,
        acquire_value: 1,
    }
}

fn prepare_gpu_frame(
    width: u32,
    height: u32,
    frame_id: u64,
    scene_frame: Option<u32>,
    compositor_probe: bool,
    system_ui: bool,
    mailbox_slot: Option<u16>,
) -> PreparedFrame {
    let request = match (system_ui, compositor_probe, scene_frame) {
        (true, false, _) => GpuRenderFrame::system_ui_request(
            width,
            height,
            frame_id,
            mailbox_slot.unwrap_or_else(|| process_exit(203)),
        ),
        (false, true, _) => GpuRenderFrame::compositor_probe_request(width, height, frame_id),
        (false, false, Some(scene_frame)) => {
            GpuRenderFrame::aurora_request(width, height, frame_id, scene_frame)
        }
        (false, false, None) => GpuRenderFrame::request(width, height, frame_id),
        (true, true, _) => process_exit(203),
    };
    let mut message = Message::EMPTY;
    message.header.opcode = GPU_RENDER_REQUEST_OPCODE;
    message.header.request_id = request.frame_id;
    message.header.payload_len = 64;
    message.payload = request.encode_inline();
    if ipc_send(GPU_CONTROL_ENDPOINT, &message) != syscall::status::OK {
        process_exit(201);
    }
    let mut frame = Message::EMPTY;
    if ipc_receive(GPU_FRAME_ENDPOINT, &mut frame) != syscall::status::OK
        || frame.header.opcode != GPU_RENDERED_FRAME_OPCODE
        || frame.header.request_id != request.frame_id
        || frame.header.payload_len != 64
        || frame.header.handle_count != GPU_RENDERED_FRAME_HANDLE_COUNT
    {
        process_exit(202);
    }
    let rendered = match GpuRenderFrame::decode_inline(&frame.payload) {
        Ok(rendered)
            if rendered.fence_id != 0
                && rendered.width == width
                && rendered.height == height
                && rendered.flags == request.flags
                && rendered.scene_frame() == request.scene_frame() =>
        {
            rendered
        }
        _ => process_exit(203),
    };
    let _fence = rendered.fence_id;
    let acquire_value = rendered.acquire_value();
    let buffer = frame.handles[0].handle;
    let acquire = frame.handles[1].handle;
    let mut descriptor = empty_descriptor();
    if graphics_buffer_get_info(buffer, &mut descriptor) != syscall::status::OK
        || descriptor.validate().is_err()
        || descriptor.width != width
        || descriptor.height != height
        || descriptor.usage.contains(BufferUsage::CPU_WRITE)
        || !descriptor.usage.contains(BufferUsage::RENDER_TARGET)
        || !descriptor.usage.contains(BufferUsage::SCANOUT)
    {
        process_exit(204);
    }
    PreparedFrame {
        descriptor,
        buffer,
        acquire,
        acquire_value,
    }
}

fn prepared_ready(prepared: PreparedFrame) -> bool {
    sync_timeline_wait(&SyncTimelineWait::new(
        prepared.acquire,
        prepared.acquire_value,
        0,
    )) == syscall::status::OK
}

fn wait_prepared(prepared: PreparedFrame) {
    if sync_timeline_wait(&SyncTimelineWait::new(
        prepared.acquire,
        prepared.acquire_value,
        SYNC_TIMEOUT_INFINITE,
    )) != syscall::status::OK
    {
        process_exit(207);
    }
}

fn discard_frame(prepared: PreparedFrame) {
    // Полученные handles являются производными копиями. Диагностический
    // target уже проверен acquire fence; рабочий оконный Canvas использует
    // отдельный retained путь и от этого handle не зависит.
    if handle_close(prepared.buffer) != syscall::status::OK
        || handle_close(prepared.acquire) != syscall::status::OK
    {
        process_exit(205);
    }
}

fn empty_descriptor() -> GraphicsBufferDesc {
    // Поля немедленно полностью перезаписывает kernel syscall до чтения.
    unsafe { core::mem::zeroed() }
}

fn query_display(display: Handle, feedback: Handle) -> DisplayScanoutInfo {
    let mut query = Message::EMPTY;
    query.header.opcode = DISPLAY_QUERY_OPCODE;
    query.header.request_id = 1;
    query.header.handle_count = DISPLAY_QUERY_HANDLE_COUNT;
    query.handles[0] = TransferredHandle {
        handle: feedback,
        reserved: 0,
        rights: Rights::SEND,
    };
    if ipc_send(display, &query) != syscall::status::OK {
        process_exit(198);
    }
    let mut reply = Message::EMPTY;
    if ipc_receive(feedback, &mut reply) != syscall::status::OK
        || reply.header.opcode != DISPLAY_INFO_OPCODE
        || reply.header.request_id != 1
        || reply.header.payload_len != 64
        || reply.header.handle_count != 0
    {
        process_exit(199);
    }
    match DisplayScanoutInfo::decode_inline(&reply.payload) {
        Ok(info) => info,
        Err(_) => process_exit(200),
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(209)
}

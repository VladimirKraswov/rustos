//! Постоянный ring-3 `compositord` bootstrap service.
//!
//! Сервис владеет политикой кадров, но не scanout и не GPU. Обычный CPU
//! surface и GPU-only `GraphicsBuffer` проходят один atomic-present protocol.
//! Системное `gpu-demo` отправляет только bounded request; compositor pacing
//! выполняется vblank feedback'ом от изолированного displayd.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    display::{
        DisplayPresentFeedback, DisplayPresentRequest, DisplayScanoutInfo, DISPLAY_FEEDBACK_OPCODE,
        DISPLAY_INFO_OPCODE, DISPLAY_PRESENT_HANDLE_COUNT, DISPLAY_PRESENT_OPCODE,
        DISPLAY_QUERY_HANDLE_COUNT, DISPLAY_QUERY_OPCODE,
    },
    gpu::{
        demo_flag, GpuDemoRequest, GpuRenderFrame, GPU_DEMO_START_OPCODE,
        GPU_RENDERED_FRAME_HANDLE_COUNT, GPU_RENDERED_FRAME_OPCODE, GPU_RENDER_REQUEST_OPCODE,
    },
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain, PixelFormatCode},
    ipc::TransferredHandle,
    memory::MEMORY_ABI_VERSION,
    surface::{SurfaceCommit, SurfaceCreateRequest, SurfaceMetrics},
    sync::{
        SyncPoint, SyncTimelineCreate, SyncTimelineSignal, SyncTimelineWait, SYNC_TIMEOUT_INFINITE,
    },
};
use rustos_runtime::{
    graphics_buffer_create, graphics_buffer_get_info, graphics_buffer_map, handle_close,
    ipc_receive, ipc_send, process_exit, sync_timeline_create, sync_timeline_signal,
    sync_timeline_wait, syscall, vm_unmap, Handle, Message, Rights, SharedMemoryMap, VmFlags,
};

const FRAME_MAGIC: u64 = 0x5255_5354_4f53_4758;
const GPU_MODE_FLAG: u64 = 1 << 63;
const GPU_FRAME_ENDPOINT: Handle = Handle(4);
const GPU_CONTROL_ENDPOINT: Handle = Handle(5);
const DEMO_CONTROL_ENDPOINT: Handle = Handle(6);

#[derive(Clone, Copy)]
struct PreparedFrame {
    descriptor: GraphicsBufferDesc,
    buffer: Handle,
    acquire: Handle,
    acquire_value: u64,
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
    let mut frame_id = 1u64;
    // Стартовый desktop остаётся обычным CPU buffer. Иначе первый GPU кадр
    // навсегда зафиксировал бы размер renderd target равным scanout и не дал
    // бы приложениям создавать компактные оконные surfaces.
    let first = prepare_cpu_frame(info);
    present_frame(display_endpoint, feedback_endpoint, info, first, frame_id);

    if !gpu_mode {
        loop {
            let mut idle = Message::EMPTY;
            if ipc_receive(feedback_endpoint, &mut idle) != syscall::status::OK {
                process_exit(197);
            }
        }
    }

    loop {
        let mut control = Message::EMPTY;
        if ipc_receive(DEMO_CONTROL_ENDPOINT, &mut control) != syscall::status::OK
            || control.header.opcode != GPU_DEMO_START_OPCODE
            || control.header.payload_len != 64
            || control.header.handle_count != 0
            || control.header.sender_pid == 0
        {
            process_exit(210);
        }
        let request = match GpuDemoRequest::decode_inline(&control.payload) {
            Ok(request) => request,
            Err(_) => process_exit(211),
        };
        let windowed = request.flags & demo_flag::WINDOWED != 0;
        let render_width = if windowed { request.width } else { info.width };
        let render_height = if windowed {
            request.height
        } else {
            info.height
        };
        if windowed {
            // Bootstrap window compositor пока забирает pixels CPU readback'ом;
            // перед возвратом kernel обязана быть видна завершённая timeline.
            frame_id = frame_id.saturating_add(1);
            let frame = prepare_gpu_frame(
                render_width,
                render_height,
                frame_id,
                Some(request.first_frame),
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
            );
        }
    }
}

/// Держит до трёх GPU кадров впереди displayd. Если более свежий frame уже
/// готов, а старый опоздал к refresh boundary, stale buffer отбрасывается —
/// это mailbox semantics, а не растущая FIFO задержка.
fn present_swapchain(
    display: Handle,
    feedback: Handle,
    info: DisplayScanoutInfo,
    request: GpuDemoRequest,
    width: u32,
    height: u32,
    frame_id: &mut u64,
) {
    const IMAGE_COUNT: usize = 3;
    let mut queue: [Option<(u64, PreparedFrame)>; IMAGE_COUNT] = [None; IMAGE_COUNT];
    let mut submitted = 0u32;
    let mut consumed = 0u32;
    while consumed < request.frame_count {
        while submitted < request.frame_count {
            let Some(slot) = queue.iter_mut().find(|slot| slot.is_none()) else {
                break;
            };
            *frame_id = frame_id.saturating_add(1);
            let scene = request.first_frame.saturating_add(submitted);
            *slot = Some((
                *frame_id,
                prepare_gpu_frame(width, height, *frame_id, Some(scene)),
            ));
            submitted = submitted.saturating_add(1);
        }

        let oldest = queue
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.map(|(id, _)| (index, id)))
            .min_by_key(|(_, id)| *id)
            .map(|(index, _)| index)
            .unwrap_or_else(|| process_exit(206));
        let newest_ready = queue
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.filter(|(_, frame)| prepared_ready(*frame))
                    .map(|(id, _)| (index, id))
            })
            .max_by_key(|(_, id)| *id)
            .map(|(index, _)| index);
        let selected = newest_ready.unwrap_or(oldest);
        let selected_id = queue[selected].expect("selected swapchain frame").0;
        for slot in &mut queue {
            if slot.is_some_and(|(id, _)| id < selected_id) {
                let (_, stale) = slot.take().expect("stale frame");
                discard_frame(stale);
                consumed = consumed.saturating_add(1);
            }
        }
        let (id, prepared) = queue[selected].take().expect("selected frame");
        present_frame(display, feedback, info, prepared, id);
        consumed = consumed.saturating_add(1);
    }
    for slot in queue.into_iter().flatten() {
        discard_frame(slot.1);
    }
}

fn present_frame(
    display: Handle,
    feedback_endpoint: Handle,
    info: DisplayScanoutInfo,
    prepared: PreparedFrame,
    frame_id: u64,
) {
    let release_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if release_value <= 0 {
        process_exit(187);
    }
    let release = Handle(release_value as u32);
    let metrics = SurfaceMetrics::new(info.width, info.height, info.width, info.height, 1000);
    let surface = SurfaceCreateRequest::new(metrics, 3);
    let mut commit = SurfaceCommit::full_damage(Handle(0x7fff), prepared.buffer, metrics, frame_id);
    commit.acquire = SyncPoint::new(prepared.acquire, prepared.acquire_value);
    if surface.validate().is_err() || commit.validate().is_err() {
        process_exit(189);
    }
    let present = DisplayPresentRequest::from_buffer(
        frame_id,
        &prepared.descriptor,
        prepared.acquire_value,
        1,
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
        || sync_timeline_wait(&SyncTimelineWait::new(release, 1, SYNC_TIMEOUT_INFINITE))
            != syscall::status::OK
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
    if handle_close(prepared.buffer) != syscall::status::OK
        || handle_close(prepared.acquire) != syscall::status::OK
        || handle_close(release) != syscall::status::OK
    {
        process_exit(193);
    }
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
) -> PreparedFrame {
    let request = match scene_frame {
        Some(scene_frame) => GpuRenderFrame::aurora_request(width, height, frame_id, scene_frame),
        None => GpuRenderFrame::request(width, height, frame_id),
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
    // Полученные handles являются производными копиями. Оригинальный
    // GraphicsBuffer остаётся у renderd и будет прочитан оконным compositor'ом
    // после возврата scheduler'а в kernel desktop.
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

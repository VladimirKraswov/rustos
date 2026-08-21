//! Постоянный ring-3 `compositord` bootstrap service.
//!
//! Сервис формирует client-owned surface frame, публикует buffer в displayd и
//! блокируется на release timeline через wait-many. Ни pixel data, ни
//! process-local pointer через IPC не передаются.

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
        GpuRenderFrame, GPU_RENDERED_FRAME_HANDLE_COUNT, GPU_RENDERED_FRAME_OPCODE,
        GPU_RENDER_REQUEST_OPCODE,
    },
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain, PixelFormatCode},
    ipc::TransferredHandle,
    memory::MEMORY_ABI_VERSION,
    surface::{SurfaceCommit, SurfaceCreateRequest, SurfaceMetrics},
    sync::{
        SyncPoint, SyncTimelineCreate, SyncTimelineSignal, SyncWaitMany, SyncWaitMode,
        SYNC_TIMEOUT_INFINITE,
    },
};
use rustos_runtime::{
    graphics_buffer_create, graphics_buffer_get_info, graphics_buffer_map, handle_close,
    ipc_receive, ipc_send, process_exit, shared_memory_create, shared_memory_map,
    sync_timeline_create, sync_timeline_signal, sync_timeline_wait_many, syscall, vm_unmap, Handle,
    Message, Rights, SharedMemoryCreate, SharedMemoryMap, VmFlags,
};

const FRAME_MAGIC: u64 = 0x5255_5354_4f53_4758;
const GPU_MODE_FLAG: u64 = 1 << 63;
const GPU_FRAME_ENDPOINT: Handle = Handle(4);
const GPU_CONTROL_ENDPOINT: Handle = Handle(5);

struct PreparedFrame {
    descriptor: GraphicsBufferDesc,
    buffer: Handle,
    acquire: Handle,
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
    let prepared = if gpu_mode {
        prepare_gpu_frame(info)
    } else {
        prepare_cpu_frame(info)
    };
    let descriptor = prepared.descriptor;
    let buffer = prepared.buffer;
    let acquire = prepared.acquire;
    let mapping = SharedMemoryMap {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        offset: 0,
        length: descriptor.byte_size.div_ceil(4096) * 4096,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };

    let release_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if release_value <= 0 {
        process_exit(187);
    }
    let release = Handle(release_value as u32);

    let metrics = SurfaceMetrics::new(info.width, info.height, info.width, info.height, 1000);
    let surface = SurfaceCreateRequest::new(metrics, 3);
    let mut commit = SurfaceCommit::full_damage(Handle(0x7fff), buffer, metrics, 1);
    commit.acquire = SyncPoint::new(acquire, 1);
    if surface.validate().is_err() || commit.validate().is_err() {
        process_exit(189);
    }
    let present = DisplayPresentRequest::from_buffer(commit.frame_id, &descriptor, 1, 1);
    let mut message = Message::EMPTY;
    message.header.opcode = DISPLAY_PRESENT_OPCODE;
    message.header.request_id = commit.frame_id;
    message.header.payload_len = 64;
    message.header.handle_count = DISPLAY_PRESENT_HANDLE_COUNT;
    message.payload = present.encode_inline();
    message.handles[0] = TransferredHandle {
        handle: buffer,
        reserved: 0,
        rights: Rights::READ,
    };
    message.handles[1] = TransferredHandle {
        handle: acquire,
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
    if ipc_send(display_endpoint, &message) != syscall::status::OK {
        process_exit(190);
    }

    let points_create = SharedMemoryCreate {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        length: 4096,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    };
    let points_value = shared_memory_create(&points_create);
    if points_value <= 0 {
        process_exit(191);
    }
    let points_memory = Handle(points_value as u32);
    let points_address = shared_memory_map(
        points_memory,
        &SharedMemoryMap {
            length: 4096,
            ..mapping
        },
    );
    if points_address <= 0 {
        process_exit(192);
    }
    unsafe {
        (points_address as *mut SyncPoint).write(SyncPoint::new(acquire, 1));
        (points_address as *mut SyncPoint)
            .add(1)
            .write(SyncPoint::new(release, 1));
    }
    let wait = SyncWaitMany::new(
        points_memory,
        0,
        2,
        SyncWaitMode::ALL,
        SYNC_TIMEOUT_INFINITE,
    );
    if sync_timeline_wait_many(&wait) != syscall::status::OK
        || vm_unmap(points_address as u64, 4096) != syscall::status::OK
        || handle_close(points_memory) != syscall::status::OK
        || handle_close(buffer) != syscall::status::OK
        || handle_close(acquire) != syscall::status::OK
        || handle_close(release) != syscall::status::OK
    {
        process_exit(193);
    }
    let mut feedback_message = Message::EMPTY;
    if ipc_receive(feedback_endpoint, &mut feedback_message) != syscall::status::OK
        || feedback_message.header.opcode != DISPLAY_FEEDBACK_OPCODE
        || feedback_message.header.request_id != commit.frame_id
        || feedback_message.header.payload_len != 64
        || feedback_message.header.handle_count != 0
    {
        process_exit(194);
    }
    let feedback = match DisplayPresentFeedback::decode_inline(&feedback_message.payload) {
        Ok(feedback) => feedback,
        Err(_) => process_exit(195),
    };
    if feedback.frame_id != commit.frame_id || feedback.output != info.output {
        process_exit(196);
    }

    loop {
        let mut idle = Message::EMPTY;
        if ipc_receive(feedback_endpoint, &mut idle) != syscall::status::OK {
            process_exit(197);
        }
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
    // GraphicsBuffer — отдельный object kind; shared-memory syscall не имеет
    // права трактовать его как обычный byte container.
    if shared_memory_map(buffer, &mapping) != syscall::status::ACCESS_DENIED {
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
    }
}

fn prepare_gpu_frame(info: DisplayScanoutInfo) -> PreparedFrame {
    let request = GpuRenderFrame::request(info.width, info.height, 1);
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
                && rendered.width == info.width
                && rendered.height == info.height =>
        {
            rendered
        }
        _ => process_exit(203),
    };
    let _fence = rendered.fence_id;
    let buffer = frame.handles[0].handle;
    let acquire = frame.handles[1].handle;
    let mut descriptor = empty_descriptor();
    if graphics_buffer_get_info(buffer, &mut descriptor) != syscall::status::OK
        || descriptor.validate().is_err()
        || descriptor.width != info.width
        || descriptor.height != info.height
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

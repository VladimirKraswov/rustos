//! Изолированный ring-3 bootstrap 3D renderer.
//!
//! `renderd` не владеет scanout и не видит PCI/MMIO. Он получает только
//! `GpuRender` capability, создаёт VirGL context, рисует треугольник в
//! GraphicsBuffer без CPU mapping и передаёт buffer + acquire timeline
//! compositor'у. Маленький encoder позднее заменит Mesa, не меняя IPC ABI.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    gpu::{
        GpuContextCreate, GpuDeviceInfo, GpuRenderFrame, GpuResourceCreate, GpuResourceImport,
        GpuSubmit, GPU_ABI_VERSION, GPU_RENDERED_FRAME_HANDLE_COUNT, GPU_RENDERED_FRAME_OPCODE,
        GPU_RENDER_REQUEST_OPCODE,
    },
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain, PixelFormatCode},
    ipc::TransferredHandle,
    sync::{SyncTimelineCreate, SyncTimelineWait, SYNC_TIMEOUT_INFINITE},
};
use rustos_runtime::{
    gpu_completion_status, gpu_context_create, gpu_get_info, gpu_resource_create,
    gpu_resource_import, gpu_submit, graphics_buffer_create, handle_close, ipc_receive, ipc_send,
    process_exit, sync_timeline_create, sync_timeline_wait, syscall, Handle, Message, Rights,
};

const CONTROL_HANDLE: Handle = Handle(4);

#[no_mangle]
pub extern "C" fn _start(frame_endpoint: u64, render_capability: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(211);
    }
    let frame_endpoint = Handle(frame_endpoint as u32);
    let render = Handle(render_capability as u32);
    let mut info = GpuDeviceInfo {
        version: GPU_ABI_VERSION,
        size: core::mem::size_of::<GpuDeviceInfo>() as u16,
        reserved_header: 0,
        features: 0,
        max_command_bytes: 1,
        max_inflight: 1,
        max_contexts: 1,
        capset_id: 0,
        capset_version: 0,
        capset_size: 0,
        reserved: [0; 3],
    };
    if gpu_get_info(render, &mut info) != syscall::status::OK || info.validate().is_err() {
        process_exit(212);
    }
    let context_value = gpu_context_create(render, &GpuContextCreate::new(b"rustos-renderd"));
    if context_value <= 0 {
        process_exit(213);
    }
    let context = Handle(context_value as u32);

    let mut request_message = Message::EMPTY;
    if ipc_receive(CONTROL_HANDLE, &mut request_message) != syscall::status::OK
        || request_message.header.opcode != GPU_RENDER_REQUEST_OPCODE
        || request_message.header.handle_count != 0
        || request_message.header.payload_len != 64
    {
        process_exit(214);
    }
    let request = match GpuRenderFrame::decode_inline(&request_message.payload) {
        Ok(request) if request.fence_id == 0 => request,
        _ => process_exit(215),
    };

    let usage = BufferUsage::RENDER_TARGET
        .union(BufferUsage::SCANOUT)
        .union(BufferUsage::TRANSFER_SOURCE);
    let domains = MemoryDomain::SYSTEM.union(MemoryDomain::SHARED);
    let descriptor = match GraphicsBufferDesc::linear(
        request.width,
        request.height,
        PixelFormatCode::B8G8R8X8_UNORM,
        usage,
        domains,
    ) {
        Ok(descriptor) => descriptor,
        Err(_) => process_exit(216),
    };
    let buffer_value = graphics_buffer_create(&descriptor);
    if buffer_value <= 0 {
        process_exit(217);
    }
    let buffer = Handle(buffer_value as u32);
    let target_resource = gpu_resource_import(context, buffer, &GpuResourceImport::render_target());
    let vertex_resource = gpu_resource_create(context, &GpuResourceCreate::vertex_buffer(96));
    if target_resource <= 0 || vertex_resource <= 0 {
        process_exit(218);
    }

    let mut commands = [0u32; 768];
    let command_dwords = match rustos_virgl::encode_triangle(
        &mut commands,
        request.width,
        request.height,
        target_resource as u32,
        vertex_resource as u32,
    ) {
        Ok(length) => length,
        Err(_) => process_exit(219),
    };
    let timeline_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if timeline_value <= 0 {
        process_exit(220);
    }
    let timeline = Handle(timeline_value as u32);
    let submit = GpuSubmit::new(
        commands.as_ptr() as u64,
        (command_dwords * 4) as u32,
        timeline,
        1,
    );
    let fence = gpu_submit(context, &submit);
    if fence <= 0
        || sync_timeline_wait(&SyncTimelineWait::new(timeline, 1, SYNC_TIMEOUT_INFINITE))
            != syscall::status::OK
        || gpu_completion_status(context, fence as u64) != syscall::status::OK
    {
        process_exit(221);
    }

    let rendered = GpuRenderFrame {
        fence_id: fence as u64,
        ..request
    };
    let mut frame = Message::EMPTY;
    frame.header.opcode = GPU_RENDERED_FRAME_OPCODE;
    frame.header.request_id = request.frame_id;
    frame.header.payload_len = 64;
    frame.header.handle_count = GPU_RENDERED_FRAME_HANDLE_COUNT;
    frame.payload = rendered.encode_inline();
    frame.handles[0] = TransferredHandle {
        handle: buffer,
        reserved: 0,
        rights: Rights::READ,
    };
    frame.handles[1] = TransferredHandle {
        handle: timeline,
        reserved: 0,
        rights: Rights::WAIT,
    };
    if ipc_send(frame_endpoint, &frame) != syscall::status::OK
        || handle_close(buffer) != syscall::status::OK
        || handle_close(timeline) != syscall::status::OK
    {
        process_exit(222);
    }

    // Context остаётся жив, поэтому displayd продолжает scanout того же host
    // resource. Следующий request пока служит только точкой блокировки.
    let mut idle = Message::EMPTY;
    if ipc_receive(CONTROL_HANDLE, &mut idle) != syscall::status::OK {
        process_exit(223);
    }
    process_exit(224)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(229)
}

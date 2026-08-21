//! Изолированный ring-3 bootstrap 3D renderer.
//!
//! `renderd` не владеет scanout и не видит PCI/MMIO. Он получает только
//! `GpuRender` capability, создаёт VirGL context, а Mesa platform layer
//! формирует кадры в GraphicsBuffer без CPU mapping. Buffer + acquire timeline
//! передаются compositor'у; сервис остаётся жив и обрабатывает много кадров.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    gpu::{
        frame_flag, GpuContextCreate, GpuDeviceInfo, GpuRenderFrame, GpuResourceCreate,
        GpuResourceImport, GpuSubmit, GPU_ABI_VERSION, GPU_RENDERED_FRAME_HANDLE_COUNT,
        GPU_RENDERED_FRAME_OPCODE, GPU_RENDER_REQUEST_OPCODE,
    },
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain, PixelFormatCode},
    ipc::TransferredHandle,
    sync::{SyncTimelineCreate, SyncTimelineWait, SYNC_TIMEOUT_INFINITE},
};
use rustos_runtime::{
    gpu_completion_status, gpu_context_create, gpu_get_info, gpu_resource_create,
    gpu_resource_import, gpu_submit, graphics_buffer_create, ipc_receive, ipc_send, process_exit,
    sync_timeline_create, sync_timeline_wait, syscall, Handle, Message, Rights,
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

    let mut width = 0;
    let mut height = 0;
    let mut buffer = Handle::INVALID;
    let mut target_resource = 0u32;
    let mut vertex_resource = 0u32;
    let timeline_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if timeline_value <= 0 {
        process_exit(220);
    }
    let timeline = Handle(timeline_value as u32);
    let mut completion_value = 0u64;
    let mut mesa_context: Option<rustos_mesa::Context> = None;

    loop {
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
        if buffer == Handle::INVALID {
            width = request.width;
            height = request.height;
            let usage = BufferUsage::RENDER_TARGET
                .union(BufferUsage::SCANOUT)
                .union(BufferUsage::TRANSFER_SOURCE);
            let domains = MemoryDomain::SYSTEM.union(MemoryDomain::SHARED);
            let descriptor = match GraphicsBufferDesc::linear(
                width,
                height,
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
            buffer = Handle(buffer_value as u32);
            let imported =
                gpu_resource_import(context, buffer, &GpuResourceImport::render_target());
            let created = gpu_resource_create(context, &GpuResourceCreate::vertex_buffer(4096));
            if imported <= 0 || created <= 0 {
                process_exit(218);
            }
            target_resource = imported as u32;
            vertex_resource = created as u32;
        } else if request.width != width || request.height != height {
            // Mode-set создаёт новый service generation через supervisor;
            // один context никогда не смешивает ресурсы разных размеров.
            process_exit(216);
        }

        let mut commands = [0u32; 768];
        let command_dwords = if request.flags & frame_flag::AURORA_SHOWCASE != 0 {
            if mesa_context.is_none() {
                mesa_context = Some(
                    match rustos_mesa::Context::new(
                        rustos_mesa::VirglWinsysSurface {
                            width,
                            height,
                            color_resource: target_resource,
                            vertex_resource,
                        },
                        rustos_mesa::ApiProfile::OpenGlCore,
                    ) {
                        Ok(context) => context,
                        Err(_) => process_exit(219),
                    },
                );
            }
            match mesa_context
                .as_mut()
                .expect("Mesa context initialized")
                .render_aurora_frame(&mut commands, request.scene_frame())
            {
                Ok(length) => length,
                Err(_) => process_exit(219),
            }
        } else {
            match rustos_virgl::encode_triangle(
                &mut commands,
                width,
                height,
                target_resource,
                vertex_resource,
            ) {
                Ok(length) => length,
                Err(_) => process_exit(219),
            }
        };
        completion_value = completion_value.saturating_add(1);
        let submit = GpuSubmit::new(
            commands.as_ptr() as u64,
            (command_dwords * 4) as u32,
            timeline,
            completion_value,
        );
        let fence = gpu_submit(context, &submit);
        if fence <= 0
            || sync_timeline_wait(&SyncTimelineWait::new(
                timeline,
                completion_value,
                SYNC_TIMEOUT_INFINITE,
            )) != syscall::status::OK
            || gpu_completion_status(context, fence as u64) != syscall::status::OK
        {
            process_exit(221);
        }

        let mut rendered = GpuRenderFrame {
            fence_id: fence as u64,
            ..request
        };
        rendered.reserved[1] = completion_value;
        let mut frame = Message::EMPTY;
        frame.header.opcode = GPU_RENDERED_FRAME_OPCODE;
        frame.header.request_id = request.frame_id;
        frame.header.payload_len = 64;
        frame.header.handle_count = GPU_RENDERED_FRAME_HANDLE_COUNT;
        frame.payload = rendered.encode_inline();
        frame.handles[0] = TransferredHandle {
            handle: buffer,
            reserved: 0,
            rights: Rights::READ.union(Rights::TRANSFER),
        };
        frame.handles[1] = TransferredHandle {
            handle: timeline,
            reserved: 0,
            rights: Rights::WAIT.union(Rights::TRANSFER),
        };
        if ipc_send(frame_endpoint, &frame) != syscall::status::OK {
            process_exit(222);
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(229)
}

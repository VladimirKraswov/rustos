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
        frame_flag, virgl_format, GpuContextCreate, GpuDeviceInfo, GpuRenderFrame,
        GpuResourceCreate, GpuResourceImport, GpuSubmit, GPU_ABI_VERSION,
        GPU_RENDERED_FRAME_HANDLE_COUNT, GPU_RENDERED_FRAME_OPCODE, GPU_RENDER_REQUEST_OPCODE,
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
const SWAPCHAIN_IMAGES: usize = 3;

#[derive(Clone, Copy)]
struct RenderSlot {
    buffer: Handle,
    target_resource: u32,
    timeline: Handle,
    completion_value: u64,
    pending_fence: u64,
}

impl RenderSlot {
    const EMPTY: Self = Self {
        buffer: Handle::INVALID,
        target_resource: 0,
        timeline: Handle::INVALID,
        completion_value: 0,
        pending_fence: 0,
    };
}

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

    if info.max_inflight < SWAPCHAIN_IMAGES as u16 {
        process_exit(223);
    }
    let mut width = 0;
    let mut height = 0;
    let mut slots = [RenderSlot::EMPTY; SWAPCHAIN_IMAGES];
    for slot in &mut slots {
        let timeline_value = sync_timeline_create(&SyncTimelineCreate::new(0));
        if timeline_value <= 0 {
            process_exit(220);
        }
        slot.timeline = Handle(timeline_value as u32);
    }
    let mut vertex_resource = 0u32;
    let mut compositor_textures = [0u32; 3];
    let mut next_slot = 0usize;
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
        if slots[0].buffer == Handle::INVALID {
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
            for slot in &mut slots {
                let buffer_value = graphics_buffer_create(&descriptor);
                if buffer_value <= 0 {
                    process_exit(217);
                }
                slot.buffer = Handle(buffer_value as u32);
                let imported =
                    gpu_resource_import(context, slot.buffer, &GpuResourceImport::render_target());
                if imported <= 0 {
                    process_exit(218);
                }
                slot.target_resource = imported as u32;
            }
            let created = gpu_resource_create(context, &GpuResourceCreate::vertex_buffer(4096));
            if created <= 0 {
                process_exit(218);
            }
            vertex_resource = created as u32;
        } else if request.width != width || request.height != height {
            // Mode-set создаёт новый service generation через supervisor;
            // один context никогда не смешивает ресурсы разных размеров.
            process_exit(216);
        }

        let slot_index = next_slot;
        next_slot = (next_slot + 1) % SWAPCHAIN_IMAGES;
        let slot = &mut slots[slot_index];
        if slot.pending_fence != 0 {
            if sync_timeline_wait(&SyncTimelineWait::new(
                slot.timeline,
                slot.completion_value,
                SYNC_TIMEOUT_INFINITE,
            )) != syscall::status::OK
                || gpu_completion_status(context, slot.pending_fence) != syscall::status::OK
            {
                process_exit(221);
            }
            slot.pending_fence = 0;
        }

        let mut commands = [0u32; 768];
        let command_dwords = if request.flags & frame_flag::AURORA_SHOWCASE != 0 {
            if mesa_context.is_none() {
                mesa_context = Some(
                    match rustos_mesa::Context::new(
                        rustos_mesa::VirglWinsysSurface {
                            width,
                            height,
                            color_resource: slot.target_resource,
                            vertex_resource,
                        },
                        rustos_mesa::ApiProfile::OpenGlCore,
                    ) {
                        Ok(context) => context,
                        Err(_) => process_exit(219),
                    },
                );
            }
            let mesa = mesa_context.as_mut().expect("Mesa context initialized");
            if mesa.bind_color_resource(slot.target_resource).is_err() {
                process_exit(219);
            }
            match mesa.render_aurora_frame(&mut commands, request.scene_frame()) {
                Ok(length) => length,
                Err(_) => process_exit(219),
            }
        } else if request.flags & frame_flag::COMPOSITOR_PROBE != 0 {
            if compositor_textures[0] == 0 {
                for texture in &mut compositor_textures {
                    let created = gpu_resource_create(
                        context,
                        &GpuResourceCreate::sampled_texture(2, 2, virgl_format::B8G8R8A8_UNORM),
                    );
                    if created <= 0 {
                        process_exit(218);
                    }
                    *texture = created as u32;
                }
            }
            match encode_compositor_probe(
                &mut commands,
                width,
                height,
                slot.target_resource,
                compositor_textures,
            ) {
                Ok(length) => length,
                Err(()) => process_exit(219),
            }
        } else {
            match rustos_virgl::encode_triangle(
                &mut commands,
                width,
                height,
                slot.target_resource,
                vertex_resource,
            ) {
                Ok(length) => length,
                Err(_) => process_exit(219),
            }
        };
        slot.completion_value = slot.completion_value.saturating_add(1);
        let submit = GpuSubmit::new(
            commands.as_ptr() as u64,
            (command_dwords * 4) as u32,
            slot.timeline,
            slot.completion_value,
        );
        let fence = gpu_submit(context, &submit);
        if fence <= 0 {
            process_exit(221);
        }
        slot.pending_fence = fence as u64;

        let mut rendered = GpuRenderFrame {
            fence_id: fence as u64,
            ..request
        };
        rendered.reserved[1] = slot.completion_value;
        let mut frame = Message::EMPTY;
        frame.header.opcode = GPU_RENDERED_FRAME_OPCODE;
        frame.header.request_id = request.frame_id;
        frame.header.payload_len = 64;
        frame.header.handle_count = GPU_RENDERED_FRAME_HANDLE_COUNT;
        frame.payload = rendered.encode_inline();
        frame.handles[0] = TransferredHandle {
            handle: slot.buffer,
            reserved: 0,
            rights: Rights::READ.union(Rights::TRANSFER),
        };
        frame.handles[1] = TransferredHandle {
            handle: slot.timeline,
            reserved: 0,
            rights: Rights::WAIT.union(Rights::TRANSFER),
        };
        if ipc_send(frame_endpoint, &frame) != syscall::status::OK {
            process_exit(222);
        }
    }
}

/// Формирует минимальный, но настоящий compositor pass: две texture сначала
/// загружаются в device resources, затем hardware blit смешивает их прямо в
/// scanout-compatible GraphicsBuffer. Здесь нет CPU framebuffer и readback.
fn encode_compositor_probe(
    commands: &mut [u32],
    width: u32,
    height: u32,
    target_resource: u32,
    textures: [u32; 3],
) -> Result<usize, ()> {
    use rustos_virgl::{
        encode_composite_pass, encode_texture_upload, BlitRect, CompositeLayer, FORMAT_BGRA8888,
        FORMAT_BGRX8888,
    };

    // BGRA. Второй atlas хранит premultiplied alpha, как и обычная оконная
    // surface; это проверяет не только copy, но и blend stage compositor'а.
    const BACKGROUND: [u8; 16] = [
        40, 24, 12, 255, 112, 58, 20, 255, 82, 38, 18, 255, 180, 92, 36, 255,
    ];
    const PANEL: [u8; 16] = [
        170, 82, 28, 208, 188, 98, 36, 208, 150, 70, 24, 208, 204, 116, 44, 208,
    ];
    const ACCENT: [u8; 16] = [
        210, 112, 42, 232, 224, 132, 52, 232, 188, 92, 34, 232, 238, 150, 62, 232,
    ];

    let mut length = 0usize;
    length += encode_texture_upload(
        commands.get_mut(length..).ok_or(())?,
        textures[0],
        BlitRect::new(0, 0, 2, 2),
        4,
        &BACKGROUND,
    )
    .map_err(|_| ())?;
    length += encode_texture_upload(
        commands.get_mut(length..).ok_or(())?,
        textures[2],
        BlitRect::new(0, 0, 2, 2),
        4,
        &ACCENT,
    )
    .map_err(|_| ())?;
    length += encode_texture_upload(
        commands.get_mut(length..).ok_or(())?,
        textures[1],
        BlitRect::new(0, 0, 2, 2),
        4,
        &PANEL,
    )
    .map_err(|_| ())?;

    let panel_width = width.saturating_mul(2) / 3;
    let panel_height = height.saturating_mul(3) / 5;
    let layers = [
        CompositeLayer {
            resource: textures[0],
            format: FORMAT_BGRA8888,
            source: BlitRect::new(0, 0, 2, 2),
            destination: BlitRect::new(0, 0, width, height),
            linear_filter: true,
            alpha_blend: false,
        },
        CompositeLayer {
            resource: textures[1],
            format: FORMAT_BGRA8888,
            source: BlitRect::new(0, 0, 2, 2),
            destination: BlitRect::new(
                (width - panel_width) / 2,
                (height - panel_height) / 2,
                panel_width,
                panel_height,
            ),
            linear_filter: true,
            alpha_blend: true,
        },
        CompositeLayer {
            resource: textures[2],
            format: FORMAT_BGRA8888,
            source: BlitRect::new(0, 0, 2, 2),
            destination: BlitRect::new(
                (width - panel_width) / 2 + panel_width / 12,
                (height - panel_height) / 2 + panel_height / 8,
                panel_width.saturating_mul(5) / 6,
                (panel_height / 7).max(1),
            ),
            linear_filter: true,
            alpha_blend: true,
        },
    ];
    length += encode_composite_pass(
        commands.get_mut(length..).ok_or(())?,
        width,
        height,
        target_resource,
        FORMAT_BGRX8888,
        BlitRect::new(0, 0, width, height),
        &layers,
    )
    .map_err(|_| ())?;
    Ok(length)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(229)
}

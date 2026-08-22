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
        frame_flag, gpu_ui_checksum, gpu_ui_static_content_hash, virgl_format, GpuContextCreate,
        GpuDeviceInfo, GpuRenderFrame, GpuResourceCreate, GpuResourceImport, GpuSubmit,
        GpuUiFrameHeader, GpuUiLayer, GpuUiQuad, GPU_ABI_VERSION, GPU_MAX_COMMAND_BYTES,
        GPU_RENDERED_FRAME_HANDLE_COUNT, GPU_RENDERED_FRAME_OPCODE, GPU_RENDER_REQUEST_OPCODE,
        GPU_UI_STREAM_BYTES,
    },
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain, PixelFormatCode},
    ipc::TransferredHandle,
    sync::{SyncTimelineCreate, SyncTimelineWait, SYNC_TIMEOUT_INFINITE},
};
use rustos_runtime::{
    gpu_completion_status, gpu_context_create, gpu_get_info, gpu_resource_create,
    gpu_resource_destroy, gpu_resource_import, gpu_submit, graphics_buffer_create, ipc_receive,
    ipc_send, process_exit, shared_memory_map, sync_timeline_create, sync_timeline_wait, syscall,
    Handle, Message, Rights, SharedMemoryMap, VmFlags,
};
use rustos_system_assets::{wallpaper, Wallpaper, WallpaperId};

const CONTROL_HANDLE: Handle = Handle(4);
const UI_STREAM_HANDLE: Handle = Handle(7);
const SWAPCHAIN_IMAGES: usize = 3;
const UI_COMMAND_DWORDS: usize = GPU_MAX_COMMAND_BYTES as usize / 4;
const UI_BATCH_QUADS: usize = 2_700;
const UI_BATCH_VERTICES: usize = UI_BATCH_QUADS * 6;
const UI_FRAME_LAYERS: usize = 32;
const GLYPH_ATLAS_SIDE: u32 = 2_048;
const GLYPH_TILE_SIDE: u32 = rustos_system_fonts::GLYPH_SIDE as u32;
const GLYPH_ATLAS_ENTRIES: usize =
    (GLYPH_ATLAS_SIDE / GLYPH_TILE_SIDE) as usize * (GLYPH_ATLAS_SIDE / GLYPH_TILE_SIDE) as usize;
const GLYPH_BATCH: usize = 256;
const UI_FRAME_QUADS: usize = (GPU_UI_STREAM_BYTES
    - core::mem::size_of::<GpuUiFrameHeader>()
    - UI_FRAME_LAYERS * core::mem::size_of::<GpuUiLayer>())
    / core::mem::size_of::<GpuUiQuad>();

// Почти мегабайт временных данных нельзя класть на растущий user stack:
// защита от stack-clash намеренно не отображает столь большой разрыв одним
// fault. renderd однопоточен, syscall копирует command stream до возврата,
// поэтому один bounded `.bss` scratch безопасен и не требует allocator'а.
static mut UI_COMMAND_SCRATCH: [u32; UI_COMMAND_DWORDS] = [0; UI_COMMAND_DWORDS];
static mut UI_UPLOAD_COMMAND_SCRATCH: [u32; UI_COMMAND_DWORDS] = [0; UI_COMMAND_DWORDS];
static mut UI_VERTEX_SCRATCH: [rustos_virgl::Vertex; UI_BATCH_VERTICES] =
    [rustos_virgl::Vertex::new([0.0; 4], [0.0; 4]); UI_BATCH_VERTICES];
static mut UI_LAYER_SNAPSHOT: [GpuUiLayer; UI_FRAME_LAYERS] = [GpuUiLayer {
    id: 0,
    content_hash: 0,
    x: 0,
    y: 0,
    width: 0,
    height: 0,
    first_quad: 0,
    quad_count: 0,
    flags: 0,
    reserved_header: 0,
    reserved: [0; 2],
}; UI_FRAME_LAYERS];
static mut UI_FRAME_SNAPSHOT: [GpuUiQuad; UI_FRAME_QUADS] = [GpuUiQuad {
    x: 0,
    y: 0,
    width: 0,
    height: 0,
    colors: [0; 4],
    flags: 0,
    reserved: 0,
}; UI_FRAME_QUADS];
static mut GLYPH_UPLOAD_PIXELS: [u8; rustos_system_fonts::GLYPH_CAPACITY * 4] =
    [0; rustos_system_fonts::GLYPH_CAPACITY * 4];
static mut GLYPH_COMPOSITE_SCRATCH: [rustos_virgl::CompositeLayer; GLYPH_BATCH] =
    [rustos_virgl::CompositeLayer {
        resource: 0,
        format: rustos_virgl::FORMAT_BGRA8888,
        source: rustos_virgl::BlitRect::new(0, 0, 1, 1),
        destination: rustos_virgl::BlitRect::new(0, 0, 1, 1),
        linear_filter: true,
        alpha_blend: true,
    }; GLYPH_BATCH];

#[derive(Clone, Copy)]
struct CachedUiLayer {
    id: u64,
    content_hash: u64,
    static_content_hash: u64,
    resource: u32,
    width: u32,
    height: u32,
    surface_handle: u32,
    seen_frame: u64,
    surface_initialized: bool,
}

impl CachedUiLayer {
    const EMPTY: Self = Self {
        id: 0,
        content_hash: 0,
        static_content_hash: 0,
        resource: 0,
        width: 0,
        height: 0,
        surface_handle: 0,
        seen_frame: 0,
        surface_initialized: false,
    };
}

#[derive(Clone, Copy)]
struct GlyphAtlasEntry {
    character: u32,
    style: u32,
    color: u32,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl GlyphAtlasEntry {
    const EMPTY: Self = Self {
        character: 0,
        style: 0,
        color: 0,
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };
}

struct GlyphAtlas {
    resource: u32,
    entries: [GlyphAtlasEntry; GLYPH_ATLAS_ENTRIES],
    len: usize,
}

/// Один переиспользуемый GPU-only render target для системных 3D Canvas.
///
/// Canvas последовательно рисуется и копируется в независимую surface окна
/// внутри одного VirGL context. Порядок команд гарантирует, что следующий
/// экземпляр не перезапишет ресурс до завершения предыдущего copy. Ни одного
/// GPU -> CPU readback в этом пути нет.
struct EmbeddedCanvas3d {
    resource: u32,
    mesa: Option<rustos_mesa::Context>,
}

impl EmbeddedCanvas3d {
    const fn new(resource: u32) -> Self {
        Self {
            resource,
            mesa: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_into_layer(
        &mut self,
        context: Handle,
        submissions: &mut UiSubmissionQueue,
        vertex_resource: u32,
        pipeline_initialized: &mut bool,
        scene_frame: u32,
        layer_resource: u32,
        layer_width: u32,
        layer_height: u32,
        destination: rustos_virgl::BlitRect,
    ) -> Result<(), ()> {
        if self.mesa.is_none() {
            let mut mesa = rustos_mesa::Context::new(
                rustos_mesa::VirglWinsysSurface {
                    width: 800,
                    height: 450,
                    color_resource: self.resource,
                    vertex_resource,
                    color_format: rustos_virgl::FORMAT_BGRA8888,
                },
                rustos_mesa::ApiProfile::OpenGlCore,
            )
            .map_err(|_| ())?;
            mesa.configure_shared_pipeline([56, 57, 58], *pipeline_initialized)
                .map_err(|_| ())?;
            self.mesa = Some(mesa);
        }
        // Scene draw и последующий GPU copy образуют один submit. Раньше
        // каждый Canvas создавал два fence и принудительный drain после трёх
        // команд; два окна превращали лёгкую сцену из 48 vertices в цепочку
        // синхронных round-trip. Один ordered stream сохраняет тот же результат
        // и оставляет место финальному composition fence.
        let mut commands = [0u32; 1_024];
        let render_words = self
            .mesa
            .as_mut()
            .ok_or(())?
            .render_aurora_frame(&mut commands, scene_frame)
            .map_err(|_| ())?;
        let canvas_layer = rustos_virgl::CompositeLayer {
            resource: self.resource,
            format: rustos_virgl::FORMAT_BGRA8888,
            source: rustos_virgl::BlitRect::new(0, 0, 800, 450),
            destination,
            linear_filter: destination.width != 800 || destination.height != 450,
            alpha_blend: false,
        };
        let copy_words = rustos_virgl::encode_composite_pass(
            &mut commands[render_words..],
            layer_width,
            layer_height,
            layer_resource,
            rustos_virgl::FORMAT_BGRA8888,
            destination,
            core::slice::from_ref(&canvas_layer),
        )
        .map_err(|_| ())?;
        submit_ui_commands(context, submissions, &commands, render_words + copy_words)?;
        *pipeline_initialized = true;
        Ok(())
    }
}

impl GlyphAtlas {
    const fn new() -> Self {
        Self {
            resource: 0,
            entries: [GlyphAtlasEntry::EMPTY; GLYPH_ATLAS_ENTRIES],
            len: 0,
        }
    }
}

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

/// Независимый набор render targets одного размера.
///
/// Desktop и оконные 3D surfaces принципиально не делят swapchain. Раньше
/// первый SystemUI кадр фиксировал `renderd` на размере scanout, а запрос
/// Aurora 800×450 завершал весь сервис. Два pool сохраняют один аппаратный
/// context/pipeline, но имеют разные GraphicsBuffer и VBO.
struct RenderPool {
    width: u32,
    height: u32,
    slots: [RenderSlot; SWAPCHAIN_IMAGES],
    next_slot: usize,
    vertex_resource: u32,
}

impl RenderPool {
    fn new() -> Result<Self, ()> {
        let mut slots = [RenderSlot::EMPTY; SWAPCHAIN_IMAGES];
        for slot in &mut slots {
            let value = sync_timeline_create(&SyncTimelineCreate::new(0));
            if value <= 0 {
                return Err(());
            }
            slot.timeline = Handle(value as u32);
        }
        Ok(Self {
            width: 0,
            height: 0,
            slots,
            next_slot: 0,
            vertex_resource: 0,
        })
    }

    fn initialize(&mut self, context: Handle, width: u32, height: u32) -> Result<(), ()> {
        if self.slots[0].buffer != Handle::INVALID {
            return (self.width == width && self.height == height)
                .then_some(())
                .ok_or(());
        }
        let usage = BufferUsage::RENDER_TARGET
            .union(BufferUsage::SCANOUT)
            .union(BufferUsage::TRANSFER_SOURCE);
        let domains = MemoryDomain::SYSTEM.union(MemoryDomain::SHARED);
        let descriptor = GraphicsBufferDesc::linear(
            width,
            height,
            PixelFormatCode::B8G8R8X8_UNORM,
            usage,
            domains,
        )
        .map_err(|_| ())?;
        for slot in &mut self.slots {
            let value = graphics_buffer_create(&descriptor);
            if value <= 0 {
                return Err(());
            }
            slot.buffer = Handle(value as u32);
            let resource =
                gpu_resource_import(context, slot.buffer, &GpuResourceImport::render_target());
            if resource <= 0 {
                return Err(());
            }
            slot.target_resource = resource as u32;
        }
        let vertex = gpu_resource_create(
            context,
            &GpuResourceCreate::vertex_buffer(GPU_MAX_COMMAND_BYTES),
        );
        if vertex <= 0 {
            return Err(());
        }
        self.vertex_resource = vertex as u32;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn select(&mut self) -> (usize, &mut RenderSlot) {
        let index = self.next_slot;
        self.next_slot = (self.next_slot + 1) % SWAPCHAIN_IMAGES;
        (index, &mut self.slots[index])
    }
}

/// Три независимых timelines позволяют держать до трёх command batches
/// одновременно. После bounded drain финальный marker публикуется уже в
/// timeline конкретного swapchain buffer.
struct UiSubmissionQueue {
    timelines: [Handle; SWAPCHAIN_IMAGES],
    values: [u64; SWAPCHAIN_IMAGES],
    fences: [u64; SWAPCHAIN_IMAGES],
    count: usize,
}

impl UiSubmissionQueue {
    const fn new() -> Self {
        Self {
            timelines: [Handle::INVALID; SWAPCHAIN_IMAGES],
            values: [0; SWAPCHAIN_IMAGES],
            fences: [0; SWAPCHAIN_IMAGES],
            count: 0,
        }
    }
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

    let ui_mapping = SharedMemoryMap {
        version: rustos_abi::memory::MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        offset: 0,
        length: GPU_UI_STREAM_BYTES as u64,
        flags: VmFlags::READ,
    };
    let ui_stream = shared_memory_map(UI_STREAM_HANDLE, &ui_mapping);
    if ui_stream <= 0 {
        process_exit(230);
    }
    let ui_stream = ui_stream as *const u8;

    if info.max_inflight < SWAPCHAIN_IMAGES as u16 {
        process_exit(223);
    }
    let mut desktop_pool = RenderPool::new().unwrap_or_else(|()| process_exit(220));
    let mut window_pool = RenderPool::new().unwrap_or_else(|()| process_exit(220));
    let mut ui_submissions = UiSubmissionQueue::new();
    for timeline in &mut ui_submissions.timelines {
        let value = sync_timeline_create(&SyncTimelineCreate::new(0));
        if value <= 0 {
            process_exit(220);
        }
        *timeline = Handle(value as u32);
    }
    let mut compositor_textures = [0u32; 3];
    let mut wallpaper_textures = [0u32; 3];
    // Coverage-span backend не резервирует 16 MiB device memory для
    // экспериментального atlas до появления первого atlas primitive.
    let mut glyph_atlas = GlyphAtlas::new();
    let canvas_resource =
        gpu_resource_create(context, &GpuResourceCreate::window_surface(800, 450));
    if canvas_resource <= 0 {
        process_exit(218);
    }
    let mut canvas_3d = EmbeddedCanvas3d::new(canvas_resource as u32);
    let mut ui_layers = [CachedUiLayer::EMPTY; UI_FRAME_LAYERS];
    for (index, layer) in ui_layers.iter_mut().enumerate() {
        // 1, 8, 9 принадлежат desktop swapchain; 16..=18 — Aurora.
        // Независимые SystemUI surfaces занимают непересекающийся bounded
        // диапазон object handles одного VirGL context.
        layer.surface_handle = 24 + index as u32;
    }
    let mut ui_pipeline_initialized = false;
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
        let system_ui = request.flags & frame_flag::SYSTEM_UI != 0;
        let pool = if system_ui {
            &mut desktop_pool
        } else {
            &mut window_pool
        };
        if pool
            .initialize(context, request.width, request.height)
            .is_err()
        {
            // Размер desktop меняет новый service generation. Размер
            // оконной surface пока фиксирован на срок жизни одного клиента;
            // ошибка изолирована от второго pool и не повреждает SystemUI.
            process_exit(216);
        }
        let width = pool.width;
        let height = pool.height;
        let vertex_resource = pool.vertex_resource;
        let (_, slot) = pool.select();
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
        let command_dwords = if system_ui {
            0
        } else if request.flags & frame_flag::AURORA_SHOWCASE != 0 {
            if mesa_context.is_none() {
                let mut mesa = match rustos_mesa::Context::new(
                    rustos_mesa::VirglWinsysSurface {
                        width,
                        height,
                        color_resource: slot.target_resource,
                        vertex_resource,
                        color_format: rustos_virgl::FORMAT_BGRX8888,
                    },
                    rustos_mesa::ApiProfile::OpenGlCore,
                ) {
                    Ok(context) => context,
                    Err(_) => process_exit(219),
                };
                if mesa
                    .configure_shared_pipeline([16, 17, 18], ui_pipeline_initialized)
                    .is_err()
                {
                    process_exit(219);
                }
                mesa_context = Some(mesa);
            }
            let mesa = mesa_context.as_mut().expect("Mesa context initialized");
            if mesa.bind_color_resource(slot.target_resource).is_err() {
                process_exit(219);
            }
            match mesa.render_aurora_frame(&mut commands, request.scene_frame()) {
                Ok(length) => {
                    ui_pipeline_initialized = true;
                    length
                }
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
        let fence = if system_ui {
            match render_system_ui_frame(
                context,
                slot,
                ui_stream,
                width,
                height,
                vertex_resource,
                &mut wallpaper_textures,
                &mut glyph_atlas,
                &mut canvas_3d,
                &mut ui_layers,
                &mut ui_pipeline_initialized,
                &mut ui_submissions,
            ) {
                Ok(fence) => fence,
                Err(stage) => process_exit(230 + i32::from(stage)),
            }
        } else {
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
            fence as u64
        };
        slot.pending_fence = fence;

        let mut rendered = GpuRenderFrame {
            fence_id: fence,
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

/// Компилирует shared SystemUI stream в несколько bounded VirGL submissions.
/// CPU здесь строит только вершины/команды; покрытие, интерполяция, alpha blend
/// и запись каждого physical pixel выполняются host GPU.
#[allow(clippy::too_many_arguments)]
fn render_system_ui_frame(
    context: Handle,
    slot: &mut RenderSlot,
    stream: *const u8,
    width: u32,
    height: u32,
    vertex_resource: u32,
    wallpaper_resources: &mut [u32; 3],
    glyph_atlas: &mut GlyphAtlas,
    canvas_3d: &mut EmbeddedCanvas3d,
    layer_cache: &mut [CachedUiLayer; UI_FRAME_LAYERS],
    pipeline_initialized: &mut bool,
    submissions: &mut UiSubmissionQueue,
) -> Result<u64, u8> {
    let header = unsafe { (stream as *const GpuUiFrameHeader).read_volatile() };
    header.validate().map_err(|_| 1)?;
    if header.width != width
        || header.height != height
        || header.layer_count as usize > UI_FRAME_LAYERS
        || header.quad_count as usize > UI_FRAME_QUADS
    {
        return Err(1);
    }
    let transform_only = header.is_transform_only();
    let shared_layers = unsafe {
        core::slice::from_raw_parts(
            stream
                .add(core::mem::size_of::<GpuUiFrameHeader>())
                .cast::<GpuUiLayer>(),
            header.layer_count as usize,
        )
    };
    let shared_quads = unsafe {
        core::slice::from_raw_parts(
            stream
                .add(core::mem::size_of::<GpuUiFrameHeader>())
                .add(core::mem::size_of_val(shared_layers))
                .cast::<GpuUiQuad>(),
            header.quad_count as usize,
        )
    };
    if gpu_ui_checksum(shared_layers, shared_quads) != header.checksum {
        return Err(2);
    }
    let mut expected_first = 0u32;
    for (index, layer) in shared_layers.iter().enumerate() {
        if layer.validate(width, height, header.quad_count).is_err()
            || (!transform_only && layer.first_quad != expected_first)
            || (transform_only && (layer.first_quad != 0 || layer.quad_count != 0))
            || shared_layers[..index]
                .iter()
                .any(|previous| previous.id == layer.id)
        {
            return Err(2);
        }
        let first = layer.first_quad as usize;
        let end = first + layer.quad_count as usize;
        let layer_quads = &shared_quads[first..end];
        if !transform_only
            && (layer.quad_count == 0
                || rustos_abi::gpu::gpu_ui_content_hash(layer_quads) != layer.content_hash
                || layer_quads
                    .iter()
                    .any(|quad| quad.validate(layer.width, layer.height).is_err()))
        {
            return Err(2);
        }
        expected_first = expected_first.saturating_add(layer.quad_count);
    }
    if expected_first != header.quad_count {
        return Err(2);
    }
    // Kernel-side mailbox вправе заменить shared stream следующим кадром,
    // пока этот кадр ждёт GPU fence. Снимок делается до первого blocking
    // syscall, после чего renderer читает только собственную память.
    let layer_snapshot = unsafe { &mut *core::ptr::addr_of_mut!(UI_LAYER_SNAPSHOT) };
    layer_snapshot[..shared_layers.len()].copy_from_slice(shared_layers);
    let snapshot = unsafe { &mut *core::ptr::addr_of_mut!(UI_FRAME_SNAPSHOT) };
    snapshot[..shared_quads.len()].copy_from_slice(shared_quads);
    let layers = &layer_snapshot[..shared_layers.len()];
    let quads = &snapshot[..shared_quads.len()];

    // Большие wallpaper resources загружаются только при первом появлении.
    // В steady state stream содержит одну texture-ссылку вместо bitmap или
    // грубой сетки; приложения и kernel GUI не знают деталей VirGL upload.
    for id in quads.iter().filter_map(GpuUiQuad::wallpaper_id) {
        ensure_wallpaper_texture(context, submissions, wallpaper_resources, id).map_err(|_| 14)?;
    }

    let mut composites = [rustos_virgl::CompositeLayer {
        resource: 0,
        format: rustos_virgl::FORMAT_BGRA8888,
        source: rustos_virgl::BlitRect::new(0, 0, 1, 1),
        destination: rustos_virgl::BlitRect::new(0, 0, 1, 1),
        linear_filter: false,
        alpha_blend: true,
    }; UI_FRAME_LAYERS];

    for (order, layer) in layers.iter().enumerate() {
        let cache_index = layer_cache
            .iter()
            .position(|cached| cached.id == layer.id)
            .or_else(|| layer_cache.iter().position(|cached| cached.id == 0))
            .ok_or(3)?;
        if transform_only
            && (layer_cache[cache_index].resource == 0
                || layer_cache[cache_index].width != layer.width
                || layer_cache[cache_index].height != layer.height
                || layer_cache[cache_index].content_hash != layer.content_hash)
        {
            return Err(15);
        }
        if layer_cache[cache_index].resource != 0
            && (layer_cache[cache_index].width != layer.width
                || layer_cache[cache_index].height != layer.height)
        {
            release_cached_layer(context, submissions, &mut layer_cache[cache_index])
                .map_err(|_| 4)?;
        }
        if layer_cache[cache_index].resource == 0 {
            let created = gpu_resource_create(
                context,
                &GpuResourceCreate::window_surface(layer.width, layer.height),
            );
            if created <= 0 {
                return Err(5);
            }
            layer_cache[cache_index].id = layer.id;
            layer_cache[cache_index].resource = created as u32;
            layer_cache[cache_index].width = layer.width;
            layer_cache[cache_index].height = layer.height;
            layer_cache[cache_index].content_hash = 0;
            layer_cache[cache_index].surface_initialized = false;
        }
        let first = layer.first_quad as usize;
        let end = first + layer.quad_count as usize;
        if !transform_only && layer_cache[cache_index].content_hash != layer.content_hash {
            let static_content_hash = gpu_ui_static_content_hash(&quads[first..end]);
            let mut rasterizer = UiLayerRasterizer {
                context,
                submissions,
                vertex_resource,
                wallpaper_resources,
                glyph_atlas,
                canvas_3d,
                pipeline_initialized,
            };
            if layer_cache[cache_index].surface_initialized
                && layer_cache[cache_index].static_content_hash == static_content_hash
            {
                update_canvas_layers(
                    &mut rasterizer,
                    &layer_cache[cache_index],
                    &quads[first..end],
                )
                .map_err(|_| 6)?;
            } else {
                rasterize_ui_layer(
                    &mut rasterizer,
                    &mut layer_cache[cache_index],
                    &quads[first..end],
                )
                .map_err(|_| 6)?;
                layer_cache[cache_index].static_content_hash = static_content_hash;
            }
            layer_cache[cache_index].content_hash = layer.content_hash;
        }
        layer_cache[cache_index].seen_frame = header.frame_id;
        composites[order] = rustos_virgl::CompositeLayer {
            resource: layer_cache[cache_index].resource,
            format: rustos_virgl::FORMAT_BGRA8888,
            source: rustos_virgl::BlitRect::new(0, 0, layer.width, layer.height),
            destination: rustos_virgl::BlitRect::new(
                layer.x as u32,
                gpu_y(height, layer.y as u32, layer.height),
                layer.width,
                layer.height,
            ),
            linear_filter: false,
            alpha_blend: !layer.is_opaque(),
        };
    }

    for cached in layer_cache.iter_mut() {
        if cached.id != 0 && cached.seen_frame != header.frame_id {
            release_cached_layer(context, submissions, cached).map_err(|_| 7)?;
        }
    }
    // Все вспомогательные draw/copy принадлежат одному VirGL context и
    // предшествуют финальному composition в одной controlq. Не ждём их здесь:
    // completion финального fence транзитивно подтверждает весь кадр. Drain
    // нужен только если все три kernel inflight slots уже заняты и для
    // composition сейчас физически нет свободного descriptor.
    if submissions.count == SWAPCHAIN_IMAGES {
        drain_ui_submissions(context, submissions).map_err(|_| 8)?;
    }

    // Steady-state drag попадает только сюда: content_hash окна совпадает,
    // поэтому GPU получает один composition pass с новым destination rect.
    let commands = unsafe { &mut *core::ptr::addr_of_mut!(UI_COMMAND_SCRATCH) };
    let composite_words = rustos_virgl::encode_composite_pass(
        commands,
        width,
        height,
        slot.target_resource,
        rustos_virgl::FORMAT_BGRX8888,
        rustos_virgl::BlitRect::new(0, 0, width, height),
        &composites[..layers.len()],
    )
    .map_err(|_| 9)?;
    slot.completion_value = slot.completion_value.saturating_add(1);
    let submit = GpuSubmit::new(
        commands.as_ptr() as u64,
        (composite_words * 4) as u32,
        slot.timeline,
        slot.completion_value,
    );
    let fence = gpu_submit(context, &submit);
    if fence <= 0 {
        return Err(10);
    }
    // Compositord ждёт этот final fence до следующего SystemUI request.
    // Следовательно, auxiliary timelines уже завершены до повторного
    // использования их монотонных values и отдельный syscall wait не нужен.
    submissions.count = 0;
    Ok(fence as u64)
}

/// Ресурсы, общие для растеризации одного независимого UI-слоя.
///
/// Контекст явно отделён от содержимого `CachedUiLayer`: перемещение окна
/// меняет только transform слоя и вообще не вызывает этот код.
struct UiLayerRasterizer<'a> {
    context: Handle,
    submissions: &'a mut UiSubmissionQueue,
    vertex_resource: u32,
    wallpaper_resources: &'a mut [u32; 3],
    glyph_atlas: &'a mut GlyphAtlas,
    canvas_3d: &'a mut EmbeddedCanvas3d,
    pipeline_initialized: &'a mut bool,
}

fn rasterize_ui_layer(
    renderer: &mut UiLayerRasterizer<'_>,
    cached: &mut CachedUiLayer,
    quads: &[GpuUiQuad],
) -> Result<(), ()> {
    let commands = unsafe { &mut *core::ptr::addr_of_mut!(UI_COMMAND_SCRATCH) };
    let clear_quad = GpuUiQuad::solid(0, 0, cached.width as u16, cached.height as u16, 0);
    let mut clear_vertices = [rustos_virgl::Vertex::new([0.0; 4], [0.0; 4]); 6];
    quad_vertices(clear_quad, cached.width, cached.height, &mut clear_vertices);
    let clear_words = rustos_virgl::encode_layer_mesh_pass(
        commands,
        cached.width,
        cached.height,
        cached.resource,
        renderer.vertex_resource,
        &clear_vertices,
        [0.0; 4],
        cached.surface_handle,
        !cached.surface_initialized,
        !*renderer.pipeline_initialized,
        true,
    )
    .map_err(|_| ())?;
    submit_ui_commands(
        renderer.context,
        renderer.submissions,
        commands,
        clear_words,
    )?;
    cached.surface_initialized = true;
    *renderer.pipeline_initialized = true;

    let mut cursor = 0usize;
    while cursor < quads.len() {
        if let Some((_instance_id, scene_frame)) = quads[cursor].canvas_3d_info() {
            let quad = quads[cursor];
            renderer.canvas_3d.render_into_layer(
                renderer.context,
                renderer.submissions,
                renderer.vertex_resource,
                renderer.pipeline_initialized,
                scene_frame,
                cached.resource,
                cached.width,
                cached.height,
                rustos_virgl::BlitRect::new(
                    u32::from(quad.x),
                    gpu_y(cached.height, u32::from(quad.y), u32::from(quad.height)),
                    u32::from(quad.width),
                    u32::from(quad.height),
                ),
            )?;
            cursor += 1;
            continue;
        }
        if quads[cursor].glyph_info().is_some() {
            let glyph_layers = unsafe { &mut *core::ptr::addr_of_mut!(GLYPH_COMPOSITE_SCRATCH) };
            let mut count = 0usize;
            while cursor < quads.len()
                && count < GLYPH_BATCH
                && quads[cursor].glyph_info().is_some()
            {
                let quad = quads[cursor];
                let source = ensure_glyph_atlas_entry(
                    renderer.context,
                    renderer.submissions,
                    renderer.glyph_atlas,
                    quad,
                )?;
                glyph_layers[count] = rustos_virgl::CompositeLayer {
                    resource: renderer.glyph_atlas.resource,
                    format: rustos_virgl::FORMAT_BGRA8888,
                    source,
                    destination: rustos_virgl::BlitRect::new(
                        u32::from(quad.x),
                        gpu_y(cached.height, u32::from(quad.y), u32::from(quad.height)),
                        u32::from(quad.width),
                        u32::from(quad.height),
                    ),
                    linear_filter: true,
                    alpha_blend: true,
                };
                count += 1;
                cursor += 1;
            }
            let words = rustos_virgl::encode_composite_pass(
                commands,
                cached.width,
                cached.height,
                cached.resource,
                rustos_virgl::FORMAT_BGRA8888,
                rustos_virgl::BlitRect::new(0, 0, cached.width, cached.height),
                &glyph_layers[..count],
            )
            .map_err(|_| ())?;
            submit_ui_commands(renderer.context, renderer.submissions, commands, words)?;
            continue;
        }
        if let Some(id) = quads[cursor].wallpaper_id() {
            let quad = quads[cursor];
            let image = wallpaper_from_id(id).ok_or(())?;
            let resource = renderer.wallpaper_resources[id as usize];
            let (source_x, source_y, source_width, source_height) =
                wallpaper_cover_crop(image, u32::from(quad.width), u32::from(quad.height));
            let wallpaper_layer = rustos_virgl::CompositeLayer {
                resource,
                format: rustos_virgl::FORMAT_BGRA8888,
                source: rustos_virgl::BlitRect::new(
                    source_x,
                    image.height - source_y - source_height,
                    source_width,
                    source_height,
                ),
                destination: rustos_virgl::BlitRect::new(
                    u32::from(quad.x),
                    gpu_y(cached.height, u32::from(quad.y), u32::from(quad.height)),
                    u32::from(quad.width),
                    u32::from(quad.height),
                ),
                linear_filter: true,
                alpha_blend: false,
            };
            let words = rustos_virgl::encode_composite_pass(
                commands,
                cached.width,
                cached.height,
                cached.resource,
                rustos_virgl::FORMAT_BGRA8888,
                rustos_virgl::BlitRect::new(0, 0, cached.width, cached.height),
                core::slice::from_ref(&wallpaper_layer),
            )
            .map_err(|_| ())?;
            submit_ui_commands(renderer.context, renderer.submissions, commands, words)?;
            cursor += 1;
            continue;
        }
        let vertices = unsafe { &mut *core::ptr::addr_of_mut!(UI_VERTEX_SCRATCH) };
        let mut count = 0usize;
        while cursor < quads.len()
            && count < UI_BATCH_QUADS
            && quads[cursor].wallpaper_id().is_none()
            && quads[cursor].glyph_info().is_none()
            && quads[cursor].canvas_3d_info().is_none()
        {
            quad_vertices(
                quads[cursor],
                cached.width,
                cached.height,
                &mut vertices[count * 6..count * 6 + 6],
            );
            count += 1;
            cursor += 1;
        }
        let words = rustos_virgl::encode_layer_mesh_pass(
            commands,
            cached.width,
            cached.height,
            cached.resource,
            renderer.vertex_resource,
            &vertices[..count * 6],
            [0.0; 4],
            cached.surface_handle,
            false,
            false,
            false,
        )
        .map_err(|_| ())?;
        submit_ui_commands(renderer.context, renderer.submissions, commands, words)?;
    }
    drain_ui_submissions(renderer.context, renderer.submissions)
}

/// Обновляет только анимированные GPU Canvas внутри уже готового retained
/// слоя. Chrome, заголовок, текст и рамка окна не меняются между кадрами и
/// потому не проходят повторную растеризацию. Если в слое нет Canvas,
/// совпадение static hash означает обычную замену метаданных без GPU работы.
fn update_canvas_layers(
    renderer: &mut UiLayerRasterizer<'_>,
    cached: &CachedUiLayer,
    quads: &[GpuUiQuad],
) -> Result<(), ()> {
    for quad in quads {
        let Some((_instance_id, scene_frame)) = quad.canvas_3d_info() else {
            continue;
        };
        renderer.canvas_3d.render_into_layer(
            renderer.context,
            renderer.submissions,
            renderer.vertex_resource,
            renderer.pipeline_initialized,
            scene_frame,
            cached.resource,
            cached.width,
            cached.height,
            rustos_virgl::BlitRect::new(
                u32::from(quad.x),
                gpu_y(cached.height, u32::from(quad.y), u32::from(quad.height)),
                u32::from(quad.width),
                u32::from(quad.height),
            ),
        )?;
    }
    Ok(())
}

/// Возвращает source rectangle glyph в постоянном BGRA coverage atlas. Новый
/// символ растеризуется/загружается один раз; повторные окна и кадры делят
/// один device-local resource, не копируя bitmap в SystemUI stream.
fn ensure_glyph_atlas_entry(
    context: Handle,
    submissions: &mut UiSubmissionQueue,
    atlas: &mut GlyphAtlas,
    quad: GpuUiQuad,
) -> Result<rustos_virgl::BlitRect, ()> {
    if atlas.resource == 0 {
        let resource = gpu_resource_create(
            context,
            &GpuResourceCreate::sampled_texture(
                GLYPH_ATLAS_SIDE,
                GLYPH_ATLAS_SIDE,
                virgl_format::B8G8R8A8_UNORM,
            ),
        );
        if resource <= 0 {
            return Err(());
        }
        atlas.resource = resource as u32;
    }
    let (character, style, crop_x, crop_y) = quad.glyph_info().ok_or(())?;
    let entry_index = atlas.entries[..atlas.len].iter().position(|entry| {
        entry.character == character as u32 && entry.style == style && entry.color == quad.colors[0]
    });
    let index = if let Some(index) = entry_index {
        index
    } else {
        if atlas.len >= atlas.entries.len() {
            return Err(());
        }
        let raster = rustos_system_fonts::rasterize(character, decode_glyph_style(style)?);
        if raster.width == 0
            || raster.height == 0
            || u32::from(raster.width) > GLYPH_TILE_SIDE
            || u32::from(raster.height) > GLYPH_TILE_SIDE
        {
            return Err(());
        }
        let index = atlas.len;
        let columns = GLYPH_ATLAS_SIDE / GLYPH_TILE_SIDE;
        let tile_x = index as u32 % columns;
        let tile_y = index as u32 / columns;
        let entry = GlyphAtlasEntry {
            character: character as u32,
            style,
            color: quad.colors[0],
            x: (tile_x * GLYPH_TILE_SIDE) as u16,
            y: (tile_y * GLYPH_TILE_SIDE) as u16,
            width: raster.width,
            height: raster.height,
        };
        let pixels = unsafe { &mut *core::ptr::addr_of_mut!(GLYPH_UPLOAD_PIXELS) };
        let width = raster.width as usize;
        let height = raster.height as usize;
        let [red, green, blue, source_alpha] = quad.colors[0].to_le_bytes();
        for gpu_row in 0..height {
            let source_row = height - gpu_row - 1;
            for x in 0..width {
                // SDF нельзя использовать как готовую alpha без специального
                // smoothstep shader: это раздувает контур. Храним точное
                // anti-aliased coverage системного rasterizer'а.
                let coverage = raster.pixels[source_row * width + x];
                let alpha = scale_channel(source_alpha, coverage);
                let offset = (gpu_row * width + x) * 4;
                pixels[offset..offset + 4].copy_from_slice(&[
                    scale_channel(blue, coverage),
                    scale_channel(green, coverage),
                    scale_channel(red, coverage),
                    alpha,
                ]);
            }
        }
        let byte_count = width * height * 4;
        let commands = unsafe { &mut *core::ptr::addr_of_mut!(UI_UPLOAD_COMMAND_SCRATCH) };
        let words = rustos_virgl::encode_texture_upload(
            commands,
            atlas.resource,
            rustos_virgl::BlitRect::new(
                u32::from(entry.x),
                u32::from(entry.y),
                u32::from(entry.width),
                u32::from(entry.height),
            ),
            4,
            &pixels[..byte_count],
        )
        .map_err(|_| ())?;
        submit_ui_commands(context, submissions, commands, words)?;
        atlas.entries[index] = entry;
        atlas.len += 1;
        index
    };

    let entry = atlas.entries[index];
    let source_right = u32::from(crop_x)
        .checked_add(u32::from(quad.width))
        .ok_or(())?;
    let source_bottom = u32::from(crop_y)
        .checked_add(u32::from(quad.height))
        .ok_or(())?;
    if source_right > u32::from(entry.width) || source_bottom > u32::from(entry.height) {
        return Err(());
    }
    Ok(rustos_virgl::BlitRect::new(
        u32::from(entry.x) + u32::from(crop_x),
        u32::from(entry.y) + u32::from(entry.height) - source_bottom,
        u32::from(quad.width),
        u32::from(quad.height),
    ))
}

fn decode_glyph_style(style: u32) -> Result<rustos_system_fonts::Style, ()> {
    use rustos_abi::gpu::ui_glyph_style;
    if !ui_glyph_style::valid(style) {
        return Err(());
    }
    Ok(rustos_system_fonts::Style {
        family: if style & ui_glyph_style::SANS != 0 {
            rustos_system_fonts::Family::Sans
        } else {
            rustos_system_fonts::Family::Console
        },
        weight: if style & ui_glyph_style::BOLD != 0 {
            rustos_system_fonts::Weight::Bold
        } else {
            rustos_system_fonts::Weight::Regular
        },
        italic: style & ui_glyph_style::ITALIC != 0,
        size: ((style & ui_glyph_style::SIZE_MASK) >> ui_glyph_style::SIZE_SHIFT) as u16,
    })
}

fn scale_channel(channel: u8, alpha: u8) -> u8 {
    ((u16::from(channel) * u16::from(alpha) + 127) / 255) as u8
}

fn release_cached_layer(
    context: Handle,
    submissions: &mut UiSubmissionQueue,
    cached: &mut CachedUiLayer,
) -> Result<(), ()> {
    if cached.resource == 0 {
        return Ok(());
    }
    if cached.surface_initialized {
        let commands = unsafe { &mut *core::ptr::addr_of_mut!(UI_COMMAND_SCRATCH) };
        let words = rustos_virgl::encode_destroy_surface(commands, cached.surface_handle)
            .map_err(|_| ())?;
        submit_ui_commands(context, submissions, commands, words)?;
        drain_ui_submissions(context, submissions)?;
    }
    if gpu_resource_destroy(context, cached.resource) != syscall::status::OK {
        return Err(());
    }
    let surface_handle = cached.surface_handle;
    *cached = CachedUiLayer {
        surface_handle,
        ..CachedUiLayer::EMPTY
    };
    Ok(())
}

fn wallpaper_from_id(id: u32) -> Option<Wallpaper> {
    match id {
        0 => Some(wallpaper(WallpaperId::SpringRiver)),
        1 => Some(wallpaper(WallpaperId::AutumnRiver)),
        2 => Some(wallpaper(WallpaperId::WinterField)),
        _ => None,
    }
}

/// Лениво создаёт полноразмерную GPU texture из встроенного block-compressed
/// ресурса. Декодирование выполняется один раз в ring 3, а не в kernel и не
/// на каждом кадре. Строки загружаются снизу вверх, согласуя top-left asset
/// coordinates с bottom-left Gallium texture coordinates.
fn ensure_wallpaper_texture(
    context: Handle,
    queue: &mut UiSubmissionQueue,
    resources: &mut [u32; 3],
    id: u32,
) -> Result<u32, ()> {
    let index = usize::try_from(id).map_err(|_| ())?;
    let slot = resources.get_mut(index).ok_or(())?;
    if *slot != 0 {
        return Ok(*slot);
    }
    let image = wallpaper_from_id(id).ok_or(())?;
    let created = gpu_resource_create(
        context,
        &GpuResourceCreate::sampled_texture(
            image.width,
            image.height,
            virgl_format::B8G8R8A8_UNORM,
        ),
    );
    if created <= 0 {
        return Err(());
    }
    *slot = created as u32;

    // Один upload несёт до 11 полных BGRA rows (~55 KiB). Texture загружается
    // примерно 66 fences вместо прежних 1440 и затем живёт весь срок renderd.
    const CHUNK_ROWS: u32 = 11;
    const MAX_UPLOAD_BYTES: usize = 1280 * CHUNK_ROWS as usize * 4;
    let mut bytes = [0u8; MAX_UPLOAD_BYTES];
    let commands = unsafe { &mut *core::ptr::addr_of_mut!(UI_UPLOAD_COMMAND_SCRATCH) };
    let mut source_y = 0u32;
    while source_y < image.height {
        let rows = (image.height - source_y).min(CHUNK_ROWS);
        for row in 0..rows {
            // First upload row имеет меньшую Gallium Y. Поэтому top-left
            // source chunk разворачивается внутри bounded staging buffer.
            let destination_row = rows - row - 1;
            for source_x in 0..image.width {
                let color = image.pixel(source_x, source_y + row);
                let base = ((destination_row * image.width + source_x) * 4) as usize;
                bytes[base..base + 4].copy_from_slice(&[color.b, color.g, color.r, u8::MAX]);
            }
        }
        let byte_count = (rows * image.width * 4) as usize;
        let dwords = rustos_virgl::encode_texture_upload(
            commands,
            *slot,
            rustos_virgl::BlitRect::new(0, image.height - source_y - rows, image.width, rows),
            4,
            &bytes[..byte_count],
        )
        .map_err(|_| ())?;
        submit_ui_commands(context, queue, commands, dwords)?;
        source_y += rows;
    }
    drain_ui_submissions(context, queue)?;
    Ok(*slot)
}

fn wallpaper_cover_crop(image: Wallpaper, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let destination_ratio_wide =
        u64::from(width) * u64::from(image.height) > u64::from(height) * u64::from(image.width);
    let (sample_width, sample_height) = if destination_ratio_wide {
        (
            image.width,
            (u64::from(image.width) * u64::from(height) / u64::from(width.max(1))).max(1) as u32,
        )
    } else {
        (
            (u64::from(image.height) * u64::from(width) / u64::from(height.max(1))).max(1) as u32,
            image.height,
        )
    };
    (
        image.width.saturating_sub(sample_width) / 2,
        image.height.saturating_sub(sample_height) / 2,
        sample_width,
        sample_height,
    )
}

fn quad_vertices(quad: GpuUiQuad, width: u32, height: u32, output: &mut [rustos_virgl::Vertex]) {
    let left = physical_to_clip_x(u32::from(quad.x), width);
    let right = physical_to_clip_x(u32::from(quad.x) + u32::from(quad.width), width);
    let top = physical_to_clip_y(u32::from(quad.y), height);
    let bottom = physical_to_clip_y(u32::from(quad.y) + u32::from(quad.height), height);
    let colors = quad.colors.map(unpack_color);
    let vertices = [
        rustos_virgl::Vertex::new([left, top, 0.0, 1.0], colors[0]),
        rustos_virgl::Vertex::new([right, top, 0.0, 1.0], colors[1]),
        rustos_virgl::Vertex::new([right, bottom, 0.0, 1.0], colors[2]),
        rustos_virgl::Vertex::new([left, top, 0.0, 1.0], colors[0]),
        rustos_virgl::Vertex::new([right, bottom, 0.0, 1.0], colors[2]),
        rustos_virgl::Vertex::new([left, bottom, 0.0, 1.0], colors[3]),
    ];
    output.copy_from_slice(&vertices);
}

fn physical_to_clip_x(value: u32, extent: u32) -> f32 {
    value as f32 * 2.0 / extent as f32 - 1.0
}

/// SystemUI использует top-left origin, Gallium render target — bottom-left.
fn physical_to_clip_y(value: u32, extent: u32) -> f32 {
    1.0 - value as f32 * 2.0 / extent as f32
}

fn gpu_y(target_height: u32, top: u32, height: u32) -> u32 {
    target_height.saturating_sub(top.saturating_add(height))
}

fn unpack_color(color: u32) -> [f32; 4] {
    color
        .to_le_bytes()
        .map(|channel| f32::from(channel) / 255.0)
}

fn submit_ui_commands(
    context: Handle,
    queue: &mut UiSubmissionQueue,
    commands: &[u32],
    dwords: usize,
) -> Result<(), ()> {
    if queue.count == SWAPCHAIN_IMAGES {
        drain_ui_submissions(context, queue)?;
    }
    let index = queue.count;
    queue.values[index] = queue.values[index].saturating_add(1);
    let submit = GpuSubmit::new(
        commands.as_ptr() as u64,
        u32::try_from(dwords.checked_mul(4).ok_or(())?).map_err(|_| ())?,
        queue.timelines[index],
        queue.values[index],
    );
    let fence = gpu_submit(context, &submit);
    if fence <= 0 {
        return Err(());
    }
    queue.fences[index] = fence as u64;
    queue.count += 1;
    Ok(())
}

fn drain_ui_submissions(context: Handle, queue: &mut UiSubmissionQueue) -> Result<(), ()> {
    for index in 0..queue.count {
        if sync_timeline_wait(&SyncTimelineWait::new(
            queue.timelines[index],
            queue.values[index],
            SYNC_TIMEOUT_INFINITE,
        )) != syscall::status::OK
            || gpu_completion_status(context, queue.fences[index]) != syscall::status::OK
        {
            return Err(());
        }
    }
    queue.count = 0;
    Ok(())
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

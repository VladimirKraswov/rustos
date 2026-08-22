//! Virtio GPU display/render driver (Virtio 1.x PCI и MMIO transport).
//!
//! 2D command set остаётся надёжным fallback. Если device предлагает VirGL,
//! отдельный ring-3 `renderd` получает контекст, импортирует GraphicsBuffer и
//! отправляет fenced 3D command stream через асинхронную control queue.

use core::ptr;

use rustos_abi::{
    gpu::{feature, GpuDeviceInfo, GpuResourceCreate, GPU_ABI_VERSION, GPU_MAX_COMMAND_BYTES},
    graphics_buffer::{GraphicsBufferDesc, PixelFormatCode},
};
use rustos_video::{
    select_startup_mode, ConnectorInfo, ConnectorKind, CpuPixelFormat, CpuSurface, DisplayMode,
    ModeSetError, PresentStats, Rect, ScanoutCapabilities, ScanoutError, StartupModePolicy,
};

use crate::memory::{self, FrameBlock};

#[cfg(target_arch = "aarch64")]
use super::virtqueue_mmio::ModernMmioTransport as GpuTransport;
use super::{edid, TransportError};
#[cfg(target_arch = "x86_64")]
use super::{pci::discover_virtio_gpu, virtqueue::ModernTransport as GpuTransport};

const MAX_SCANOUTS: usize = 16;
// Preferred + до 16 EDID timings + полный набор стандартных режимов. Лимит
// остаётся bounded, но больше не отбрасывает low-resolution fallback после
// Retina/UltraWide timings монитора.
const MAX_MODES: usize = 48;
const GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_GET_EDID: u32 = 0x010a;
const CMD_GET_CAPSET_INFO: u32 = 0x0108;
const CMD_CTX_CREATE: u32 = 0x0200;
const CMD_CTX_DESTROY: u32 = 0x0201;
const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
const CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
const CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const CMD_SUBMIT_3D: u32 = 0x0207;
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_EDID: u32 = 0x1104;
const RESP_OK_CAPSET_INFO: u32 = 0x1102;
const FLAG_FENCE: u32 = 1;
const RESOURCE_FLAG_Y_0_TOP: u32 = 1;
const MAX_RENDER_RESOURCES: usize = 4;
const NO_GRAPHICS_OBJECT: u16 = u16::MAX;
const MIN_WIDTH: u32 = 640;
const MIN_HEIGHT: u32 = 480;
const MAX_WIDTH: u32 = 3840;
const MAX_HEIGHT: u32 = 2160;
/// Retina host сообщает virtio-gpu физический native mode (например,
/// 2880×1800), но без DPI scaling такой desktop получается слишком мелким.
/// Native mode остаётся доступен для `DISPLAY MODE`, а при загрузке GUI
/// выбирает комфортный широкий logical scanout не больше 1600×900.
const STARTUP_MAX_WIDTH: u32 = 1600;
const STARTUP_MAX_HEIGHT: u32 = 900;

#[repr(C)]
#[derive(Clone, Copy)]
struct ControlHeader {
    kind: u32,
    flags: u32,
    fence_id: u64,
    context_id: u32,
    ring_index: u8,
    padding: [u8; 3],
}

impl ControlHeader {
    const ZERO: Self = Self {
        kind: 0,
        flags: 0,
        fence_id: 0,
        context_id: 0,
        ring_index: 0,
        padding: [0; 3],
    };
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DisplayOne {
    rect: GpuRect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DisplayInfoResponse {
    header: ControlHeader,
    displays: [DisplayOne; MAX_SCANOUTS],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GetEdidRequest {
    header: ControlHeader,
    scanout: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EdidResponse {
    header: ControlHeader,
    size: u32,
    padding: u32,
    bytes: [u8; 1024],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Create2dRequest {
    header: ControlHeader,
    resource: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryEntry {
    address: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AttachBackingRequest {
    header: ControlHeader,
    resource: u32,
    entries: u32,
    entry: MemoryEntry,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceRequest {
    header: ControlHeader,
    resource: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SetScanoutRequest {
    header: ControlHeader,
    rect: GpuRect,
    scanout: u32,
    resource: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TransferRequest {
    header: ControlHeader,
    rect: GpuRect,
    offset: u64,
    resource: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FlushRequest {
    header: ControlHeader,
    rect: GpuRect,
    resource: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GetCapsetInfoRequest {
    header: ControlHeader,
    index: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapsetInfoResponse {
    header: ControlHeader,
    id: u32,
    max_version: u32,
    max_size: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ContextCreateRequest {
    header: ControlHeader,
    name_length: u32,
    context_init: u32,
    name: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Create3dRequest {
    header: ControlHeader,
    resource: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    samples: u32,
    flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Submit3dRequest {
    header: ControlHeader,
    size: u32,
    padding: u32,
}

/// Wire layout `virtio_gpu_transfer_host_3d`. В отличие от 2D transfer этот
/// запрос копирует уже отрисованный host/GPU resource обратно в attached
/// system-memory backing, чтобы software compositor мог включить поверхность
/// в обычное окно без повторной CPU-растеризации.
#[repr(C)]
#[derive(Clone, Copy)]
struct Transfer3dRequest {
    header: ControlHeader,
    box_3d: GpuBox,
    offset: u64,
    resource: u32,
    level: u32,
    stride: u32,
    layer_stride: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuBox {
    x: u32,
    y: u32,
    z: u32,
    width: u32,
    height: u32,
    depth: u32,
}

#[derive(Clone, Copy)]
struct Resource {
    id: u32,
    backing: FrameBlock,
    bytes: u32,
}

#[derive(Clone, Copy)]
struct RenderResource {
    used: bool,
    id: u32,
    context: u32,
    graphics_object: u16,
    width: u32,
    height: u32,
    has_backing: bool,
}

impl RenderResource {
    const EMPTY: Self = Self {
        used: false,
        id: 0,
        context: 0,
        graphics_object: NO_GRAPHICS_OBJECT,
        width: 0,
        height: 0,
        has_backing: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RenderCompletion {
    pub fence_id: u64,
    pub succeeded: bool,
}

pub struct VirtioGpu {
    transport: GpuTransport,
    scanout: u32,
    resource: Resource,
    next_resource: u32,
    mode: DisplayMode,
    modes: [DisplayMode; MAX_MODES],
    mode_count: usize,
    preferred_mode: DisplayMode,
    edid_valid: bool,
    width_mm: u16,
    height_mm: u16,
    fence: u64,
    active_scanout_resource: u32,
    capset_id: u32,
    capset_version: u32,
    capset_size: u32,
    render_context: u32,
    render_resources: [RenderResource; MAX_RENDER_RESOURCES],
}

impl VirtioGpu {
    pub fn initialize(fallback: DisplayMode) -> Result<Self, ModeSetError> {
        #[cfg(target_arch = "x86_64")]
        let transport = {
            let regions = discover_virtio_gpu().ok_or(ModeSetError::DeviceLost)?;
            GpuTransport::initialize(regions).map_err(map_transport)?
        };
        #[cfg(target_arch = "aarch64")]
        let transport = GpuTransport::initialize().map_err(map_transport)?;
        let placeholder = Resource {
            id: 0,
            backing: FrameBlock { phys: 0, frames: 0 },
            bytes: 0,
        };
        let mut gpu = Self {
            transport,
            scanout: 0,
            resource: placeholder,
            next_resource: 1,
            mode: fallback,
            modes: [fallback; MAX_MODES],
            mode_count: 0,
            preferred_mode: fallback,
            edid_valid: false,
            width_mm: 0,
            height_mm: 0,
            fence: 0,
            active_scanout_resource: 0,
            capset_id: 0,
            capset_version: 0,
            capset_size: 0,
            render_context: 0,
            render_resources: [RenderResource::EMPTY; MAX_RENDER_RESOURCES],
        };
        gpu.discover_capset();
        let (scanout, preferred) = gpu.display_info().unwrap_or((0, fallback));
        gpu.scanout = scanout;
        gpu.add_mode(preferred);
        let connector_preferred = gpu.read_edid().unwrap_or(preferred);
        gpu.preferred_mode = connector_preferred;
        // Полезные wide modes остаются доступны даже если виртуальный EDID
        // содержит только preferred timing. Virtio-gpu 2D scanout допускает
        // любой ресурс в пределах безопасного размера драйвера.
        for (width, height) in [
            (3840, 2160),
            (3840, 1600),
            (3440, 1440),
            (3200, 1800),
            (2880, 1800),
            (2560, 1600),
            (2560, 1440),
            (2560, 1080),
            (2048, 1152),
            (1920, 1200),
            (1920, 1080),
            (1680, 1050),
            (1600, 900),
            (1440, 900),
            (1440, 810),
            (1366, 768),
            (1280, 1024),
            (1280, 800),
            (1280, 720),
            (1152, 648),
            (1024, 768),
            (1024, 600),
            (800, 600),
            (640, 480),
        ] {
            gpu.add_mode(DisplayMode {
                width,
                height,
                stride_pixels: width,
                format: CpuPixelFormat::Bgr888,
                refresh_millihertz: 60_000,
            });
        }
        let connector_preferred = DisplayMode {
            format: CpuPixelFormat::Bgr888,
            stride_pixels: connector_preferred.width,
            ..connector_preferred
        };
        let startup = select_startup_mode(
            connector_preferred,
            fallback,
            StartupModePolicy::desktop(STARTUP_MAX_WIDTH, STARTUP_MAX_HEIGHT),
        );
        // Автоматически выведенный integer-fit mode обязан быть виден через
        // DISPLAY MODES наравне с EDID и стандартными режимами.
        gpu.add_mode(startup);
        let resource = gpu.allocate_resource(startup)?;
        if let Err(error) = gpu.set_scanout_resource(resource.id, startup) {
            let _ = gpu.detach_resource(resource.id);
            let _ = gpu.unref_resource(resource.id);
            let _ = memory::free(resource.backing);
            return Err(error);
        }
        gpu.resource = resource;
        gpu.mode = startup;
        Ok(gpu)
    }

    pub const fn mode(&self) -> DisplayMode {
        self.mode
    }

    pub const fn connector(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Virtual,
            connected: true,
            preferred_mode: self.preferred_mode,
            width_mm: self.width_mm,
            height_mm: self.height_mm,
        }
    }

    pub const fn capabilities(&self) -> ScanoutCapabilities {
        ScanoutCapabilities {
            page_flip: false,
            vsync_event: false,
            hardware_cursor: false,
            multiple_outputs: false,
        }
    }

    pub const fn edid_valid(&self) -> bool {
        self.edid_valid
    }

    pub fn virgl_ready(&self) -> bool {
        self.render_info().is_some()
    }

    pub fn output_count(&self) -> u32 {
        self.transport.num_scanouts()
    }

    pub fn modes(&self, output: &mut [DisplayMode]) -> usize {
        let count = output.len().min(self.mode_count);
        output[..count].copy_from_slice(&self.modes[..count]);
        count
    }

    pub fn set_mode(&mut self, requested: DisplayMode) -> Result<DisplayMode, ModeSetError> {
        if requested.width == self.mode.width && requested.height == self.mode.height {
            return Ok(self.mode);
        }
        if requested.format != CpuPixelFormat::Bgr888
            || !self.modes[..self.mode_count]
                .iter()
                .any(|mode| mode.width == requested.width && mode.height == requested.height)
        {
            return Err(ModeSetError::UnsupportedMode);
        }
        let requested = DisplayMode {
            stride_pixels: requested.width,
            format: CpuPixelFormat::Bgr888,
            ..requested
        };
        let replacement = self.allocate_resource(requested)?;
        if let Err(error) = self.set_scanout_resource(replacement.id, requested) {
            let _ = self.detach_resource(replacement.id);
            let _ = self.unref_resource(replacement.id);
            let _ = memory::free(replacement.backing);
            return Err(error);
        }

        let previous = self.resource;
        self.resource = replacement;
        self.mode = requested;
        // SET_SCANOUT уже отвязал старый resource. Освобождаем DMA backing
        // только после подтверждённого DETACH; при ошибке безопаснее оставить
        // bounded leak, чем дать device доступ к повторно выданным кадрам.
        if self.detach_resource(previous.id).is_ok() {
            let _ = self.unref_resource(previous.id);
            let _ = memory::free(previous.backing);
        }
        Ok(requested)
    }

    pub fn present(
        &mut self,
        source: CpuSurface<'_>,
        damage: &[Rect],
        sequence: u64,
    ) -> Result<PresentStats, ScanoutError> {
        if source.width() != self.mode.width || source.height() != self.mode.height {
            return Err(ScanoutError::InvalidSurface);
        }
        self.ensure_primary_scanout()
            .map_err(|_| ScanoutError::DeviceLost)?;
        let bounds = Rect::new(0, 0, self.mode.width, self.mode.height);
        let target = self.resource.backing.phys as *mut u32;
        let mut rectangles = 0u32;
        let mut pixels = 0u64;
        for requested in damage.iter().copied() {
            let rect = requested.intersection(bounds);
            if rect.is_empty() {
                continue;
            }
            for y in rect.y as u32..rect.bottom() as u32 {
                let row = source
                    .row(y, rect.x as u32, rect.width)
                    .ok_or(ScanoutError::InvalidSurface)?;
                let destination =
                    unsafe { target.add(y as usize * self.mode.width as usize + rect.x as usize) };
                if source.format() == CpuPixelFormat::Bgr888 {
                    unsafe { ptr::copy_nonoverlapping(row.as_ptr(), destination, row.len()) };
                } else {
                    for (offset, raw) in row.iter().copied().enumerate() {
                        let converted = CpuPixelFormat::Bgr888.pack(source.format().unpack(raw));
                        unsafe { destination.add(offset).write(converted) };
                    }
                }
            }
            self.transfer(rect).map_err(|_| ScanoutError::DeviceLost)?;
            self.flush(rect).map_err(|_| ScanoutError::DeviceLost)?;
            rectangles = rectangles.saturating_add(1);
            pixels = pixels.saturating_add(rect.area());
        }
        Ok(PresentStats {
            sequence,
            rectangles,
            pixels,
        })
    }

    /// Публикует full-frame из capability-backed scatter/gather памяти.
    ///
    /// Driver получает только функцию преобразования page index в физический
    /// адрес. Поэтому object table, capability handles и process policy не
    /// протекают в virtio protocol layer. Копирование выполняется построчно и
    /// корректно пересекает границы физических extents.
    pub fn present_pages<PhysicalPage>(
        &mut self,
        descriptor: GraphicsBufferDesc,
        mut physical_page: PhysicalPage,
        sequence: u64,
    ) -> Result<PresentStats, ScanoutError>
    where
        PhysicalPage: FnMut(usize) -> Option<u64>,
    {
        const PAGE_BYTES: usize = 4096;

        if descriptor.validate().is_err()
            || descriptor.width != self.mode.width
            || descriptor.height != self.mode.height
            || !matches!(
                descriptor.format,
                PixelFormatCode::B8G8R8X8_UNORM | PixelFormatCode::B8G8R8A8_UNORM
            )
        {
            return Err(ScanoutError::InvalidSurface);
        }
        self.ensure_primary_scanout()
            .map_err(|_| ScanoutError::DeviceLost)?;
        let plane = descriptor.planes[0];
        let row_bytes = usize::try_from(descriptor.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(ScanoutError::InvalidSurface)?;
        let stride = plane.stride_bytes as usize;
        let plane_offset =
            usize::try_from(plane.offset).map_err(|_| ScanoutError::InvalidSurface)?;
        let target = self.resource.backing.phys as *mut u8;
        for y in 0..descriptor.height as usize {
            let mut source_offset = plane_offset
                .checked_add(y.checked_mul(stride).ok_or(ScanoutError::InvalidSurface)?)
                .ok_or(ScanoutError::InvalidSurface)?;
            let destination_offset = y
                .checked_mul(row_bytes)
                .ok_or(ScanoutError::InvalidSurface)?;
            let mut copied = 0usize;
            while copied < row_bytes {
                let page_index = source_offset / PAGE_BYTES;
                let within_page = source_offset % PAGE_BYTES;
                let chunk = (PAGE_BYTES - within_page).min(row_bytes - copied);
                let physical = physical_page(page_index).ok_or(ScanoutError::InvalidSurface)?;
                // SAFETY: graphics object владеет полной source page, chunk
                // не пересекает её границу; target resource выделен driver'ом
                // минимум на width*height*4 и source/target не совпадают.
                unsafe {
                    ptr::copy_nonoverlapping(
                        (physical as *const u8).add(within_page),
                        target.add(destination_offset + copied),
                        chunk,
                    )
                };
                copied += chunk;
                source_offset += chunk;
            }
        }
        let full = Rect::new(0, 0, self.mode.width, self.mode.height);
        self.transfer(full)
            .and_then(|_| self.flush(full))
            .map_err(|_| ScanoutError::DeviceLost)?;
        Ok(PresentStats {
            sequence,
            rectangles: 1,
            pixels: u64::from(self.mode.width) * u64::from(self.mode.height),
        })
    }

    /// Возможности render path. Отсутствие VirGL выражается `None`, а не
    /// software fallback: caller не сможет случайно принять CPU за GPU.
    pub fn render_info(&self) -> Option<GpuDeviceInfo> {
        (self.transport.virgl_supported() && self.capset_id != 0).then_some(GpuDeviceInfo {
            version: GPU_ABI_VERSION,
            size: core::mem::size_of::<GpuDeviceInfo>() as u16,
            reserved_header: 0,
            features: feature::VIRGL | feature::ASYNC_FENCE | feature::ZERO_COPY_SCANOUT,
            max_command_bytes: GPU_MAX_COMMAND_BYTES,
            // Transport держит четыре DMA slot, однако process ABI намеренно
            // допускает один незавершённый submit на bootstrap-контекст. Так
            // нельзя случайно переиспользовать timeline/ресурс раньше fence.
            max_inflight: 1,
            max_contexts: 1,
            capset_id: self.capset_id,
            capset_version: self.capset_version,
            capset_size: self.capset_size,
            reserved: [0; 3],
        })
    }

    pub fn create_render_context(&mut self, context: u32, name: &[u8]) -> Result<(), ModeSetError> {
        if self.render_info().is_none() || context == 0 || self.render_context != 0 {
            return Err(ModeSetError::RequiresReboot);
        }
        let mut request = ContextCreateRequest {
            header: self.header(CMD_CTX_CREATE),
            name_length: name.len().min(64) as u32,
            // CONTEXT_INIT — отдельный negotiated feature. Classic VirGL
            // context выбирает capset не этим полем, поэтому оно равно нулю.
            context_init: 0,
            name: [0; 64],
        };
        request.header.context_id = context;
        request.name[..request.name_length as usize]
            .copy_from_slice(&name[..request.name_length as usize]);
        self.command_nodata(&request)?;
        self.render_context = context;
        Ok(())
    }

    pub fn import_render_target(
        &mut self,
        context: u32,
        graphics_object: u16,
        descriptor: GraphicsBufferDesc,
        backing: FrameBlock,
    ) -> Result<u32, ModeSetError> {
        if context != self.render_context
            || context == 0
            || descriptor.format != PixelFormatCode::B8G8R8X8_UNORM
            || backing.frames * 4096 < descriptor.byte_size
            || descriptor.byte_size > u64::from(u32::MAX)
            || self
                .render_resources
                .iter()
                .any(|resource| resource.used && resource.graphics_object == graphics_object)
        {
            return Err(ModeSetError::UnsupportedMode);
        }
        let slot_index = self
            .render_resources
            .iter()
            .position(|candidate| !candidate.used)
            .ok_or(ModeSetError::OutOfMemory)?;
        let resource = self.create_3d_resource(
            context,
            2,
            GPU_FORMAT_B8G8R8X8_UNORM,
            (1 << 1) | (1 << 8),
            descriptor.width,
            descriptor.height,
            1,
            1,
            RESOURCE_FLAG_Y_0_TOP,
        )?;
        let attach = AttachBackingRequest {
            header: self.header(CMD_RESOURCE_ATTACH_BACKING),
            resource,
            entries: 1,
            entry: MemoryEntry {
                address: backing.phys,
                length: descriptor.byte_size as u32,
                padding: 0,
            },
        };
        if self.command_nodata(&attach).is_err() || self.attach_context(context, resource).is_err()
        {
            let _ = self.detach_resource(resource);
            let _ = self.unref_resource(resource);
            return Err(ModeSetError::DeviceLost);
        }
        self.render_resources[slot_index] = RenderResource {
            used: true,
            id: resource,
            context,
            graphics_object,
            width: descriptor.width,
            height: descriptor.height,
            has_backing: true,
        };
        Ok(resource)
    }

    pub fn create_render_resource(
        &mut self,
        context: u32,
        request: GpuResourceCreate,
    ) -> Result<u32, ModeSetError> {
        if context != self.render_context || request.validate().is_err() {
            return Err(ModeSetError::UnsupportedMode);
        }
        let slot_index = self
            .render_resources
            .iter()
            .position(|candidate| !candidate.used)
            .ok_or(ModeSetError::OutOfMemory)?;
        let resource = self.create_3d_resource(
            context,
            request.target,
            request.format,
            request.bind,
            request.width,
            request.height,
            request.depth,
            request.array_size,
            0,
        )?;
        if self.attach_context(context, resource).is_err() {
            let _ = self.unref_resource(resource);
            return Err(ModeSetError::DeviceLost);
        }
        self.render_resources[slot_index] = RenderResource {
            used: true,
            id: resource,
            context,
            graphics_object: NO_GRAPHICS_OBJECT,
            width: request.width,
            height: request.height,
            has_backing: false,
        };
        Ok(resource)
    }

    pub fn submit_render(&mut self, context: u32, commands: &[u8]) -> Result<u64, ModeSetError> {
        if context != self.render_context || !valid_virgl_stream(commands) {
            return Err(ModeSetError::UnsupportedMode);
        }
        let mut request = Submit3dRequest {
            header: self.header(CMD_SUBMIT_3D),
            size: commands.len() as u32,
            padding: 0,
        };
        request.header.context_id = context;
        let fence = request.header.fence_id;
        let prefix = unsafe {
            core::slice::from_raw_parts(
                (&request as *const Submit3dRequest).cast::<u8>(),
                core::mem::size_of::<Submit3dRequest>(),
            )
        };
        self.transport
            .submit_bytes(prefix, commands, core::mem::size_of::<ControlHeader>())
            .map_err(map_transport)?;
        Ok(fence)
    }

    pub fn poll_render(&mut self) -> Result<Option<RenderCompletion>, ModeSetError> {
        self.transport
            .poll_completion()
            .map(|completion| {
                completion.map(|completion| RenderCompletion {
                    fence_id: completion.fence_id,
                    succeeded: completion.response_kind == RESP_OK_NODATA,
                })
            })
            .map_err(map_transport)
    }

    /// Дожидается конкретного fence только при аварийном teardown процесса.
    /// Обычный render path всегда неблокирующий и завершается из timer bottom
    /// half. Здесь ожидание необходимо, чтобы QEMU/реальный GPU не продолжал
    /// DMA после освобождения capability-backed кадров.
    pub fn drain_render(&mut self, fence_id: u64) -> Result<RenderCompletion, ModeSetError> {
        for _ in 0..50_000_000 {
            if let Some(completion) = self.poll_render()? {
                if completion.fence_id == fence_id {
                    return Ok(completion);
                }
            }
            core::hint::spin_loop();
        }
        Err(ModeSetError::DeviceLost)
    }

    pub fn present_imported(
        &mut self,
        graphics_object: u16,
        sequence: u64,
    ) -> Result<Option<PresentStats>, ScanoutError> {
        let Some(resource) = self
            .render_resources
            .iter()
            .copied()
            .find(|resource| resource.used && resource.graphics_object == graphics_object)
        else {
            return Ok(None);
        };
        if resource.width != self.mode.width || resource.height != self.mode.height {
            return Err(ScanoutError::InvalidSurface);
        }
        if self.active_scanout_resource != resource.id {
            self.set_scanout_resource(resource.id, self.mode)
                .map_err(|_| ScanoutError::DeviceLost)?;
        }
        self.flush_resource(
            resource.id,
            Rect::new(0, 0, self.mode.width, self.mode.height),
        )
        .map_err(|_| ScanoutError::DeviceLost)?;
        Ok(Some(PresentStats {
            sequence,
            rectangles: 1,
            pixels: u64::from(self.mode.width) * u64::from(self.mode.height),
        }))
    }

    /// Синхронизирует imported VirGL render target с его guest backing.
    /// Вызов выполняется только после успешного render fence и поэтому не
    /// является частью submit hot path.
    pub fn download_imported(&mut self, graphics_object: u16) -> Result<(), ModeSetError> {
        let resource = self
            .render_resources
            .iter()
            .copied()
            .find(|resource| resource.used && resource.graphics_object == graphics_object)
            .ok_or(ModeSetError::UnsupportedMode)?;
        if !resource.has_backing {
            return Err(ModeSetError::UnsupportedMode);
        }
        let mut request = Transfer3dRequest {
            header: self.header(CMD_TRANSFER_FROM_HOST_3D),
            box_3d: GpuBox {
                x: 0,
                y: 0,
                z: 0,
                width: resource.width,
                height: resource.height,
                depth: 1,
            },
            offset: 0,
            resource: resource.id,
            level: 0,
            stride: resource.width.saturating_mul(4),
            layer_stride: resource
                .width
                .saturating_mul(resource.height)
                .saturating_mul(4),
        };
        request.header.context_id = resource.context;
        self.command_nodata(&request)
    }

    pub fn destroy_render_context(&mut self, context: u32) {
        if context == 0 || context != self.render_context {
            return;
        }
        let _ = self.ensure_primary_scanout();
        for index in 0..self.render_resources.len() {
            let resource = self.render_resources[index];
            if !resource.used || resource.context != context {
                continue;
            }
            let _ = self.detach_context(context, resource.id);
            if resource.has_backing {
                let _ = self.detach_resource(resource.id);
            }
            let _ = self.unref_resource(resource.id);
            self.render_resources[index] = RenderResource::EMPTY;
        }
        let mut request = self.header(CMD_CTX_DESTROY);
        request.context_id = context;
        let _ = self.command_nodata(&request);
        self.render_context = 0;
    }

    fn discover_capset(&mut self) {
        if !self.transport.virgl_supported() {
            return;
        }
        for index in 0..self.transport.num_capsets().min(64) {
            let request = GetCapsetInfoRequest {
                header: self.header(CMD_GET_CAPSET_INFO),
                index,
                padding: 0,
            };
            let Ok(response) = self.transport.command::<_, CapsetInfoResponse>(&request) else {
                return;
            };
            if response.header.kind == RESP_OK_CAPSET_INFO
                && matches!(response.id, 1 | 2)
                && (self.capset_id == 0 || response.id == 2)
            {
                self.capset_id = response.id;
                self.capset_version = response.max_version;
                self.capset_size = response.max_size;
            }
        }
    }

    fn display_info(&mut self) -> Result<(u32, DisplayMode), ModeSetError> {
        let request = self.header(CMD_GET_DISPLAY_INFO);
        let response: DisplayInfoResponse =
            self.transport.command(&request).map_err(map_transport)?;
        if response.header.kind != RESP_OK_DISPLAY_INFO {
            return Err(ModeSetError::DeviceLost);
        }
        let scanout_count = self.transport.num_scanouts().min(MAX_SCANOUTS as u32) as usize;
        for (index, display) in response.displays[..scanout_count].iter().enumerate() {
            if display.enabled != 0 && display.rect.width != 0 && display.rect.height != 0 {
                return Ok((
                    index as u32,
                    DisplayMode {
                        width: display.rect.width,
                        height: display.rect.height,
                        stride_pixels: display.rect.width,
                        format: CpuPixelFormat::Bgr888,
                        refresh_millihertz: 0,
                    },
                ));
            }
        }
        Err(ModeSetError::DeviceLost)
    }

    fn read_edid(&mut self) -> Option<DisplayMode> {
        if !self.transport.edid_supported() {
            return None;
        }
        let request = GetEdidRequest {
            header: self.header(CMD_GET_EDID),
            scanout: self.scanout,
            padding: 0,
        };
        let Ok(response) = self.transport.command::<_, EdidResponse>(&request) else {
            return None;
        };
        if response.header.kind != RESP_OK_EDID {
            return None;
        }
        let size = (response.size as usize).min(response.bytes.len());
        let info = edid::parse(&response.bytes[..size])?;
        self.edid_valid = true;
        self.width_mm = info.width_mm;
        self.height_mm = info.height_mm;
        let mut preferred = None;
        for mode in info.modes[..info.mode_count].iter().copied() {
            let display_mode = DisplayMode {
                width: mode.width,
                height: mode.height,
                stride_pixels: mode.width,
                format: CpuPixelFormat::Bgr888,
                refresh_millihertz: mode.refresh_millihertz,
            };
            self.add_mode(display_mode);
            if mode.preferred {
                preferred = Some(display_mode);
            }
        }
        preferred
    }

    fn add_mode(&mut self, mode: DisplayMode) {
        if mode.width < MIN_WIDTH
            || mode.height < MIN_HEIGHT
            || mode.width > MAX_WIDTH
            || mode.height > MAX_HEIGHT
        {
            return;
        }
        if let Some(existing) = self.modes[..self.mode_count]
            .iter_mut()
            .find(|existing| existing.width == mode.width && existing.height == mode.height)
        {
            if existing.refresh_millihertz == 0 && mode.refresh_millihertz != 0 {
                existing.refresh_millihertz = mode.refresh_millihertz;
            }
            return;
        }
        if self.mode_count < MAX_MODES {
            self.modes[self.mode_count] = DisplayMode {
                stride_pixels: mode.width,
                format: CpuPixelFormat::Bgr888,
                ..mode
            };
            self.mode_count += 1;
        }
    }

    fn allocate_resource(&mut self, mode: DisplayMode) -> Result<Resource, ModeSetError> {
        let bytes = mode
            .width
            .checked_mul(mode.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ModeSetError::UnsupportedMode)?;
        let backing = memory::allocate(u64::from(bytes).div_ceil(4096), 1)
            .map_err(|_| ModeSetError::OutOfMemory)?;
        unsafe { ptr::write_bytes(backing.phys as *mut u8, 0, (backing.frames * 4096) as usize) };
        let resource = self.next_resource;
        self.next_resource = self.next_resource.wrapping_add(1).max(1);
        let create = Create2dRequest {
            header: self.header(CMD_RESOURCE_CREATE_2D),
            resource,
            format: GPU_FORMAT_B8G8R8X8_UNORM,
            width: mode.width,
            height: mode.height,
        };
        if self.command_nodata(&create).is_err() {
            let _ = memory::free(backing);
            return Err(ModeSetError::DeviceLost);
        }
        let attach = AttachBackingRequest {
            header: self.header(CMD_RESOURCE_ATTACH_BACKING),
            resource,
            entries: 1,
            entry: MemoryEntry {
                address: backing.phys,
                length: bytes,
                padding: 0,
            },
        };
        if self.command_nodata(&attach).is_err() {
            let _ = self.unref_resource(resource);
            let _ = memory::free(backing);
            return Err(ModeSetError::DeviceLost);
        }
        Ok(Resource {
            id: resource,
            backing,
            bytes,
        })
    }

    fn set_scanout_resource(
        &mut self,
        resource: u32,
        mode: DisplayMode,
    ) -> Result<(), ModeSetError> {
        let request = SetScanoutRequest {
            header: self.header(CMD_SET_SCANOUT),
            rect: GpuRect {
                x: 0,
                y: 0,
                width: mode.width,
                height: mode.height,
            },
            scanout: self.scanout,
            resource,
        };
        self.command_nodata(&request)?;
        self.active_scanout_resource = resource;
        Ok(())
    }

    fn ensure_primary_scanout(&mut self) -> Result<(), ModeSetError> {
        if self.active_scanout_resource != self.resource.id {
            self.set_scanout_resource(self.resource.id, self.mode)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn create_3d_resource(
        &mut self,
        context: u32,
        target: u32,
        format: u32,
        bind: u32,
        width: u32,
        height: u32,
        depth: u32,
        array_size: u32,
        flags: u32,
    ) -> Result<u32, ModeSetError> {
        if self.render_resources.iter().all(|resource| resource.used) {
            return Err(ModeSetError::OutOfMemory);
        }
        let resource = self.next_resource;
        self.next_resource = self.next_resource.wrapping_add(1).max(1);
        let request = Create3dRequest {
            header: self.header(CMD_RESOURCE_CREATE_3D),
            resource,
            target,
            format,
            bind,
            width,
            height,
            depth,
            array_size,
            last_level: 0,
            samples: 0,
            flags,
            padding: 0,
        };
        let _ = context;
        self.command_nodata(&request)?;
        Ok(resource)
    }

    fn attach_context(&mut self, context: u32, resource: u32) -> Result<(), ModeSetError> {
        let mut request = ResourceRequest {
            header: self.header(CMD_CTX_ATTACH_RESOURCE),
            resource,
            padding: 0,
        };
        request.header.context_id = context;
        self.command_nodata(&request)
    }

    fn detach_context(&mut self, context: u32, resource: u32) -> Result<(), ModeSetError> {
        let mut request = ResourceRequest {
            header: self.header(CMD_CTX_DETACH_RESOURCE),
            resource,
            padding: 0,
        };
        request.header.context_id = context;
        self.command_nodata(&request)
    }

    fn detach_resource(&mut self, resource: u32) -> Result<(), ModeSetError> {
        let request = ResourceRequest {
            header: self.header(CMD_RESOURCE_DETACH_BACKING),
            resource,
            padding: 0,
        };
        self.command_nodata(&request)
    }

    fn unref_resource(&mut self, resource: u32) -> Result<(), ModeSetError> {
        let request = ResourceRequest {
            header: self.header(CMD_RESOURCE_UNREF),
            resource,
            padding: 0,
        };
        self.command_nodata(&request)
    }

    fn transfer(&mut self, rect: Rect) -> Result<(), ModeSetError> {
        let offset =
            (u64::from(rect.y as u32) * u64::from(self.mode.width) + u64::from(rect.x as u32)) * 4;
        if offset >= u64::from(self.resource.bytes) {
            return Err(ModeSetError::DeviceLost);
        }
        let request = TransferRequest {
            header: self.header(CMD_TRANSFER_TO_HOST_2D),
            rect: GpuRect {
                x: rect.x as u32,
                y: rect.y as u32,
                width: rect.width,
                height: rect.height,
            },
            offset,
            resource: self.resource.id,
            padding: 0,
        };
        self.command_nodata(&request)
    }

    fn flush(&mut self, rect: Rect) -> Result<(), ModeSetError> {
        self.flush_resource(self.resource.id, rect)
    }

    fn flush_resource(&mut self, resource: u32, rect: Rect) -> Result<(), ModeSetError> {
        let request = FlushRequest {
            header: self.header(CMD_RESOURCE_FLUSH),
            rect: GpuRect {
                x: rect.x as u32,
                y: rect.y as u32,
                width: rect.width,
                height: rect.height,
            },
            resource,
            padding: 0,
        };
        self.command_nodata(&request)
    }

    fn command_nodata<Request: Copy>(&mut self, request: &Request) -> Result<(), ModeSetError> {
        let response: ControlHeader = self.transport.command(request).map_err(map_transport)?;
        (response.kind == RESP_OK_NODATA)
            .then_some(())
            .ok_or(ModeSetError::DeviceLost)
    }

    fn header(&mut self, kind: u32) -> ControlHeader {
        self.fence = self.fence.wrapping_add(1);
        ControlHeader {
            kind,
            flags: FLAG_FENCE,
            fence_id: self.fence,
            ..ControlHeader::ZERO
        }
    }
}

/// Проверяет framing VirGL stream до передачи host renderer'у. Содержимое
/// команд остаётся задачей Mesa/renderd, но malformed length и неизвестные
/// opcodes не могут заставить decoder выйти за command buffer.
fn valid_virgl_stream(commands: &[u8]) -> bool {
    if commands.is_empty()
        || commands.len() > GPU_MAX_COMMAND_BYTES as usize
        || !commands.len().is_multiple_of(4)
    {
        return false;
    }
    let mut offset = 0usize;
    while offset < commands.len() {
        let header = u32::from_le_bytes([
            commands[offset],
            commands[offset + 1],
            commands[offset + 2],
            commands[offset + 3],
        ]);
        let opcode = header as u8;
        let payload = (header >> 16) as usize;
        if payload == 0
            || !matches!(opcode, 1 | 2 | 4 | 5 | 6 | 7 | 8 | 9 | 31 | 52)
            || offset
                .checked_add((payload + 1) * 4)
                .is_none_or(|end| end > commands.len())
        {
            return false;
        }
        offset += (payload + 1) * 4;
    }
    offset == commands.len()
}

impl Drop for VirtioGpu {
    fn drop(&mut self) {
        if self.resource.id == 0 {
            return;
        }
        let disable = SetScanoutRequest {
            header: self.header(CMD_SET_SCANOUT),
            rect: GpuRect::default(),
            scanout: self.scanout,
            resource: 0,
        };
        let _ = self.command_nodata(&disable);
        if self.detach_resource(self.resource.id).is_ok() {
            let _ = self.unref_resource(self.resource.id);
            let _ = memory::free(self.resource.backing);
        }
        self.resource.id = 0;
    }
}

fn map_transport(error: TransportError) -> ModeSetError {
    match error {
        TransportError::OutOfMemory => ModeSetError::OutOfMemory,
        TransportError::Unsupported | TransportError::InvalidConfiguration => {
            ModeSetError::UnsupportedMode
        }
        TransportError::RejectedFeatures
        | TransportError::Timeout
        | TransportError::Busy
        | TransportError::DeviceError => ModeSetError::DeviceLost,
    }
}

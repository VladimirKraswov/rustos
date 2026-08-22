//! Virtio GPU display/render driver (Virtio 1.x PCI и MMIO transport).
//!
//! 2D command set остаётся надёжным fallback. Если device предлагает VirGL,
//! отдельный ring-3 `renderd` получает контекст, импортирует GraphicsBuffer и
//! отправляет fenced 3D command stream через асинхронную control queue.

use core::ptr;

use rustos_abi::{
    gpu::{
        feature, GpuDeviceInfo, GpuResourceCreate, GpuResourceImport, GPU_ABI_VERSION,
        GPU_MAX_COMMAND_BYTES,
    },
    graphics_buffer::{GraphicsBufferDesc, PixelFormatCode},
};
use rustos_video::{
    select_startup_mode, ConnectorInfo, ConnectorKind, CpuPixelFormat, CpuSurface, DisplayMode,
    ModeSetError, PresentStats, Rect, ScanoutCapabilities, ScanoutError, StartupModePolicy,
};

use crate::{
    arch,
    memory::{self, FrameBlock},
    serial,
};

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
#[cfg(target_arch = "aarch64")]
const GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
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
#[cfg(target_arch = "aarch64")]
const CMD_UPDATE_CURSOR: u32 = 0x0300;
#[cfg(target_arch = "aarch64")]
const CMD_MOVE_CURSOR: u32 = 0x0301;
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_EDID: u32 = 0x1104;
const RESP_OK_CAPSET_INFO: u32 = 0x1102;
const FLAG_FENCE: u32 = 1;
const RESOURCE_FLAG_Y_0_TOP: u32 = 1;
// Три swapchain targets, vertex/atlas resources и независимые поверхности
// окон должны сосуществовать в одном compositor context. Таблица хранит
// только metadata; фактическую память по-прежнему ограничивает allocator.
const MAX_RENDER_RESOURCES: usize = 64;
const PRIMARY_BUFFER_COUNT: usize = 3;
const MAX_PENDING_PRESENT_COMMANDS: usize = PRIMARY_BUFFER_COUNT * 3;
const MAX_READY_RENDER_COMPLETIONS: usize = 8;
#[cfg(target_arch = "aarch64")]
const CURSOR_EXTENT: u32 = 64;
#[cfg(target_arch = "aarch64")]
const CURSOR_BYTES: u32 = CURSOR_EXTENT * CURSOR_EXTENT * 4;
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
#[cfg(target_arch = "aarch64")]
struct CursorPosition {
    scanout: u32,
    x: u32,
    y: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[cfg(target_arch = "aarch64")]
struct UpdateCursorRequest {
    header: ControlHeader,
    position: CursorPosition,
    resource: u32,
    hotspot_x: u32,
    hotspot_y: u32,
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
}

#[derive(Clone, Copy)]
struct PendingPresentCommand {
    fence: u64,
    slot: u8,
    final_command: bool,
}

impl PendingPresentCommand {
    const EMPTY: Self = Self {
        fence: 0,
        slot: 0,
        final_command: false,
    };
}

#[derive(Clone, Copy)]
struct RenderResource {
    used: bool,
    id: u32,
    context: u32,
    graphics_object: u16,
    width: u32,
    height: u32,
    format: u32,
    bind: u32,
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
        format: 0,
        bind: 0,
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
    primary: [Resource; PRIMARY_BUFFER_COUNT],
    primary_busy: [bool; PRIMARY_BUFFER_COUNT],
    primary_damage: [Rect; PRIMARY_BUFFER_COUNT],
    primary_next: usize,
    pending_present: bool,
    source_pixels: *const u32,
    source_format: CpuPixelFormat,
    pending_present_commands: [PendingPresentCommand; MAX_PENDING_PRESENT_COMMANDS],
    ready_render: [Option<RenderCompletion>; MAX_READY_RENDER_COMPLETIONS],
    ready_render_read: usize,
    ready_render_write: usize,
    async_device_failed: bool,
    async_present_logged: bool,
    #[cfg(target_arch = "aarch64")]
    cursor_resource: Resource,
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
    #[cfg(target_arch = "aarch64")]
    pending_cursor: Option<UpdateCursorRequest>,
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
        };
        let mut gpu = Self {
            transport,
            scanout: 0,
            primary: [placeholder; PRIMARY_BUFFER_COUNT],
            primary_busy: [false; PRIMARY_BUFFER_COUNT],
            primary_damage: [Rect::EMPTY; PRIMARY_BUFFER_COUNT],
            primary_next: 0,
            pending_present: false,
            source_pixels: ptr::null(),
            source_format: CpuPixelFormat::Bgr888,
            pending_present_commands: [PendingPresentCommand::EMPTY; MAX_PENDING_PRESENT_COMMANDS],
            ready_render: [None; MAX_READY_RENDER_COMPLETIONS],
            ready_render_read: 0,
            ready_render_write: 0,
            async_device_failed: false,
            async_present_logged: false,
            #[cfg(target_arch = "aarch64")]
            cursor_resource: placeholder,
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
            #[cfg(target_arch = "aarch64")]
            pending_cursor: None,
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
        let primary = gpu.allocate_primary_resources(startup)?;
        if let Err(error) = gpu.set_scanout_resource(primary[0].id, startup) {
            gpu.release_primary_resources(primary);
            return Err(error);
        }
        gpu.primary = primary;
        gpu.mode = startup;
        let full = Rect::new(0, 0, startup.width, startup.height);
        gpu.primary_damage = [full; PRIMARY_BUFFER_COUNT];
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
            page_flip: true,
            vsync_event: false,
            hardware_cursor: self.transport.cursor_supported(),
            multiple_outputs: false,
        }
    }

    /// Загружает ARGB sprite в отдельный 64×64 cursor resource и атомарно
    /// связывает его с hardware cursor plane. Горячий mouse path после этого
    /// использует только `move_cursor` и никогда не трогает scanout damage.
    #[allow(clippy::too_many_arguments)]
    pub fn update_cursor(
        &mut self,
        pixels: &[u32],
        width: u32,
        height: u32,
        hotspot_x: u32,
        hotspot_y: u32,
        pointer_x: i32,
        pointer_y: i32,
    ) -> Result<(), ModeSetError> {
        #[cfg(target_arch = "aarch64")]
        {
            if !self.transport.cursor_supported()
                || width == 0
                || height == 0
                || width > CURSOR_EXTENT
                || height > CURSOR_EXTENT
                || hotspot_x >= width
                || hotspot_y >= height
                || pixels.len()
                    != usize::try_from(u64::from(width) * u64::from(height)).unwrap_or(usize::MAX)
            {
                return Err(ModeSetError::UnsupportedMode);
            }
            self.ensure_cursor_resource()?;
            let destination = self.cursor_resource.backing.phys as *mut u32;
            // SAFETY: cursor backing содержит ровно 64*64 u32; input length и
            // обе размерности проверены выше, строки не пересекаются.
            unsafe { ptr::write_bytes(destination, 0, (CURSOR_BYTES / 4) as usize) };
            for row in 0..height as usize {
                unsafe {
                    ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(row * width as usize),
                        destination.add(row * CURSOR_EXTENT as usize),
                        width as usize,
                    )
                };
            }
            arch::dma_write_barrier();
            self.transfer_resource_2d(
                self.cursor_resource.id,
                Rect::new(0, 0, CURSOR_EXTENT, CURSOR_EXTENT),
                0,
            )?;
            let request = self.cursor_request(
                CMD_UPDATE_CURSOR,
                self.cursor_resource.id,
                hotspot_x,
                hotspot_y,
                pointer_x,
                pointer_y,
            );
            self.queue_cursor(request)
        }
        #[cfg(target_arch = "x86_64")]
        {
            let _ = (
                pixels, width, height, hotspot_x, hotspot_y, pointer_x, pointer_y,
            );
            Err(ModeSetError::UnsupportedMode)
        }
    }

    /// Неблокирующее перемещение hardware plane. При заполненной cursorq
    /// сохраняется только последняя позиция (mailbox), старые кадры не ждём.
    pub fn move_cursor(&mut self, pointer_x: i32, pointer_y: i32) -> Result<(), ModeSetError> {
        #[cfg(target_arch = "aarch64")]
        {
            if self.cursor_resource.id == 0 {
                return Err(ModeSetError::UnsupportedMode);
            }
            let request = self.cursor_request(
                CMD_MOVE_CURSOR,
                self.cursor_resource.id,
                0,
                0,
                pointer_x,
                pointer_y,
            );
            self.queue_cursor(request)
        }
        #[cfg(target_arch = "x86_64")]
        {
            let _ = (pointer_x, pointer_y);
            Err(ModeSetError::UnsupportedMode)
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
        self.drain_primary_presents()?;
        let replacement = self.allocate_primary_resources(requested)?;
        if let Err(error) = self.set_scanout_resource(replacement[0].id, requested) {
            self.release_primary_resources(replacement);
            return Err(error);
        }

        let previous = self.primary;
        self.primary = replacement;
        self.primary_busy = [false; PRIMARY_BUFFER_COUNT];
        let full = Rect::new(0, 0, requested.width, requested.height);
        self.primary_damage = [full; PRIMARY_BUFFER_COUNT];
        self.primary_next = 0;
        self.pending_present = false;
        self.source_pixels = ptr::null();
        self.mode = requested;
        // SET_SCANOUT уже отвязал старый resource. Освобождаем DMA backing
        // только после подтверждённого DETACH; при ошибке безопаснее оставить
        // bounded leak, чем дать device доступ к повторно выданным кадрам.
        self.release_primary_resources(previous);
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
        let source_pixels = source
            .contiguous_pixels()
            .ok_or(ScanoutError::InvalidSurface)?;
        self.source_pixels = source_pixels.as_ptr();
        self.source_format = source.format();
        let bounds = Rect::new(0, 0, self.mode.width, self.mode.height);
        let mut rectangles = 0u32;
        let mut pixels = 0u64;
        let mut commit = Rect::EMPTY;
        for requested in damage.iter().copied() {
            let rect = requested.intersection(bounds);
            if rect.is_empty() {
                continue;
            }
            commit = if commit.is_empty() {
                rect
            } else {
                commit.union(rect)
            };
            rectangles = rectangles.saturating_add(1);
            pixels = pixels.saturating_add(rect.area());
        }
        if !commit.is_empty() {
            for pending in &mut self.primary_damage {
                *pending = if pending.is_empty() {
                    commit
                } else {
                    pending.union(commit)
                };
            }
            self.pending_present = true;
            self.drain_async_completions()
                .map_err(|_| ScanoutError::DeviceLost)?;
            self.queue_latest_primary()
                .map_err(|_| ScanoutError::DeviceLost)?;
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
        // Нельзя писать primary[0], пока устройство ещё читает его в одном
        // из асинхронных GUI present. Этот редкий ring-3 displayd путь может
        // ждать, но никогда не пересекает DMA с CPU copy.
        self.drain_primary_presents()
            .and_then(|_| self.ensure_primary_scanout())
            .map_err(|_| ScanoutError::DeviceLost)?;
        let plane = descriptor.planes[0];
        let row_bytes = usize::try_from(descriptor.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(ScanoutError::InvalidSurface)?;
        let stride = plane.stride_bytes as usize;
        let plane_offset =
            usize::try_from(plane.offset).map_err(|_| ScanoutError::InvalidSurface)?;
        let target = self.primary[0].backing.phys as *mut u8;
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
        self.transfer_resource_2d(self.primary[0].id, full, 0)
            .and_then(|_| self.flush_resource(self.primary[0].id, full))
            .map_err(|_| ScanoutError::DeviceLost)?;
        // Следующий kernel GUI commit может оказаться частичным. Остальные
        // два resources содержат более старый desktop, поэтому перед их
        // повторным scanout они обязаны получить целиком актуальный backbuffer.
        self.primary_damage = [full; PRIMARY_BUFFER_COUNT];
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
            // Независимые swapchain buffers используют отдельные timelines;
            // один и тот же timeline нельзя переиспользовать до completion.
            max_inflight: 3,
            max_contexts: 1,
            capset_id: self.capset_id,
            capset_version: self.capset_version,
            capset_size: self.capset_size,
            reserved: [0; 3],
        })
    }

    /// Возвращает platform interrupt virtio-gpu и предварительно снимает
    /// возможный старый latch. GIC включается только после полного DRIVER_OK,
    /// поэтому IRQ не может прийти в обработчик с полуготовым transport.
    pub fn prepare_interrupt(&mut self) -> Result<Option<u32>, ModeSetError> {
        #[cfg(target_arch = "aarch64")]
        {
            let _ = self
                .transport
                .acknowledge_interrupt()
                .map_err(map_transport)?;
            Ok(Some(self.transport.interrupt_id()))
        }
        #[cfg(target_arch = "x86_64")]
        {
            // PCI MSI-X/IOAPIC adapter ещё не подключён. x86-64 продолжает
            // bounded polling из timer bottom half без ложной IRQ-рекламы.
            Ok(None)
        }
    }

    /// Снимает interrupt status только у нашего transport. Used ring затем
    /// разбирается обычным `poll_render`: IRQ остаётся коротким top half и
    /// никогда не копирует следующий 2D frame.
    pub fn handle_interrupt(&mut self, interrupt: u32) -> Result<bool, ModeSetError> {
        #[cfg(target_arch = "aarch64")]
        {
            if interrupt != self.transport.interrupt_id() {
                return Ok(false);
            }
            let acknowledged = self
                .transport
                .acknowledge_interrupt()
                .map_err(map_transport)?;
            if let Some(pending) = self.pending_cursor.take() {
                match self.transport.submit_cursor(&pending) {
                    Ok(()) => {}
                    Err(TransportError::Busy) => self.pending_cursor = Some(pending),
                    Err(error) => return Err(map_transport(error)),
                }
            }
            Ok(acknowledged)
        }
        #[cfg(target_arch = "x86_64")]
        {
            let _ = interrupt;
            Ok(false)
        }
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

    pub fn import_render_resource(
        &mut self,
        context: u32,
        graphics_object: u16,
        descriptor: GraphicsBufferDesc,
        backing: FrameBlock,
        request: GpuResourceImport,
    ) -> Result<u32, ModeSetError> {
        let format = match descriptor.format {
            PixelFormatCode::B8G8R8A8_UNORM => 1,
            PixelFormatCode::B8G8R8X8_UNORM => GPU_FORMAT_B8G8R8X8_UNORM,
            PixelFormatCode::R8_UNORM => 64,
            _ => return Err(ModeSetError::UnsupportedMode),
        };
        if context != self.render_context
            || context == 0
            || request.validate().is_err()
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
            request.target,
            format,
            request.bind,
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
            format,
            bind: request.bind,
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
            format: request.format,
            bind: request.bind,
            has_backing: false,
        };
        Ok(resource)
    }

    /// Уничтожает только device-local resource текущего render context.
    /// Capability-backed imports имеют отдельный жизненный цикл и здесь
    /// намеренно отвергаются, чтобы renderer не мог оборвать чужой DMA.
    pub fn destroy_render_resource(
        &mut self,
        context: u32,
        resource: u32,
    ) -> Result<(), ModeSetError> {
        if context != self.render_context || resource == 0 {
            return Err(ModeSetError::UnsupportedMode);
        }
        let index = self
            .render_resources
            .iter()
            .position(|candidate| {
                candidate.used
                    && candidate.context == context
                    && candidate.id == resource
                    && !candidate.has_backing
            })
            .ok_or(ModeSetError::UnsupportedMode)?;
        self.detach_context(context, resource)?;
        self.unref_resource(resource)?;
        self.render_resources[index] = RenderResource::EMPTY;
        Ok(())
    }

    pub fn submit_render(&mut self, context: u32, commands: &[u8]) -> Result<u64, ModeSetError> {
        if context != self.render_context
            || !valid_virgl_stream(commands, context, &self.render_resources)
        {
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
        self.drain_async_completions()?;
        let completion = self.ready_render[self.ready_render_read].take();
        if completion.is_some() {
            self.ready_render_read = (self.ready_render_read + 1) % MAX_READY_RENDER_COMPLETIONS;
        }
        Ok(completion)
    }

    /// Обслуживает отложенный mailbox present вне IRQ/timer context.
    /// GUI вызывает этот метод только после проверки очереди ввода: даже
    /// полноэкранная копия в свободный DMA resource не задерживает мышь.
    pub fn service_present(&mut self) -> Result<(), ModeSetError> {
        self.drain_async_completions()?;
        self.queue_latest_primary()
    }

    /// Разбирает общую controlq: present fences остаются внутри display
    /// driver, render fences публикуются process manager'у. Благодаря этому
    /// IRQ освобождает 2D buffers даже когда ring-3 GPU submission отсутствует.
    fn drain_async_completions(&mut self) -> Result<(), ModeSetError> {
        while let Some(completion) = self.transport.poll_completion().map_err(map_transport)? {
            if let Some(index) = self
                .pending_present_commands
                .iter()
                .position(|pending| pending.fence == completion.fence_id)
            {
                let pending = self.pending_present_commands[index];
                self.pending_present_commands[index] = PendingPresentCommand::EMPTY;
                if completion.response_kind != RESP_OK_NODATA {
                    self.async_device_failed = true;
                }
                if pending.final_command {
                    self.primary_busy[usize::from(pending.slot)] = false;
                }
                continue;
            }
            if self.ready_render[self.ready_render_write].is_some() {
                self.async_device_failed = true;
                return Err(ModeSetError::DeviceLost);
            }
            self.ready_render[self.ready_render_write] = Some(RenderCompletion {
                fence_id: completion.fence_id,
                succeeded: completion.response_kind == RESP_OK_NODATA,
            });
            self.ready_render_write = (self.ready_render_write + 1) % MAX_READY_RENDER_COMPLETIONS;
        }
        if self.async_device_failed {
            return Err(ModeSetError::DeviceLost);
        }
        Ok(())
    }

    fn drain_primary_presents(&mut self) -> Result<(), ModeSetError> {
        for _ in 0..50_000_000 {
            self.drain_async_completions()?;
            self.queue_latest_primary()?;
            if !self.pending_present
                && self.primary_busy.iter().all(|busy| !*busy)
                && self
                    .pending_present_commands
                    .iter()
                    .all(|pending| pending.fence == 0)
            {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(ModeSetError::DeviceLost)
    }

    /// Дожидается следующего completion только при аварийном teardown процесса.
    /// Обычный render path всегда неблокирующий и завершается из timer bottom
    /// half. Здесь ожидание необходимо, чтобы QEMU/реальный GPU не продолжал
    /// DMA после освобождения capability-backed кадров.
    pub fn drain_next_render(&mut self) -> Result<RenderCompletion, ModeSetError> {
        for _ in 0..50_000_000 {
            if let Some(completion) = self.poll_render()? {
                return Ok(completion);
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
        self.drain_primary_presents()
            .map_err(|_| ScanoutError::DeviceLost)?;
        if self.active_scanout_resource != resource.id {
            self.set_scanout_resource(resource.id, self.mode)
                .map_err(|_| ScanoutError::DeviceLost)?;
        }
        self.flush_resource(
            resource.id,
            Rect::new(0, 0, self.mode.width, self.mode.height),
        )
        .map_err(|_| ScanoutError::DeviceLost)?;
        // Возврат к CPU desktop должен начинаться с полного содержимого, а
        // не с небольшого damage поверх последнего старого primary resource.
        let full = Rect::new(0, 0, self.mode.width, self.mode.height);
        self.primary_damage = [full; PRIMARY_BUFFER_COUNT];
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
        })
    }

    fn allocate_primary_resources(
        &mut self,
        mode: DisplayMode,
    ) -> Result<[Resource; PRIMARY_BUFFER_COUNT], ModeSetError> {
        let empty = Resource {
            id: 0,
            backing: FrameBlock { phys: 0, frames: 0 },
        };
        let mut resources = [empty; PRIMARY_BUFFER_COUNT];
        for index in 0..PRIMARY_BUFFER_COUNT {
            match self.allocate_resource(mode) {
                Ok(resource) => resources[index] = resource,
                Err(error) => {
                    self.release_primary_resources(resources);
                    return Err(error);
                }
            }
        }
        Ok(resources)
    }

    fn release_primary_resources(&mut self, resources: [Resource; PRIMARY_BUFFER_COUNT]) {
        for resource in resources {
            if resource.id == 0 {
                continue;
            }
            if self.detach_resource(resource.id).is_ok() {
                let _ = self.unref_resource(resource.id);
                let _ = memory::free(resource.backing);
            }
        }
    }

    fn queue_latest_primary(&mut self) -> Result<(), ModeSetError> {
        if self.async_device_failed {
            return Err(ModeSetError::DeviceLost);
        }
        if !self.pending_present {
            return Ok(());
        }
        let Some(slot) = (0..PRIMARY_BUFFER_COUNT)
            .map(|offset| (self.primary_next + offset) % PRIMARY_BUFFER_COUNT)
            .find(|slot| !self.primary_busy[*slot])
        else {
            // Все три кадра заняты: mailbox уже содержит newest sequence и
            // будет отправлен из completion bottom half.
            return Ok(());
        };
        let damage = self.primary_damage[slot];
        if damage.is_empty() || self.source_pixels.is_null() {
            self.pending_present = false;
            return Ok(());
        }
        self.copy_primary_damage(slot, damage)?;
        let resource = self.primary[slot].id;
        let offset = (u64::from(damage.y as u32) * u64::from(self.mode.width)
            + u64::from(damage.x as u32))
            * 4;
        let transfer = TransferRequest {
            header: self.header(CMD_TRANSFER_TO_HOST_2D),
            rect: GpuRect {
                x: damage.x as u32,
                y: damage.y as u32,
                width: damage.width,
                height: damage.height,
            },
            offset,
            resource,
            padding: 0,
        };
        self.submit_present_command(&transfer, transfer.header.fence_id, slot, false)?;
        let scanout = SetScanoutRequest {
            header: self.header(CMD_SET_SCANOUT),
            rect: GpuRect {
                x: 0,
                y: 0,
                width: self.mode.width,
                height: self.mode.height,
            },
            scanout: self.scanout,
            resource,
        };
        self.submit_present_command(&scanout, scanout.header.fence_id, slot, false)?;
        let flush = FlushRequest {
            header: self.header(CMD_RESOURCE_FLUSH),
            rect: GpuRect {
                x: damage.x as u32,
                y: damage.y as u32,
                width: damage.width,
                height: damage.height,
            },
            resource,
            padding: 0,
        };
        self.submit_present_command(&flush, flush.header.fence_id, slot, true)?;
        self.primary_busy[slot] = true;
        self.primary_damage[slot] = Rect::EMPTY;
        self.primary_next = (slot + 1) % PRIMARY_BUFFER_COUNT;
        self.active_scanout_resource = resource;
        self.pending_present = false;
        if !self.async_present_logged {
            serial::put_str(
                "[video] 2d-present=async buffers=3 damage=coalesced mailbox=latest completion=deferred\n",
            );
            self.async_present_logged = true;
        }
        Ok(())
    }

    fn copy_primary_damage(&mut self, slot: usize, damage: Rect) -> Result<(), ModeSetError> {
        let target = self.primary[slot].backing.phys as *mut u32;
        for y in damage.y as u32..damage.bottom() as u32 {
            let offset = y as usize * self.mode.width as usize + damage.x as usize;
            let source = unsafe { self.source_pixels.add(offset) };
            let destination = unsafe { target.add(offset) };
            if self.source_format == CpuPixelFormat::Bgr888 {
                unsafe { ptr::copy_nonoverlapping(source, destination, damage.width as usize) };
            } else {
                for column in 0..damage.width as usize {
                    let raw = unsafe { source.add(column).read() };
                    let converted = CpuPixelFormat::Bgr888.pack(self.source_format.unpack(raw));
                    unsafe { destination.add(column).write(converted) };
                }
            }
        }
        arch::dma_write_barrier();
        Ok(())
    }

    fn submit_present_command<Request: Copy>(
        &mut self,
        request: &Request,
        fence: u64,
        slot: usize,
        final_command: bool,
    ) -> Result<(), ModeSetError> {
        let Some(record_index) = self
            .pending_present_commands
            .iter()
            .position(|record| record.fence == 0)
        else {
            self.async_device_failed = true;
            return Err(ModeSetError::DeviceLost);
        };
        // Регистрируем fence до публикации descriptor. Быстрое устройство
        // вправе завершить команду сразу после queue notify; IRQ тогда уже
        // найдёт владельца completion и не примет его за render fence.
        self.pending_present_commands[record_index] = PendingPresentCommand {
            fence,
            slot: slot as u8,
            final_command,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (request as *const Request).cast::<u8>(),
                core::mem::size_of::<Request>(),
            )
        };
        if self
            .transport
            .submit_bytes(bytes, &[], core::mem::size_of::<ControlHeader>())
            .is_err()
        {
            self.pending_present_commands[record_index] = PendingPresentCommand::EMPTY;
            self.async_device_failed = true;
            return Err(ModeSetError::DeviceLost);
        }
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn ensure_cursor_resource(&mut self) -> Result<(), ModeSetError> {
        if self.cursor_resource.id != 0 {
            return Ok(());
        }
        let backing = memory::allocate(u64::from(CURSOR_BYTES).div_ceil(4096), 1)
            .map_err(|_| ModeSetError::OutOfMemory)?;
        unsafe { ptr::write_bytes(backing.phys as *mut u8, 0, (backing.frames * 4096) as usize) };
        let resource = self.next_resource;
        self.next_resource = self.next_resource.wrapping_add(1).max(1);
        let create = Create2dRequest {
            header: self.header(CMD_RESOURCE_CREATE_2D),
            resource,
            format: GPU_FORMAT_B8G8R8A8_UNORM,
            width: CURSOR_EXTENT,
            height: CURSOR_EXTENT,
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
                length: CURSOR_BYTES,
                padding: 0,
            },
        };
        if self.command_nodata(&attach).is_err() {
            let _ = self.unref_resource(resource);
            let _ = memory::free(backing);
            return Err(ModeSetError::DeviceLost);
        }
        self.cursor_resource = Resource {
            id: resource,
            backing,
        };
        Ok(())
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
        if self.active_scanout_resource != self.primary[0].id {
            self.set_scanout_resource(self.primary[0].id, self.mode)?;
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

    fn transfer_resource_2d(
        &mut self,
        resource: u32,
        rect: Rect,
        offset: u64,
    ) -> Result<(), ModeSetError> {
        let request = TransferRequest {
            header: self.header(CMD_TRANSFER_TO_HOST_2D),
            rect: GpuRect {
                x: rect.x.max(0) as u32,
                y: rect.y.max(0) as u32,
                width: rect.width,
                height: rect.height,
            },
            offset,
            resource,
            padding: 0,
        };
        self.command_nodata(&request)
    }

    #[cfg(target_arch = "aarch64")]
    fn cursor_request(
        &self,
        kind: u32,
        resource: u32,
        hotspot_x: u32,
        hotspot_y: u32,
        pointer_x: i32,
        pointer_y: i32,
    ) -> UpdateCursorRequest {
        UpdateCursorRequest {
            // Cursorq не возвращает protocol response, поэтому fenced header
            // здесь запрещён: completion выражается только used-ring entry.
            header: ControlHeader {
                kind,
                ..ControlHeader::ZERO
            },
            position: CursorPosition {
                scanout: self.scanout,
                x: pointer_x.max(0) as u32,
                y: pointer_y.max(0) as u32,
                padding: 0,
            },
            resource,
            hotspot_x,
            hotspot_y,
            padding: 0,
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn queue_cursor(&mut self, request: UpdateCursorRequest) -> Result<(), ModeSetError> {
        match self.transport.submit_cursor(&request) {
            Ok(()) => Ok(()),
            Err(TransportError::Busy) => {
                // Настоящий mailbox: сохраняется лишь новейшая позиция/форма.
                self.pending_cursor = Some(request);
                Ok(())
            }
            Err(error) => Err(map_transport(error)),
        }
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
        if self.fence == 0 {
            // Нулевой fence зарезервирован как EMPTY sentinel во внутренних
            // bounded tables. Даже после u64 wrap он не публикуется device.
            self.fence = 1;
        }
        ControlHeader {
            kind,
            flags: FLAG_FENCE,
            fence_id: self.fence,
            ..ControlHeader::ZERO
        }
    }
}

/// Проверяет framing и все resource references до передачи host renderer'у.
///
/// `renderd` остаётся недоверенным ring-3 процессом: даже имея GpuRender
/// capability, он может ссылаться только на resources своего context. Полный
/// semantic validator upstream Mesa IR здесь не нужен, но command length,
/// object kind, fixed payload и DMA bounds обязаны быть проверены kernel.
fn valid_virgl_stream(commands: &[u8], context: u32, resources: &[RenderResource]) -> bool {
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
        let object = (header >> 8) as u8;
        let payload = (header >> 16) as usize;
        if payload == 0
            || offset
                .checked_add((payload + 1) * 4)
                .is_none_or(|end| end > commands.len())
            || !valid_virgl_command(
                commands, offset, opcode, object, payload, context, resources,
            )
        {
            return false;
        }
        offset += (payload + 1) * 4;
    }
    offset == commands.len()
}

fn valid_virgl_command(
    commands: &[u8],
    offset: usize,
    opcode: u8,
    object: u8,
    payload: usize,
    context: u32,
    resources: &[RenderResource],
) -> bool {
    match opcode {
        // CREATE_OBJECT. Только используемые renderer'ом типы и их точный
        // wire-size. Shader содержит variable-length TGSI text после пяти
        // обязательных dwords.
        1 => {
            let valid_size = match object {
                1 => payload == 11, // blend
                2 => payload == 9,  // rasterizer
                3 => payload == 5,  // depth/stencil/alpha
                4 => payload >= 5,  // shader + TGSI text
                5 => payload == 9,  // vertex elements
                6 => payload == 6,  // sampler view
                7 => payload == 9,  // sampler state
                8 => payload == 5,  // render surface
                _ => false,
            };
            if !valid_size {
                return false;
            }
            if matches!(object, 6 | 8) {
                let Some(resource) = command_word(commands, offset, 1)
                    .and_then(|id| render_resource(resources, context, id))
                else {
                    return false;
                };
                let Some(format) = command_word(commands, offset, 2) else {
                    return false;
                };
                resource.format == format
                    && if object == 6 {
                        resource.bind & (1 << 3) != 0
                    } else {
                        resource.bind & (1 << 1) != 0
                    }
            } else {
                true
            }
        }
        2 => payload == 1 && matches!(object, 1 | 2 | 3 | 5),
        // DESTROY_OBJECT. Renderer может освобождать только surface object;
        // immutable pipeline objects живут до уничтожения context.
        3 => object == 8 && payload == 1,
        4 => object == 0 && payload == 7,
        5 => object == 0 && payload == 3,
        6 => {
            object == 0
                && payload == 3
                && command_word(commands, offset, 2)
                    .and_then(|id| render_resource(resources, context, id))
                    .is_some_and(|resource| resource.bind & (1 << 4) != 0)
        }
        7 => object == 0 && payload == 8,
        8 => object == 0 && payload == 12,
        9 => valid_inline_write(commands, offset, object, payload, context, resources),
        10 => object == 0 && payload >= 3,
        14 => object == 0 && payload >= 3 && (payload - 1).is_multiple_of(2),
        16 => valid_blit(commands, offset, object, payload, context, resources),
        18 => object == 0 && payload >= 3,
        31 => object == 0 && payload == 2,
        52 => object == 0 && payload == 6,
        _ => false,
    }
}

fn valid_inline_write(
    commands: &[u8],
    offset: usize,
    object: u8,
    payload: usize,
    context: u32,
    resources: &[RenderResource],
) -> bool {
    if object != 0 || payload < 12 {
        return false;
    }
    let Some(resource) =
        command_word(commands, offset, 0).and_then(|id| render_resource(resources, context, id))
    else {
        return false;
    };
    let Some(x) = command_word(commands, offset, 5) else {
        return false;
    };
    let Some(y) = command_word(commands, offset, 6) else {
        return false;
    };
    let Some(width) = command_word(commands, offset, 8) else {
        return false;
    };
    let Some(height) = command_word(commands, offset, 9) else {
        return false;
    };
    let Some(depth) = command_word(commands, offset, 10) else {
        return false;
    };
    let bytes_per_pixel = match resource.format {
        1 | 2 => 4usize,
        64 => 1,
        _ => return false,
    };
    let Some(expected_bytes) = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(height as usize))
        .and_then(|pixels| pixels.checked_mul(depth as usize))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
    else {
        return false;
    };
    x.checked_add(width)
        .is_some_and(|right| right <= resource.width)
        && y.checked_add(height)
            .is_some_and(|bottom| bottom <= resource.height)
        && depth == 1
        && (payload - 11) * 4 == expected_bytes.next_multiple_of(4)
}

fn valid_blit(
    commands: &[u8],
    offset: usize,
    object: u8,
    payload: usize,
    context: u32,
    resources: &[RenderResource],
) -> bool {
    if object != 0 || payload != 21 {
        return false;
    }
    let Some(destination) =
        command_word(commands, offset, 3).and_then(|id| render_resource(resources, context, id))
    else {
        return false;
    };
    let Some(source) =
        command_word(commands, offset, 12).and_then(|id| render_resource(resources, context, id))
    else {
        return false;
    };
    let Some(destination_format) = command_word(commands, offset, 5) else {
        return false;
    };
    let Some(source_format) = command_word(commands, offset, 14) else {
        return false;
    };
    let Some(destination_x) = command_word(commands, offset, 6) else {
        return false;
    };
    let Some(destination_y) = command_word(commands, offset, 7) else {
        return false;
    };
    let Some(destination_width) = command_word(commands, offset, 9) else {
        return false;
    };
    let Some(destination_height) = command_word(commands, offset, 10) else {
        return false;
    };
    let Some(source_x) = command_word(commands, offset, 15) else {
        return false;
    };
    let Some(source_y) = command_word(commands, offset, 16) else {
        return false;
    };
    let Some(source_width) = command_word(commands, offset, 18) else {
        return false;
    };
    let Some(source_height) = command_word(commands, offset, 19) else {
        return false;
    };
    destination.bind & (1 << 1) != 0
        && source.bind & (1 << 3) != 0
        && destination.format == destination_format
        && source.format == source_format
        && destination_width != 0
        && destination_height != 0
        && source_width != 0
        && source_height != 0
        && destination_x
            .checked_add(destination_width)
            .is_some_and(|right| right <= destination.width)
        && destination_y
            .checked_add(destination_height)
            .is_some_and(|bottom| bottom <= destination.height)
        && source_x
            .checked_add(source_width)
            .is_some_and(|right| right <= source.width)
        && source_y
            .checked_add(source_height)
            .is_some_and(|bottom| bottom <= source.height)
}

fn render_resource(resources: &[RenderResource], context: u32, id: u32) -> Option<RenderResource> {
    resources
        .iter()
        .copied()
        .find(|resource| resource.used && resource.context == context && resource.id == id)
}

fn command_word(commands: &[u8], offset: usize, payload_index: usize) -> Option<u32> {
    let start = offset
        .checked_add(4)?
        .checked_add(payload_index.checked_mul(4)?)?;
    let bytes = commands.get(start..start.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

impl Drop for VirtioGpu {
    fn drop(&mut self) {
        // cursorq не имеет response descriptor. До device reset нельзя
        // доказать, что hardware plane перестал читать backing, поэтому
        // аварийный Drop предпочитает bounded leak возможному DMA UAF.
        // Нормальный system-lifetime driver сюда не попадает.
        if self.primary.iter().all(|resource| resource.id == 0) {
            return;
        }
        if self.drain_primary_presents().is_err() {
            // Device всё ещё может владеть одним из DMA buffers.
            return;
        }
        let disable = SetScanoutRequest {
            header: self.header(CMD_SET_SCANOUT),
            rect: GpuRect::default(),
            scanout: self.scanout,
            resource: 0,
        };
        if self.command_nodata(&disable).is_err() {
            return;
        }
        let primary = self.primary;
        self.release_primary_resources(primary);
        for resource in &mut self.primary {
            resource.id = 0;
        }
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

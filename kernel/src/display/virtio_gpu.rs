//! Virtio GPU 2D display driver (Virtio 1.x modern PCI transport).
//!
//! Драйвер использует только обязательный unaccelerated 2D command set:
//! display info/EDID, resource create, attach backing, set scanout,
//! transfer-to-host и flush. VirGL и host-specific API намеренно не нужны:
//! compositor остаётся CPU-only, а протокол уже пригоден для user-space
//! `displayd` и software OpenGL surfaces.

use core::ptr;

use rustos_video::{
    ConnectorInfo, ConnectorKind, DisplayMode, ModeSetError, PixelFormat, PresentStats, Rect,
    ScanoutCapabilities, ScanoutError, Surface,
};

use crate::memory::{self, FrameBlock};

use super::{
    edid,
    pci::discover_virtio_gpu,
    virtqueue::{ModernTransport, TransportError},
};

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
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_EDID: u32 = 0x1104;
const FLAG_FENCE: u32 = 1;
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
const INTEGER_MIN_WIDTH: u32 = 800;
const INTEGER_MIN_HEIGHT: u32 = 540;

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

#[derive(Clone, Copy)]
struct Resource {
    id: u32,
    backing: FrameBlock,
    bytes: u32,
}

pub struct VirtioGpu {
    transport: ModernTransport,
    scanout: u32,
    resource: Resource,
    next_resource: u32,
    mode: DisplayMode,
    modes: [DisplayMode; MAX_MODES],
    mode_count: usize,
    width_mm: u16,
    height_mm: u16,
    fence: u64,
}

impl VirtioGpu {
    pub fn initialize(fallback: DisplayMode) -> Result<Self, ModeSetError> {
        let regions = discover_virtio_gpu().ok_or(ModeSetError::DeviceLost)?;
        let transport = ModernTransport::initialize(regions).map_err(map_transport)?;
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
            width_mm: 0,
            height_mm: 0,
            fence: 0,
        };
        let (scanout, preferred) = gpu.display_info().unwrap_or((0, fallback));
        gpu.scanout = scanout;
        gpu.add_mode(preferred);
        gpu.read_edid();
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
                format: PixelFormat::Bgr888,
                refresh_millihertz: 60_000,
            });
        }
        let connector_preferred = DisplayMode {
            format: PixelFormat::Bgr888,
            stride_pixels: preferred.width,
            ..preferred
        };
        let startup = startup_mode(connector_preferred, fallback);
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
            preferred_mode: self.modes[0],
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

    pub fn modes(&self, output: &mut [DisplayMode]) -> usize {
        let count = output.len().min(self.mode_count);
        output[..count].copy_from_slice(&self.modes[..count]);
        count
    }

    pub fn set_mode(&mut self, requested: DisplayMode) -> Result<DisplayMode, ModeSetError> {
        if requested.width == self.mode.width && requested.height == self.mode.height {
            return Ok(self.mode);
        }
        if requested.format != PixelFormat::Bgr888
            || !self.modes[..self.mode_count]
                .iter()
                .any(|mode| mode.width == requested.width && mode.height == requested.height)
        {
            return Err(ModeSetError::UnsupportedMode);
        }
        let requested = DisplayMode {
            stride_pixels: requested.width,
            format: PixelFormat::Bgr888,
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
        source: Surface<'_>,
        damage: &[Rect],
        sequence: u64,
    ) -> Result<PresentStats, ScanoutError> {
        if source.width() != self.mode.width || source.height() != self.mode.height {
            return Err(ScanoutError::InvalidSurface);
        }
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
                if source.format() == PixelFormat::Bgr888 {
                    unsafe { ptr::copy_nonoverlapping(row.as_ptr(), destination, row.len()) };
                } else {
                    for (offset, raw) in row.iter().copied().enumerate() {
                        let converted = PixelFormat::Bgr888.pack(source.format().unpack(raw));
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
                        format: PixelFormat::Bgr888,
                        refresh_millihertz: 0,
                    },
                ));
            }
        }
        Err(ModeSetError::DeviceLost)
    }

    fn read_edid(&mut self) {
        if !self.transport.edid_supported() {
            return;
        }
        let request = GetEdidRequest {
            header: self.header(CMD_GET_EDID),
            scanout: self.scanout,
            padding: 0,
        };
        let Ok(response) = self.transport.command::<_, EdidResponse>(&request) else {
            return;
        };
        if response.header.kind != RESP_OK_EDID {
            return;
        }
        let size = (response.size as usize).min(response.bytes.len());
        let Some(info) = edid::parse(&response.bytes[..size]) else {
            return;
        };
        self.width_mm = info.width_mm;
        self.height_mm = info.height_mm;
        for mode in info.modes[..info.mode_count].iter().copied() {
            self.add_mode(DisplayMode {
                width: mode.width,
                height: mode.height,
                stride_pixels: mode.width,
                format: PixelFormat::Bgr888,
                refresh_millihertz: mode.refresh_millihertz,
            });
        }
    }

    fn add_mode(&mut self, mode: DisplayMode) {
        if mode.width < MIN_WIDTH
            || mode.height < MIN_HEIGHT
            || mode.width > MAX_WIDTH
            || mode.height > MAX_HEIGHT
            || self.modes[..self.mode_count]
                .iter()
                .any(|existing| existing.width == mode.width && existing.height == mode.height)
        {
            return;
        }
        if self.mode_count < MAX_MODES {
            self.modes[self.mode_count] = DisplayMode {
                stride_pixels: mode.width,
                format: PixelFormat::Bgr888,
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
        let request = FlushRequest {
            header: self.header(CMD_RESOURCE_FLUSH),
            rect: GpuRect {
                x: rect.x as u32,
                y: rect.y as u32,
                width: rect.width,
                height: rect.height,
            },
            resource: self.resource.id,
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

/// Ограничивает только начальный logical mode. Если monitor меньше лимита,
/// сохраняется его preferred mode; ручной `DISPLAY MODE` не ограничивается.
fn startup_mode(preferred: DisplayMode, fallback: DisplayMode) -> DisplayMode {
    let preferred_is_usable = preferred.width >= MIN_WIDTH && preferred.height >= MIN_HEIGHT;
    if preferred_is_usable
        && preferred.width <= STARTUP_MAX_WIDTH
        && preferred.height <= STARTUP_MAX_HEIGHT
    {
        return preferred;
    }
    if preferred_is_usable {
        if let Some((width, height)) = integer_fit_dimensions(preferred.width, preferred.height) {
            return DisplayMode {
                width,
                height,
                stride_pixels: width,
                format: PixelFormat::Bgr888,
                refresh_millihertz: preferred.refresh_millihertz.max(60_000),
            };
        }
    }
    let fallback_is_usable = fallback.width >= MIN_WIDTH
        && fallback.height >= MIN_HEIGHT
        && fallback.width <= STARTUP_MAX_WIDTH
        && fallback.height <= STARTUP_MAX_HEIGHT;
    let (width, height) = if preferred_is_usable && preferred.width >= 16 * preferred.height / 10 {
        (STARTUP_MAX_WIDTH, STARTUP_MAX_HEIGHT)
    } else if fallback_is_usable {
        (fallback.width, fallback.height)
    } else {
        (1280, 800)
    };
    DisplayMode {
        width,
        height,
        stride_pixels: width,
        format: PixelFormat::Bgr888,
        refresh_millihertz: preferred.refresh_millihertz.max(60_000),
    }
}

/// Подбирает surface, которую host увеличит на целое число.
///
/// Cocoa сообщает virtio-gpu fullscreen backing size ещё до старта kernel:
/// например, 2880×1800. Прежний clamp превращал его в 1600×900 и давал
/// дробный коэффициент 1.8. Теперь выбирается 1440×900 ×2. Если только одна
/// ось выходит за startup budget, вторая остаётся точной ограничивающей осью,
/// а свободное место становится letterbox без масштабирования bitmap.
fn integer_fit_dimensions(host_width: u32, host_height: u32) -> Option<(u32, u32)> {
    for scale in 2..=6 {
        if host_width % scale != 0 || host_height % scale != 0 {
            continue;
        }
        let desired_width = host_width / scale;
        let desired_height = host_height / scale;
        if desired_width > STARTUP_MAX_WIDTH && desired_height > STARTUP_MAX_HEIGHT {
            continue;
        }
        let width = desired_width.min(STARTUP_MAX_WIDTH);
        let height = desired_height.min(STARTUP_MAX_HEIGHT);
        if width >= INTEGER_MIN_WIDTH && height >= INTEGER_MIN_HEIGHT {
            return Some((width, height));
        }
    }
    None
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
        TransportError::RejectedFeatures | TransportError::Timeout => ModeSetError::DeviceLost,
    }
}

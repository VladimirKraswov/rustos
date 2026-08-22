//! Единственный kernel broker scanout-устройства.
//!
//! Virtio transport остаётся kernel mechanism, но authority отделена от
//! framebuffer и выражается `DisplayScanout` capability. Bootstrap GUI
//! обращается сюда напрямую только как аварийный in-kernel клиент; ring-3
//! `displayd` проходит тот же сериализованный driver path через syscall ABI.

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

use rustos_abi::{
    display::{scanout_capabilities, DisplayScanoutInfo, DISPLAY_SCANOUT_ABI_VERSION},
    gpu::{GpuDeviceInfo, GpuResourceCreate},
    graphics_buffer::GraphicsBufferDesc,
    surface::OutputId,
};
use rustos_video::{
    ConnectorInfo, CpuSurface, DisplayMode, ModeSetError, PresentStats, Rect, ScanoutCapabilities,
    ScanoutError,
};

use super::{virtio_gpu::RenderCompletion, VirtioGpu};
use crate::serial;

const DEFAULT_REFRESH_MILLIHERTZ: u32 = 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayBrokerError {
    Unavailable,
    Busy,
    InvalidSurface,
    DeviceLost,
    UnsupportedMode,
    OutOfMemory,
}

impl DisplayBrokerError {
    pub const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Busy => "busy",
            Self::InvalidSurface => "invalid-surface",
            Self::DeviceLost => "device-lost",
            Self::UnsupportedMode => "unsupported-mode",
            Self::OutOfMemory => "out-of-memory",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayDeviceInfo {
    pub transport: &'static str,
    pub mode: DisplayMode,
    pub preferred_mode: DisplayMode,
    pub edid_valid: bool,
    pub virgl_ready: bool,
    pub outputs: u32,
}

struct Device {
    gpu: VirtioGpu,
    mode_generation: u64,
}

struct LockedDevice {
    locked: AtomicBool,
    value: UnsafeCell<Option<Device>>,
}

// Доступ к VirtioGpu сериализуется spinlock'ом. AP-ядра пока parked, но это
// также не позволяет будущему syscall path пересечься с bootstrap present.
unsafe impl Sync for LockedDevice {}

impl LockedDevice {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }

    fn acquire(&self) -> Result<DeviceGuard<'_>, DisplayBrokerError> {
        // Display driver вызывается из bounded syscall/present path. Вместо
        // unbounded spin возвращаем BUSY после ограниченного числа попыток.
        for _ in 0..4096 {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(DeviceGuard { device: self });
            }
            core::hint::spin_loop();
        }
        Err(DisplayBrokerError::Busy)
    }

    fn install(&self, gpu: VirtioGpu) -> Result<(), DisplayBrokerError> {
        let mut guard = self.acquire()?;
        if guard.get().is_some() {
            return Ok(());
        }
        *guard.get() = Some(Device {
            gpu,
            mode_generation: 1,
        });
        Ok(())
    }
}

struct DeviceGuard<'a> {
    device: &'a LockedDevice,
}

impl DeviceGuard<'_> {
    fn get(&mut self) -> &mut Option<Device> {
        // SAFETY: guard существует только после успешного Acquire и снимает
        // lock в Drop; другой CPU не получает mutable reference одновременно.
        unsafe { &mut *self.device.value.get() }
    }
}

impl Drop for DeviceGuard<'_> {
    fn drop(&mut self) {
        self.device.locked.store(false, Ordering::Release);
    }
}

static DEVICE: LockedDevice = LockedDevice::new();

/// Один раз инициализирует native virtio-gpu transport.
pub fn initialize(fallback: DisplayMode) -> Result<DisplayMode, DisplayBrokerError> {
    if let Ok(mode) = mode() {
        return Ok(mode);
    }
    let gpu = VirtioGpu::initialize(fallback).map_err(map_mode_error)?;
    let selected = gpu.mode();
    DEVICE.install(gpu)?;
    if let Ok(info) = device_info() {
        log_hardware_report(info);
    }
    Ok(selected)
}

fn log_hardware_report(device: DisplayDeviceInfo) {
    serial::put_str("[hardware] display-driver=virtio-gpu transport=");
    serial::put_str(device.transport);
    serial::put_str(" mode=");
    serial::put_u32(device.mode.width);
    serial::put_str("x");
    serial::put_u32(device.mode.height);
    serial::put_str(" preferred=");
    serial::put_u32(device.preferred_mode.width);
    serial::put_str("x");
    serial::put_u32(device.preferred_mode.height);
    serial::put_str(" edid=");
    serial::put_str(if device.edid_valid {
        "valid"
    } else {
        "unavailable"
    });
    serial::put_str(" outputs=");
    serial::put_u32(device.outputs);
    serial::put_str(" renderer=");
    serial::put_str(if device.virgl_ready { "virgl" } else { "cpu" });
    serial::put_str("\n");
}

pub fn mode() -> Result<DisplayMode, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_ref()
        .map(|device| device.gpu.mode())
        .ok_or(DisplayBrokerError::Unavailable)
}

pub fn connector() -> Result<ConnectorInfo, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_ref()
        .map(|device| device.gpu.connector())
        .ok_or(DisplayBrokerError::Unavailable)
}

pub fn device_info() -> Result<DisplayDeviceInfo, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    let device = guard
        .get()
        .as_ref()
        .ok_or(DisplayBrokerError::Unavailable)?;
    Ok(DisplayDeviceInfo {
        #[cfg(target_arch = "x86_64")]
        transport: "modern-pci",
        #[cfg(target_arch = "aarch64")]
        transport: "modern-mmio",
        mode: device.gpu.mode(),
        preferred_mode: device.gpu.connector().preferred_mode,
        edid_valid: device.gpu.edid_valid(),
        virgl_ready: device.gpu.virgl_ready(),
        outputs: device.gpu.output_count(),
    })
}

pub fn capabilities() -> Result<ScanoutCapabilities, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_ref()
        .map(|device| device.gpu.capabilities())
        .ok_or(DisplayBrokerError::Unavailable)
}

pub fn modes(output: &mut [DisplayMode]) -> Result<usize, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_ref()
        .map(|device| device.gpu.modes(output))
        .ok_or(DisplayBrokerError::Unavailable)
}

pub fn set_mode(requested: DisplayMode) -> Result<DisplayMode, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    let device = guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?;
    let previous = device.gpu.mode();
    let selected = device.gpu.set_mode(requested).map_err(map_mode_error)?;
    if selected.width != previous.width || selected.height != previous.height {
        device.mode_generation = device.mode_generation.wrapping_add(1).max(1);
    }
    Ok(selected)
}

pub fn present(
    source: CpuSurface<'_>,
    damage: &[Rect],
    sequence: u64,
) -> Result<PresentStats, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .present(source, damage, sequence)
        .map_err(map_scanout_error)
}

/// Прямой full-frame commit capability-backed buffer'а.
pub fn present_graphics<PhysicalPage>(
    graphics_object: u16,
    descriptor: GraphicsBufferDesc,
    physical_page: PhysicalPage,
    sequence: u64,
) -> Result<PresentStats, DisplayBrokerError>
where
    PhysicalPage: FnMut(usize) -> Option<u64>,
{
    let mut guard = DEVICE.acquire()?;
    let device = guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?;
    if let Some(stats) = device
        .gpu
        .present_imported(graphics_object, sequence)
        .map_err(map_scanout_error)?
    {
        return Ok(stats);
    }
    device
        .gpu
        .present_pages(descriptor, physical_page, sequence)
        .map_err(map_scanout_error)
}

pub fn render_info() -> Result<GpuDeviceInfo, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_ref()
        .and_then(|device| device.gpu.render_info())
        .ok_or(DisplayBrokerError::Unavailable)
}

pub fn create_render_context(context: u32, name: &[u8]) -> Result<(), DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .create_render_context(context, name)
        .map_err(map_mode_error)
}

pub fn import_render_target(
    context: u32,
    graphics_object: u16,
    descriptor: GraphicsBufferDesc,
    backing: crate::memory::FrameBlock,
) -> Result<u32, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .import_render_target(context, graphics_object, descriptor, backing)
        .map_err(map_mode_error)
}

pub fn create_render_resource(
    context: u32,
    request: GpuResourceCreate,
) -> Result<u32, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .create_render_resource(context, request)
        .map_err(map_mode_error)
}

pub fn submit_render(context: u32, commands: &[u8]) -> Result<u64, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .submit_render(context, commands)
        .map_err(map_mode_error)
}

pub fn poll_render() -> Result<Option<RenderCompletion>, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .poll_render()
        .map_err(map_mode_error)
}

/// Безопасно получает следующий completion перед уничтожением GPU context.
/// Это не часть кадрового hot path: blocking разрешён только при
/// завершении/падении renderd. Конкретный fence сопоставляет process manager,
/// потому что одновременно могут выполняться несколько независимых кадров.
pub fn drain_next_render() -> Result<RenderCompletion, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .drain_next_render()
        .map_err(map_mode_error)
}

/// Копирует завершённый VirGL render target в attached guest-memory backing.
/// Это не rasterization: pixels создаёт host GPU, команда лишь выполняет
/// explicit readback для временного kernel compositor bridge.
pub fn download_render_target(graphics_object: u16) -> Result<(), DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    guard
        .get()
        .as_mut()
        .ok_or(DisplayBrokerError::Unavailable)?
        .gpu
        .download_imported(graphics_object)
        .map_err(map_mode_error)
}

pub fn destroy_render_context(context: u32) {
    let Ok(mut guard) = DEVICE.acquire() else {
        return;
    };
    if let Some(device) = guard.get().as_mut() {
        device.gpu.destroy_render_context(context);
    }
}

/// Wire snapshot для process ABI. Virtio-gpu 2D завершает fenced FLUSH, но
/// отдельного vblank IRQ не экспортирует, поэтому timing явно estimated.
pub fn info() -> Result<DisplayScanoutInfo, DisplayBrokerError> {
    let mut guard = DEVICE.acquire()?;
    let device = guard
        .get()
        .as_ref()
        .ok_or(DisplayBrokerError::Unavailable)?;
    let mode = device.gpu.mode();
    Ok(DisplayScanoutInfo {
        version: DISPLAY_SCANOUT_ABI_VERSION,
        size: core::mem::size_of::<DisplayScanoutInfo>() as u16,
        flags: 0,
        output: OutputId(1),
        width: mode.width,
        height: mode.height,
        stride_bytes: mode.width.saturating_mul(4),
        format: rustos_abi::graphics_buffer::PixelFormatCode::B8G8R8X8_UNORM,
        refresh_millihertz: mode.refresh_millihertz.max(DEFAULT_REFRESH_MILLIHERTZ),
        capabilities: scanout_capabilities::ATOMIC_PRESENT
            | scanout_capabilities::ESTIMATED_VBLANK
            | scanout_capabilities::MODE_SET,
        mode_generation: device.mode_generation,
        reserved: [0; 2],
    })
}

pub fn refresh_interval_ns() -> Result<u64, DisplayBrokerError> {
    let refresh = u64::from(info()?.refresh_millihertz);
    Ok(1_000_000_000_000u64.div_ceil(refresh))
}

fn map_scanout_error(error: ScanoutError) -> DisplayBrokerError {
    match error {
        ScanoutError::InvalidSurface | ScanoutError::UnsupportedFormat => {
            DisplayBrokerError::InvalidSurface
        }
        ScanoutError::DeviceLost => DisplayBrokerError::DeviceLost,
    }
}

fn map_mode_error(error: ModeSetError) -> DisplayBrokerError {
    match error {
        ModeSetError::UnsupportedMode | ModeSetError::RequiresReboot => {
            DisplayBrokerError::UnsupportedMode
        }
        ModeSetError::OutOfMemory => DisplayBrokerError::OutOfMemory,
        ModeSetError::DeviceLost => DisplayBrokerError::DeviceLost,
    }
}

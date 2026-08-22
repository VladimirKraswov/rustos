//! Сквозной ring-3 тест `surface.dll` и compositord.
//!
//! Процесс создаёт приватный event endpoint, публикует capability-backed
//! полноэкранный buffer, получает release timeline и presentation feedback,
//! затем уничтожает surface. Kernel не видит surface policy и не копирует
//! pixels между процессами.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    graphics_buffer::{BufferUsage, GraphicsBufferDesc, MemoryDomain, PixelFormatCode},
    memory::MEMORY_ABI_VERSION,
    surface::{feedback_flags, PresentationStatus, SurfaceMetrics},
    sync::{SyncTimelineCreate, SyncTimelineSignal, SyncTimelineWait, SYNC_TIMEOUT_INFINITE},
};
use rustos_runtime::{
    graphics_buffer_create, graphics_buffer_map, handle_close, process_exit, sync_timeline_create,
    sync_timeline_signal, sync_timeline_wait, syscall, vm_unmap, Handle, SharedMemoryMap, VmFlags,
};
use rustos_surface::{SurfaceClient, SurfaceEvent};

const FRAME_MAGIC: u64 = 0x5355_5246_4143_4531;

#[no_mangle]
pub extern "C" fn _start(compositor_endpoint: u64, packed_dimensions: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(241);
    }
    let width = packed_dimensions as u32;
    let height = (packed_dimensions >> 32) as u32;
    let metrics = SurfaceMetrics::new(width, height, width, height, 1000);
    let mut client = match SurfaceClient::connect(Handle(compositor_endpoint as u32), metrics, 3) {
        Ok(client) if client.queue_depth() == 3 && client.generation() != 0 => client,
        _ => process_exit(242),
    };

    let usage = BufferUsage::CPU_READ
        .union(BufferUsage::CPU_WRITE)
        .union(BufferUsage::RENDER_TARGET)
        .union(BufferUsage::SCANOUT);
    let domains = MemoryDomain::SYSTEM
        .union(MemoryDomain::HOST_VISIBLE)
        .union(MemoryDomain::SHARED);
    let descriptor = match GraphicsBufferDesc::linear(
        width,
        height,
        PixelFormatCode::B8G8R8A8_UNORM,
        usage,
        domains,
    ) {
        Ok(descriptor) => descriptor,
        Err(_) => process_exit(243),
    };
    let buffer_value = graphics_buffer_create(&descriptor);
    if buffer_value <= 0 {
        process_exit(244);
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
    let address = graphics_buffer_map(buffer, &mapping);
    if address <= 0 {
        process_exit(245);
    }
    unsafe {
        (address as *mut u64).write_volatile(FRAME_MAGIC);
        ((address as *mut u32).add((u64::from(width) * u64::from(height) - 1) as usize))
            .write_volatile(0xff_1c_78_e8);
    }
    if vm_unmap(address as u64, mapped_length) != syscall::status::OK {
        process_exit(246);
    }

    let acquire_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if acquire_value <= 0 {
        process_exit(247);
    }
    let acquire = Handle(acquire_value as u32);
    if sync_timeline_signal(&SyncTimelineSignal::new(acquire, 1)) != syscall::status::OK
        || client.commit_full(0, 1, buffer, acquire, true).is_err()
    {
        process_exit(248);
    }

    let (release, release_value) = match client.receive_event() {
        Ok(SurfaceEvent::BufferReleased {
            info,
            release_timeline,
        }) if info.surface == client.id()
            && info.frame_id == 1
            && info.buffer_slot == 0
            && info.release_value != 0 =>
        {
            (release_timeline, info.release_value)
        }
        _ => process_exit(249),
    };
    if sync_timeline_wait(&SyncTimelineWait::new(
        release,
        release_value,
        SYNC_TIMEOUT_INFINITE,
    )) != syscall::status::OK
    {
        process_exit(250);
    }
    match client.receive_event() {
        Ok(SurfaceEvent::Presentation(feedback))
            if feedback.surface == client.id()
                && feedback.frame_id == 1
                && feedback.status == PresentationStatus::PRESENTED
                && feedback.flags == feedback_flags::DIRECT_SCANOUT => {}
        _ => process_exit(251),
    }
    if handle_close(release) != syscall::status::OK
        || handle_close(buffer) != syscall::status::OK
        || handle_close(acquire) != syscall::status::OK
        || client.disconnect().is_err()
    {
        process_exit(252);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(259)
}

//! Первый ring-3 `compositord` vertical slice.
//!
//! Сервис формирует client-owned surface frame, публикует buffer в displayd и
//! блокируется на release timeline через wait-many. Ни pixel data, ни
//! process-local pointer через IPC не передаются.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    display::{DisplayPresentRequest, DISPLAY_PRESENT_HANDLE_COUNT, DISPLAY_PRESENT_OPCODE},
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
    graphics_buffer_create, graphics_buffer_map, handle_close, ipc_send, process_exit,
    shared_memory_create, shared_memory_map, sync_timeline_create, sync_timeline_signal,
    sync_timeline_wait_many, syscall, vm_unmap, Handle, Message, Rights, SharedMemoryCreate,
    SharedMemoryMap, VmFlags,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FRAME_MAGIC: u64 = 0x5255_5354_4f53_4758;

#[no_mangle]
pub extern "C" fn _start(display_endpoint: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(181);
    }
    let usage = BufferUsage::CPU_READ
        .union(BufferUsage::CPU_WRITE)
        .union(BufferUsage::RENDER_TARGET)
        .union(BufferUsage::SCANOUT);
    let domains = MemoryDomain::SYSTEM
        .union(MemoryDomain::HOST_VISIBLE)
        .union(MemoryDomain::SHARED);
    let descriptor = match GraphicsBufferDesc::linear(
        WIDTH,
        HEIGHT,
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
        ((address as *mut u32).add((WIDTH * HEIGHT - 1) as usize)).write_volatile(0xff_24_80_ff);
    }
    if vm_unmap(address as u64, mapped_length) != syscall::status::OK {
        process_exit(186);
    }

    let acquire_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    let release_value = sync_timeline_create(&SyncTimelineCreate::new(0));
    if acquire_value <= 0 || release_value <= 0 {
        process_exit(187);
    }
    let acquire = Handle(acquire_value as u32);
    let release = Handle(release_value as u32);
    if sync_timeline_signal(&SyncTimelineSignal::new(acquire, 1)) != syscall::status::OK
        || sync_timeline_signal(&SyncTimelineSignal::new(acquire, 0))
            != syscall::status::INVALID_ARGUMENT
    {
        process_exit(188);
    }

    let metrics = SurfaceMetrics::new(WIDTH, HEIGHT, WIDTH, HEIGHT, 1000);
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
        rights: Rights::READ.union(Rights::MAP),
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
    if ipc_send(Handle(display_endpoint as u32), &message) != syscall::status::OK {
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
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(199)
}

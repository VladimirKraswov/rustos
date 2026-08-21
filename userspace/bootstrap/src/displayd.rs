//! Первый изолированный `displayd`: принимает готовый graphics buffer,
//! ожидает acquire timeline и возвращает release timeline.
//!
//! На этом milestone backend намеренно headless: hardware display capability
//! ещё не выдана сервису. Но memory/sync/capability path уже тот же, который
//! будет использовать virtio-gpu atomic present.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    display::{DisplayPresentRequest, DISPLAY_PRESENT_HANDLE_COUNT, DISPLAY_PRESENT_OPCODE},
    graphics_buffer::{
        AlphaMode, BufferUsage, ColorDescription, GraphicsBufferDesc, MemoryDomain,
        PixelFormatCode, PlaneLayout, GRAPHICS_BUFFER_ABI_VERSION,
    },
    memory::MEMORY_ABI_VERSION,
    sync::{SyncTimelineSignal, SyncTimelineWait, SYNC_TIMEOUT_INFINITE},
};
use rustos_runtime::{
    graphics_buffer_get_info, graphics_buffer_map, handle_close, ipc_receive, process_exit,
    sync_timeline_signal, sync_timeline_wait, syscall, vm_unmap, Handle, Message, SharedMemoryMap,
    VmFlags,
};

const FRAME_MAGIC: u64 = 0x5255_5354_4f53_4758;

#[no_mangle]
pub extern "C" fn _start(endpoint: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(171);
    }
    let mut message = Message::EMPTY;
    if ipc_receive(Handle(endpoint as u32), &mut message) != syscall::status::OK
        || message.header.opcode != DISPLAY_PRESENT_OPCODE
        || message.header.payload_len != 64
        || message.header.handle_count != DISPLAY_PRESENT_HANDLE_COUNT
        || message.header.sender_pid == 0
    {
        process_exit(172);
    }
    let present = match DisplayPresentRequest::decode_inline(&message.payload) {
        Ok(present) => present,
        Err(_) => process_exit(173),
    };
    let buffer = message.handles[0].handle;
    let acquire = message.handles[1].handle;
    let release = message.handles[2].handle;

    let mut descriptor = empty_descriptor();
    // Timeline capability не может подменить graphics buffer, даже если
    // числовой handle выглядит корректно в локальной таблице displayd.
    if graphics_buffer_get_info(acquire, &mut descriptor) != syscall::status::ACCESS_DENIED
        || graphics_buffer_get_info(buffer, &mut descriptor) != syscall::status::OK
        || descriptor.validate().is_err()
        || descriptor.width != present.width
        || descriptor.height != present.height
        || descriptor.format != present.format
        || descriptor.planes[0].stride_bytes != present.stride_bytes
        || descriptor.byte_size != present.byte_size
    {
        process_exit(174);
    }
    if sync_timeline_wait(&SyncTimelineWait::new(
        acquire,
        present.acquire_value,
        SYNC_TIMEOUT_INFINITE,
    )) != syscall::status::OK
    {
        process_exit(175);
    }
    let mapped_length = descriptor.byte_size.div_ceil(4096) * 4096;
    let mapping = SharedMemoryMap {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        offset: 0,
        length: mapped_length,
        flags: VmFlags::READ,
    };
    let address = graphics_buffer_map(buffer, &mapping);
    if address <= 0 || unsafe { (address as *const u64).read_volatile() } != FRAME_MAGIC {
        process_exit(176);
    }
    if vm_unmap(address as u64, mapped_length) != syscall::status::OK
        || sync_timeline_signal(&SyncTimelineSignal::new(release, present.release_value))
            != syscall::status::OK
        || handle_close(buffer) != syscall::status::OK
        || handle_close(acquire) != syscall::status::OK
        || handle_close(release) != syscall::status::OK
    {
        process_exit(177);
    }
    process_exit(0)
}

fn empty_descriptor() -> GraphicsBufferDesc {
    GraphicsBufferDesc {
        version: GRAPHICS_BUFFER_ABI_VERSION,
        size: core::mem::size_of::<GraphicsBufferDesc>() as u16,
        width: 0,
        height: 0,
        format: PixelFormatCode(0),
        plane_count: 0,
        alpha_mode: AlphaMode::OPAQUE,
        reserved_header: 0,
        reserved_alignment: 0,
        usage: BufferUsage::NONE,
        memory_domains: MemoryDomain::SYSTEM,
        flags: 0,
        byte_size: 0,
        modifier: 0,
        color: ColorDescription::SRGB,
        planes: [PlaneLayout::EMPTY; 4],
        reserved_tail: [0; 2],
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(179)
}

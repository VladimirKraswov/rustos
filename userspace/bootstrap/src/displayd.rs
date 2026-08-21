//! Постоянный ring-3 `displayd` с эксклюзивной scanout capability.
//!
//! Сервис не видит MMIO/physical frames: kernel object выполняет только
//! validated atomic present и vblank wait. Любой protocol/device fault
//! завершает этот процесс, после чего supervisor перезапускает display stack.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    display::{
        DisplayAtomicPresent, DisplayPresentFeedback, DisplayPresentRequest, DisplayScanoutInfo,
        DisplayVblankWait, DISPLAY_FEEDBACK_OPCODE, DISPLAY_INFO_OPCODE,
        DISPLAY_PRESENT_HANDLE_COUNT, DISPLAY_PRESENT_OPCODE, DISPLAY_QUERY_HANDLE_COUNT,
        DISPLAY_QUERY_OPCODE, DISPLAY_SCANOUT_ABI_VERSION,
    },
    graphics_buffer::{
        AlphaMode, BufferUsage, ColorDescription, GraphicsBufferDesc, MemoryDomain,
        PixelFormatCode, PlaneLayout, GRAPHICS_BUFFER_ABI_VERSION,
    },
    sync::{SyncTimelineSignal, SyncTimelineWait, SYNC_TIMEOUT_INFINITE},
};
use rustos_runtime::{
    display_atomic_present, display_get_info, display_wait_vblank, graphics_buffer_get_info,
    handle_close, ipc_receive, ipc_send, monotonic_time_ns, process_exit, sync_timeline_signal,
    sync_timeline_wait, syscall, Handle, Message,
};

#[no_mangle]
pub extern "C" fn _start(endpoint: u64, scanout: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(171);
    }
    let endpoint = Handle(endpoint as u32);
    let scanout = Handle(scanout as u32);
    let mut info = empty_scanout_info();
    if display_get_info(scanout, &mut info) != syscall::status::OK || info.validate().is_err() {
        process_exit(172);
    }

    loop {
        let mut message = Message::EMPTY;
        if ipc_receive(endpoint, &mut message) != syscall::status::OK
            || message.header.sender_pid == 0
        {
            process_exit(173);
        }
        match message.header.opcode {
            DISPLAY_QUERY_OPCODE => handle_query(scanout, &message),
            DISPLAY_PRESENT_OPCODE => handle_present(scanout, &message),
            _ => process_exit(174),
        }
    }
}

fn handle_query(scanout: Handle, message: &Message) {
    if message.header.payload_len != 0 || message.header.handle_count != DISPLAY_QUERY_HANDLE_COUNT
    {
        process_exit(175);
    }
    let reply = message.handles[0].handle;
    let mut info = empty_scanout_info();
    if display_get_info(scanout, &mut info) != syscall::status::OK || info.validate().is_err() {
        process_exit(176);
    }
    let mut response = Message::EMPTY;
    response.header.opcode = DISPLAY_INFO_OPCODE;
    response.header.flags = rustos_abi::ipc::flags::REPLY;
    response.header.request_id = message.header.request_id;
    response.header.payload_len = 64;
    response.payload = info.encode_inline();
    if ipc_send(reply, &response) != syscall::status::OK
        || handle_close(reply) != syscall::status::OK
    {
        process_exit(177);
    }
}

fn handle_present(scanout: Handle, message: &Message) {
    if message.header.payload_len != 64
        || message.header.handle_count != DISPLAY_PRESENT_HANDLE_COUNT
    {
        process_exit(178);
    }
    let present = match DisplayPresentRequest::decode_inline(&message.payload) {
        Ok(present) => present,
        Err(_) => process_exit(179),
    };
    let buffer = message.handles[0].handle;
    let acquire = message.handles[1].handle;
    let release = message.handles[2].handle;
    let feedback_endpoint = message.handles[3].handle;
    let mut descriptor = empty_descriptor();
    let mut info = empty_scanout_info();

    // Ни timeline, ни GraphicsBuffer не могут подменить эксклюзивный scanout.
    if display_get_info(acquire, &mut info) != syscall::status::ACCESS_DENIED
        || display_get_info(scanout, &mut info) != syscall::status::OK
        || graphics_buffer_get_info(acquire, &mut descriptor) != syscall::status::ACCESS_DENIED
        || graphics_buffer_get_info(buffer, &mut descriptor) != syscall::status::OK
        || descriptor.validate().is_err()
        || descriptor.width != present.width
        || descriptor.height != present.height
        || descriptor.format != present.format
        || descriptor.planes[0].stride_bytes != present.stride_bytes
        || descriptor.byte_size != present.byte_size
        || descriptor.width != info.width
        || descriptor.height != info.height
    {
        process_exit(180);
    }
    if sync_timeline_wait(&SyncTimelineWait::new(
        acquire,
        present.acquire_value,
        SYNC_TIMEOUT_INFINITE,
    )) != syscall::status::OK
    {
        process_exit(181);
    }
    let sequence = display_atomic_present(
        scanout,
        buffer,
        &DisplayAtomicPresent::new(present.frame_id, info.mode_generation),
    );
    if sequence <= 0
        || display_wait_vblank(
            scanout,
            &DisplayVblankWait::new(sequence as u64, 1_000_000_000),
        ) != syscall::status::OK
    {
        process_exit(182);
    }
    let actual_time = monotonic_time_ns();
    let refresh_interval = 1_000_000_000_000u64.div_ceil(u64::from(info.refresh_millihertz));
    if actual_time <= 0
        || sync_timeline_signal(&SyncTimelineSignal::new(release, present.release_value))
            != syscall::status::OK
    {
        process_exit(183);
    }
    let feedback = DisplayPresentFeedback::presented(
        present.frame_id,
        sequence as u64,
        actual_time as u64,
        refresh_interval,
        info.output,
    );
    let mut response = Message::EMPTY;
    response.header.opcode = DISPLAY_FEEDBACK_OPCODE;
    response.header.flags = rustos_abi::ipc::flags::REPLY;
    response.header.request_id = message.header.request_id;
    response.header.payload_len = 64;
    response.payload = feedback.encode_inline();
    if ipc_send(feedback_endpoint, &response) != syscall::status::OK
        || handle_close(buffer) != syscall::status::OK
        || handle_close(acquire) != syscall::status::OK
        || handle_close(release) != syscall::status::OK
        || handle_close(feedback_endpoint) != syscall::status::OK
    {
        process_exit(184);
    }
}

fn empty_scanout_info() -> DisplayScanoutInfo {
    DisplayScanoutInfo {
        version: DISPLAY_SCANOUT_ABI_VERSION,
        size: core::mem::size_of::<DisplayScanoutInfo>() as u16,
        flags: 0,
        output: rustos_abi::surface::OutputId::NONE,
        width: 0,
        height: 0,
        stride_bytes: 0,
        format: PixelFormatCode(0),
        refresh_millihertz: 0,
        capabilities: 0,
        mode_generation: 0,
        reserved: [0; 2],
    }
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
    process_exit(199)
}

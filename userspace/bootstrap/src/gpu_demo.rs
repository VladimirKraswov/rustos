//! Системное ring-3 приложение «Aurora 3D».
//!
//! Приложение не получает GPU capability. Оно передаёт compositor'у только
//! bounded intent «показать N кадров», после чего завершается. Это важная
//! учебная граница: untrusted UI/app не может управлять virtqueue или scanout.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::gpu::{GpuDemoRequest, GPU_DEMO_START_OPCODE};
use rustos_runtime::{ipc_send, process_exit, syscall, Handle, Message};

#[no_mangle]
pub extern "C" fn _start(compositor_endpoint: u64, frame_count: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(231);
    }
    let frames = u32::try_from(frame_count).unwrap_or(rustos_mesa::DEFAULT_DEMO_FRAMES);
    let request = GpuDemoRequest::new(frames);
    if request.validate().is_err() {
        process_exit(232);
    }
    let mut message = Message::EMPTY;
    message.header.opcode = GPU_DEMO_START_OPCODE;
    message.header.request_id = 1;
    message.header.payload_len = 64;
    message.payload = request.encode_inline();
    if ipc_send(Handle(compositor_endpoint as u32), &message) != syscall::status::OK {
        process_exit(233);
    }
    process_exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(239)
}

//! Минимальный runtime программ RustOS.
//!
//! Здесь нет драйверов и filesystem parser'а. Runtime содержит только
//! стабильные syscall wrappers; `vfs.dll` позднее добавит удобный C ABI и
//! batching поверх тех же capability-вызовов.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;

mod arch;

pub use rustos_abi::{ipc::Message, syscall, Handle, Rights};

/// Bootstrap capability корневого VFS namespace текущего процесса.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct VfsCapability(pub Handle);

/// Выполняет `vfs_stat` для UTF-8 пути. Положительный результат — размер,
/// отрицательный — `rustos_abi::syscall::status`.
pub fn vfs_stat(capability: VfsCapability, path: &str) -> i64 {
    unsafe {
        syscall3(
            syscall::number::VFS_STAT,
            capability.0 .0 as u64,
            path.as_ptr() as u64,
            path.len() as u64,
        )
    }
}

/// Завершает процесс. Kernel освободит address space и известит supervisor.
pub fn process_exit(status: i32) -> ! {
    unsafe {
        let _ = syscall3(syscall::number::PROCESS_EXIT, status as i64 as u64, 0, 0);
    }
    // `process_exit` не возвращается. Цикл — защита от дефекта kernel ABI.
    loop {
        core::hint::spin_loop();
    }
}

/// Отправляет небольшое inline IPC-сообщение. Capability handles внутри
/// сообщения ядро заменяет производными handles таблицы получателя.
pub fn ipc_send(endpoint: Handle, message: &Message) -> i64 {
    unsafe {
        syscall3(
            syscall::number::IPC_SEND,
            endpoint.0 as u64,
            message as *const Message as u64,
            0,
        )
    }
}

/// Получает следующее сообщение. При пустой очереди kernel блокирует поток и
/// переключает CPU; после wake функция возвращает уже заполненный buffer.
pub fn ipc_receive(endpoint: Handle, message: &mut Message) -> i64 {
    unsafe {
        syscall3(
            syscall::number::IPC_RECEIVE,
            endpoint.0 as u64,
            message as *mut Message as u64,
            0,
        )
    }
}

/// Низкоуровневый ABI. Номера операций и семантика аргументов общие, а
/// регистры/инструкция входа выбираются ISA backend'ом ниже.
///
/// # Safety
///
/// Указатели в аргументах должны ссылаться на user mappings текущего
/// процесса. Kernel всё равно проверяет диапазоны и права страниц.
#[inline]
pub unsafe fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { arch::syscall3(number, arg0, arg1, arg2) }
}

/// Монотонный аппаратный counter без единиц времени. Используется тестами
/// вытеснения; частоту в обычном приложении сообщает системный clock service.
pub fn monotonic_counter() -> u64 {
    arch::monotonic_counter()
}

/// Намеренно создаёт illegal-instruction fault для проверки изоляции.
pub fn trigger_test_fault() -> ! {
    arch::trigger_test_fault()
}

// Freestanding user ELF иногда получает ссылки на эти C builtins из core.
// Побайтовые реализации малы; оптимизированные версии позже предоставляет
// system runtime DLL.

#[no_mangle]
/// C `memset` для freestanding core.
///
/// # Safety
///
/// `destination` должен быть доступен для записи `count` байт.
pub unsafe extern "C" fn memset(destination: *mut c_void, value: i32, count: usize) -> *mut c_void {
    unsafe {
        let bytes = destination.cast::<u8>();
        for index in 0..count {
            bytes.add(index).write(value as u8);
        }
    }
    destination
}

#[no_mangle]
/// C `memcpy` для непересекающихся диапазонов.
///
/// # Safety
///
/// Оба диапазона валидны на `count` байт и не пересекаются.
pub unsafe extern "C" fn memcpy(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    unsafe {
        let destination = destination.cast::<u8>();
        let source = source.cast::<u8>();
        for index in 0..count {
            destination.add(index).write(source.add(index).read());
        }
    }
    destination
}

#[no_mangle]
/// C `memmove`, разрешающий пересечение.
///
/// # Safety
///
/// Оба диапазона валидны на `count` байт.
pub unsafe extern "C" fn memmove(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    if (destination as usize) <= (source as usize) {
        unsafe { memcpy(destination, source, count) }
    } else {
        unsafe {
            let destination = destination.cast::<u8>();
            let source = source.cast::<u8>();
            for index in (0..count).rev() {
                destination.add(index).write(source.add(index).read());
            }
        }
        destination
    }
}

#[no_mangle]
/// C `memcmp`.
///
/// # Safety
///
/// Оба диапазона доступны для чтения `count` байт.
pub unsafe extern "C" fn memcmp(left: *const c_void, right: *const c_void, count: usize) -> i32 {
    unsafe {
        let left = left.cast::<u8>();
        let right = right.cast::<u8>();
        for index in 0..count {
            let left = left.add(index).read();
            let right = right.add(index).read();
            if left != right {
                return left as i32 - right as i32;
            }
        }
    }
    0
}

//! Минимальный runtime программ RustOS.
//!
//! Здесь нет драйверов и filesystem parser'а. Runtime содержит только
//! стабильные syscall wrappers; `vfs.dll` позднее добавит удобный C ABI и
//! batching поверх тех же capability-вызовов.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;

mod arch;

pub use rustos_abi::{
    block::BlockIoRequest,
    ipc::Message,
    memory::{SharedMemoryCreate, SharedMemoryMap, VmFlags, VmMapRequest},
    process::{
        ProcessSpawnRequest, ProcessSpawnResult, ProcessStartInfo, ThreadCreateRequest,
        ThreadCreateResult,
    },
    syscall, ExitReason, Handle, Rights,
};

/// Bootstrap capability корневого VFS namespace текущего процесса.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct VfsCapability(pub Handle);

/// Добровольно отдаёт остаток текущего scheduler quantum.
pub fn thread_yield() -> i64 {
    unsafe { syscall3(syscall::number::THREAD_YIELD, 0, 0, 0) }
}

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

/// Создаёт дочерний ELF64-процесс. В отличие от PID, возвращённый handle
/// является авторизацией для `wait` и `kill` и может безопасно передаваться.
pub fn process_spawn(request: &ProcessSpawnRequest, result: &mut ProcessSpawnResult) -> i64 {
    unsafe {
        syscall3(
            syscall::number::PROCESS_SPAWN,
            request as *const ProcessSpawnRequest as u64,
            result as *mut ProcessSpawnResult as u64,
            0,
        )
    }
}

/// Блокирует текущий поток до завершения процесса.
pub fn process_wait(process: Handle, reason: &mut ExitReason) -> i64 {
    unsafe {
        syscall3(
            syscall::number::PROCESS_WAIT,
            process.0 as u64,
            reason as *mut ExitReason as u64,
            0,
        )
    }
}

/// Завершает процесс, на который у caller есть capability DESTROY.
pub fn process_kill(process: Handle, status: i32) -> i64 {
    unsafe {
        syscall3(
            syscall::number::PROCESS_KILL,
            process.0 as u64,
            status as i64 as u64,
            0,
        )
    }
}

/// Создаёт поток в текущем address space. Стек и TLS заранее отображает
/// вызывающая сторона; kernel проверяет entry, stack и thread pointer.
pub fn thread_create(request: &ThreadCreateRequest, result: &mut ThreadCreateResult) -> i64 {
    unsafe {
        syscall3(
            syscall::number::THREAD_CREATE,
            request as *const ThreadCreateRequest as u64,
            result as *mut ThreadCreateResult as u64,
            0,
        )
    }
}

/// Завершает только текущий поток.
pub fn thread_exit(status: i32) -> ! {
    unsafe {
        let _ = syscall3(syscall::number::THREAD_EXIT, status as i64 as u64, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Блокирует текущий поток до завершения целевого потока.
pub fn thread_join(thread: Handle, reason: &mut ExitReason) -> i64 {
    unsafe {
        syscall3(
            syscall::number::THREAD_JOIN,
            thread.0 as u64,
            reason as *mut ExitReason as u64,
            0,
        )
    }
}

/// Устанавливает thread pointer текущего потока: FS base на AMD64 и
/// TPIDR_EL0 на AArch64.
pub fn thread_set_tls(address: u64) -> i64 {
    unsafe { syscall3(syscall::number::THREAD_SET_TLS, address, 0, 0) }
}

/// Читает первое `u64` по текущему thread pointer. Это низкоуровневый helper
/// для проверки TLS bootstrap; обычный Rust-код обращается к TLS через
/// сгенерированную компилятором модель и не вызывает эту функцию напрямую.
///
/// # Safety
///
/// Thread pointer должен ссылаться хотя бы на восемь доступных для чтения
/// байт в address space вызывающего потока.
pub unsafe fn read_thread_pointer_u64() -> u64 {
    unsafe { arch::read_thread_pointer_u64() }
}

/// Отображает анонимные zero-filled страницы и возвращает virtual address.
pub fn vm_map(request: &VmMapRequest) -> i64 {
    unsafe {
        syscall3(
            syscall::number::VM_MAP,
            request as *const VmMapRequest as u64,
            0,
            0,
        )
    }
}

/// Удаляет отображение и освобождает private physical frames.
pub fn vm_unmap(address: u64, length: u64) -> i64 {
    unsafe { syscall3(syscall::number::VM_UNMAP, address, length, 0) }
}

/// Изменяет PTE-права диапазона с обязательной политикой W^X.
pub fn vm_protect(address: u64, length: u64, flags: VmFlags) -> i64 {
    unsafe { syscall3(syscall::number::VM_PROTECT, address, length, flags.0) }
}

/// Создаёт shared-memory object; положительный результат — capability handle.
pub fn shared_memory_create(request: &SharedMemoryCreate) -> i64 {
    unsafe {
        syscall3(
            syscall::number::SHARED_MEMORY_CREATE,
            request as *const SharedMemoryCreate as u64,
            0,
            0,
        )
    }
}

/// Отображает shared-memory capability в address space процесса.
pub fn shared_memory_map(handle: Handle, request: &SharedMemoryMap) -> i64 {
    unsafe {
        syscall3(
            syscall::number::SHARED_MEMORY_MAP,
            handle.0 as u64,
            request as *const SharedMemoryMap as u64,
            0,
        )
    }
}

/// Необратимо запечатывает shared object после заполнения. Основной сценарий
/// loader'а: создать RW, записать страницу DLL, убрать RW mapping, seal в RX
/// и передавать полученный capability другим процессам.
pub fn shared_memory_seal(handle: Handle, flags: VmFlags) -> i64 {
    unsafe {
        syscall3(
            syscall::number::SHARED_MEMORY_SEAL,
            handle.0 as u64,
            flags.0,
            0,
        )
    }
}

/// Закрывает capability. Shared object освобождается после исчезновения
/// последнего capability и последнего mapping reference.
pub fn handle_close(handle: Handle) -> i64 {
    unsafe { syscall3(syscall::number::HANDLE_CLOSE, handle.0 as u64, 0, 0) }
}

/// Возвращает монотонное время в наносекундах.
pub fn monotonic_time_ns() -> i64 {
    unsafe { syscall3(syscall::number::CLOCK_MONOTONIC, 0, 0, 0) }
}

/// Размер block capability в логических 4-KiB блоках.
pub fn block_get_size(device: Handle) -> i64 {
    unsafe { syscall3(syscall::number::BLOCK_GET_SIZE, device.0 as u64, 0, 0) }
}

/// Читает один логический блок в user buffer запроса.
pub fn block_read(device: Handle, request: &BlockIoRequest) -> i64 {
    unsafe {
        syscall3(
            syscall::number::BLOCK_READ,
            device.0 as u64,
            request as *const BlockIoRequest as u64,
            0,
        )
    }
}

/// Записывает один логический блок из user buffer запроса.
pub fn block_write(device: Handle, request: &BlockIoRequest) -> i64 {
    unsafe {
        syscall3(
            syscall::number::BLOCK_WRITE,
            device.0 as u64,
            request as *const BlockIoRequest as u64,
            0,
        )
    }
}

/// Просит устройство завершить все ранее подтверждённые записи.
pub fn block_flush(device: Handle) -> i64 {
    unsafe { syscall3(syscall::number::BLOCK_FLUSH, device.0 as u64, 0, 0) }
}

/// Проверяет read-only bootstrap block нового процесса.
///
/// # Safety
///
/// `address` должен быть первым аргументом entry point, переданным ядром.
pub unsafe fn process_start_info(address: u64) -> Option<&'static ProcessStartInfo> {
    if address == 0 || !address.is_multiple_of(core::mem::align_of::<ProcessStartInfo>() as u64) {
        return None;
    }
    let info = unsafe { &*(address as *const ProcessStartInfo) };
    (info.version == rustos_abi::process::PROCESS_ABI_VERSION
        && info.size as usize >= core::mem::size_of::<ProcessStartInfo>())
    .then_some(info)
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

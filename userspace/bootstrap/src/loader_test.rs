//! Сквозной ring-3 тест нативного resolver'а: VFS -> RUNE dependency -> call.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use rustos_abi::{
    process::{
        ProcessSpawnRequest, ProcessSpawnResult, SpawnCapability, StartupRole, PROCESS_ABI_VERSION,
    },
    syscall, ExitReason, Handle, PriorityClass, Rights,
};
use rustos_rune_format::{interface_id, symbol_id};
use rustos_rune_loader::{DynamicLoader, ModuleSource, RuntimeMemory, SearchPolicy};
use rustos_runtime::{process_exit, process_spawn, process_wait, thread_set_tls};
use rustos_vfs::VfsClient;

const NAMESPACE_SLOT: Handle = Handle(1);
const CHILD_SHARED_SLOT: u16 = 5;
const MAX_DLL_BYTES: usize = 64 * 1024;
const ROOT_PATH: &str = "/apps/loader-test/root.rune";
const FIXTURE_PATH: &str = "/system/lib/fixture-1.rune";
const CHILD_PATH: &str = "system/bin/loader-child.rune";

static mut ROOT_IMAGE: [u8; MAX_DLL_BYTES] = [0; MAX_DLL_BYTES];
static mut FIXTURE_IMAGE: [u8; MAX_DLL_BYTES] = [0; MAX_DLL_BYTES];
static mut TLS_STORAGE: [u8; 4096] = [0; 4096];

struct Images {
    root: &'static [u8],
    fixture: &'static [u8],
}

impl ModuleSource for Images {
    fn open<'a>(&'a self, path: &str) -> Option<&'a [u8]> {
        match path {
            ROOT_PATH => Some(self.root),
            FIXTURE_PATH => Some(self.fixture),
            _ => None,
        }
    }
}

#[no_mangle]
pub extern "C" fn _start(server: u64, reply: u64, abi_version: u64) -> ! {
    if abi_version != syscall::ABI_VERSION {
        process_exit(180);
    }
    let mut vfs = match VfsClient::connect(Handle(server as u32), Handle(reply as u32)) {
        Ok(client) => client,
        Err(_) => process_exit(181),
    };
    let root = unsafe { &mut *core::ptr::addr_of_mut!(ROOT_IMAGE) };
    let fixture = unsafe { &mut *core::ptr::addr_of_mut!(FIXTURE_IMAGE) };
    let root_length = read_file(&mut vfs, ROOT_PATH, root).unwrap_or_else(|_| process_exit(182));
    let fixture_length =
        read_file(&mut vfs, FIXTURE_PATH, fixture).unwrap_or_else(|_| process_exit(183));
    let images = Images {
        root: &root[..root_length],
        fixture: &fixture[..fixture_length],
    };
    let mut memory = RuntimeMemory;
    let mut loader = DynamicLoader::new(
        &images,
        SearchPolicy {
            application_dir: "/apps/loader-test",
            private_library_dir: Some("/apps/loader-test/lib"),
            system_library_dir: "/system/lib",
        },
    );
    // Prepare не получает Memory и потому физически не может частично map'ить
    // address space. Пустой capability policy допустим, потому что fixture
    // является чистой in-process DLL. Только после обеих проверок выполняем
    // единый commit.
    let resolution = loader
        .prepare(ROOT_PATH)
        .unwrap_or_else(|_| process_exit(184));
    let capability_plan = loader
        .resolve_capabilities(&[])
        .unwrap_or_else(|_| process_exit(188));
    if !capability_plan.grants().is_empty() {
        process_exit(188);
    }
    let program = loader
        .commit(resolution, &mut memory)
        .unwrap_or_else(|_| process_exit(184));
    if program.modules != 2
        || program.relro_pages == 0
        || program.shared_pages < 2
        || program.tls.size == 0
    {
        process_exit(185);
    }
    if program.tls.size != 0 {
        let storage = unsafe { &mut *core::ptr::addr_of_mut!(TLS_STORAGE) };
        let thread_pointer = loader
            .initialize_tls(storage)
            .unwrap_or_else(|_| process_exit(186));
        if thread_set_tls(thread_pointer) != syscall::status::OK {
            process_exit(186);
        }
    }
    let root_interface = interface_id("org.rustos.example.loader-root/1");
    let answer = loader
        .symbol(
            root_interface,
            symbol_id(root_interface, "linked_answer()->u64"),
            1,
            1,
        )
        .unwrap_or_else(|_| process_exit(187));
    if program.entry != answer {
        process_exit(187);
    }
    let function: extern "C" fn() -> u64 = unsafe { core::mem::transmute(program.entry as usize) };
    if function() != 42 {
        process_exit(188);
    }

    let fixture_interface = interface_id("org.rustos.example.answer/1");
    let shared = loader
        .shared_executable_region(fixture_interface)
        .unwrap_or_else(|| process_exit(189));
    let fixture_answer = loader
        .symbol(
            fixture_interface,
            symbol_id(fixture_interface, "fixture_shared_answer()->u64"),
            1,
            1,
        )
        .unwrap_or_else(|_| process_exit(189));
    let offset = fixture_answer
        .checked_sub(shared.address)
        .filter(|offset| *offset < shared.length)
        .unwrap_or_else(|| process_exit(189));
    if spawn_shared_code_child(shared.handle, offset, shared.length).is_err() {
        process_exit(189);
    }
    if vfs.shutdown_service().is_err() {
        process_exit(189);
    }
    loader.unload(&mut memory);
    process_exit(0)
}

fn read_file(client: &mut VfsClient, path: &str, output: &mut [u8]) -> Result<usize, i32> {
    use rustos_abi::vfs::{open_flags, seek_from};

    let file = client.open(path, open_flags::READ)?;
    let size = client.seek(file, 0, seek_from::END)?;
    if size == 0 || size as usize > output.len() {
        let _ = client.close(file);
        return Err(rustos_abi::vfs::status::LIMIT_REACHED);
    }
    client.seek(file, 0, seek_from::START)?;
    let read = client.read(file, &mut output[..size as usize])?;
    client.close(file)?;
    if read == size as usize {
        Ok(read)
    } else {
        Err(rustos_abi::vfs::status::IO)
    }
}

fn spawn_shared_code_child(handle: Handle, offset: u64, length: u64) -> Result<(), ()> {
    let mut arguments = [0u8; 48];
    let argument_length = encode_mapping(&mut arguments, offset, length).ok_or(())?;
    let transfer = SpawnCapability {
        source: handle,
        target_slot: CHILD_SHARED_SLOT,
        role: StartupRole::NONE,
        rights: Rights::READ.union(Rights::EXECUTE).union(Rights::MAP),
    };
    let request = ProcessSpawnRequest {
        version: PROCESS_ABI_VERSION,
        flags: 0,
        path_address: CHILD_PATH.as_ptr() as u64,
        path_length: CHILD_PATH.len() as u32,
        priority: PriorityClass::Interactive as u8,
        reserved0: [0; 3],
        arguments_address: arguments.as_ptr() as u64,
        arguments_length: argument_length as u32,
        argument_count: 1,
        environment_address: 0,
        environment_length: 0,
        environment_count: 0,
        capabilities_address: &transfer as *const SpawnCapability as u64,
        capability_count: 1,
        namespace: NAMESPACE_SLOT,
    };
    let mut child = ProcessSpawnResult {
        process: Handle::INVALID,
        reserved: 0,
        pid: rustos_abi::ProcessId::KERNEL,
    };
    if process_spawn(&request, &mut child) != syscall::status::OK {
        return Err(());
    }
    let mut reason = ExitReason {
        status: -1,
        exception: 0,
        flags: 0,
        fault_address: 0,
    };
    if process_wait(child.process, &mut reason) != syscall::status::OK
        || reason.status != 0
        || reason.exception != 0
    {
        return Err(());
    }
    Ok(())
}

fn encode_mapping(output: &mut [u8], offset: u64, length: u64) -> Option<usize> {
    let mut cursor = encode_decimal(output, 0, offset)?;
    *output.get_mut(cursor)? = b':';
    cursor += 1;
    cursor = encode_decimal(output, cursor, length)?;
    *output.get_mut(cursor)? = 0;
    Some(cursor + 1)
}

fn encode_decimal(output: &mut [u8], start: usize, mut value: u64) -> Option<usize> {
    let mut reversed = [0u8; 20];
    let mut count = 0usize;
    loop {
        reversed[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let end = start.checked_add(count)?;
    let target = output.get_mut(start..end)?;
    for (index, byte) in target.iter_mut().enumerate() {
        *byte = reversed[count - index - 1];
    }
    Some(end)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(198)
}

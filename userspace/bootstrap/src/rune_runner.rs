//! Статический user-space загрузчик RUNE-программ из VaraniaFS.
//!
//! Kernel умеет запускать только этот небольшой доверенный bootstrap из
//! initramfs. Все сложные операции — VFS, dependency graph, DLL resolution,
//! TLS и новый startup block — выполняются в изолированном ring 3 процессе.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};
use core::{alloc::GlobalAlloc, panic::PanicInfo, ptr, slice, str};

use rustos_abi::vfs::VfsObject;
use rustos_abi::{
    memory::MEMORY_ABI_VERSION,
    process::{
        ProcessStartInfo, StartupCapability, StartupRole, PROCESS_ABI_VERSION,
        PROCESS_START_INFO_ADDRESS,
    },
    syscall, Handle, VmFlags, PAGE_SIZE,
};
use rustos_rune_format::{architecture, parse_dependency, record_kind, Container, DEPENDENCY_SIZE};
use rustos_rune_loader::{DynamicLoader, ModuleSource, RuntimeMemory, SearchPolicy};
use rustos_runtime::{jump_to_image, process_exit, thread_set_tls, vm_map, VmMapRequest};
use rustos_vfs::VfsClient;

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_MODULES: usize = 8;
const TARGET_STACK_BYTES: u64 = 8 * 1024 * 1024;

struct VmBumpAllocator;

#[global_allocator]
static ALLOCATOR: VmBumpAllocator = VmBumpAllocator;

unsafe impl GlobalAlloc for VmBumpAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let alignment = layout.align().max(16);
        let requested = match layout.size().max(1).checked_add(alignment) {
            Some(size) => size,
            None => return ptr::null_mut(),
        };
        let length = (requested as u64).div_ceil(PAGE_SIZE) * PAGE_SIZE;
        let address = vm_map(&VmMapRequest {
            version: MEMORY_ABI_VERSION,
            reserved: 0,
            address: 0,
            length,
            flags: VmFlags::READ.union(VmFlags::WRITE),
        });
        if address <= 0 {
            return ptr::null_mut();
        }
        ((address as usize + alignment - 1) & !(alignment - 1)) as *mut u8
    }

    unsafe fn dealloc(&self, _pointer: *mut u8, _layout: core::alloc::Layout) {
        // Loader живёт только до безвозвратного jump в target. Все его
        // временные mappings атомарно освобождает kernel при process exit.
    }
}

struct Image {
    path: String,
    bytes: Vec<u8>,
}

struct Images {
    entries: Vec<Image>,
}

impl ModuleSource for Images {
    fn open<'a>(&'a self, path: &str) -> Option<&'a [u8]> {
        self.entries
            .iter()
            .find(|image| image.path == path)
            .map(|image| image.bytes.as_slice())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(start_address: u64, abi_version: u64, _reserved: u64) -> ! {
    let info = validate_start_info(start_address, abi_version).unwrap_or_else(|| process_exit(120));
    let arguments = argument_slices(info).unwrap_or_else(|| process_exit(121));
    if arguments.len() < 2 {
        process_exit(122);
    }
    let target_path = str::from_utf8(arguments[1])
        .ok()
        .map(String::from)
        .unwrap_or_else(|| process_exit(123));
    let server = startup_handle(info, StartupRole::VFS).unwrap_or_else(|| process_exit(124));
    let reply = startup_handle(info, StartupRole::VFS_REPLY).unwrap_or_else(|| process_exit(125));
    let mut vfs = VfsClient::connect(server, reply).unwrap_or_else(|_| process_exit(126));

    let application_dir = parent_directory(&target_path).unwrap_or_else(|| process_exit(127));
    let private_directory = format!("{application_dir}/lib");
    let images = preload_graph(&mut vfs, &target_path, &application_dir, &private_directory)
        // Не уничтожаем причинный VFS status: supervisor/serial должен
        // отличать transport fault, checksum I/O и отсутствующую DLL. Раньше
        // все эти случаи превращались в неинформативный exit 128.
        .unwrap_or_else(|error| process_exit(error));
    let mut memory = RuntimeMemory;
    let mut loader = DynamicLoader::new(
        &images,
        SearchPolicy {
            application_dir: &application_dir,
            private_library_dir: Some(&private_directory),
            system_library_dir: "/system/lib",
        },
    );
    let loaded = loader
        .load(&target_path, &mut memory)
        .unwrap_or_else(|_| process_exit(129));

    let mut tls_storage = vec![0u8; loaded.tls.storage_size as usize];
    let thread_pointer = loader
        .initialize_tls(&mut tls_storage)
        .unwrap_or_else(|_| process_exit(130));
    if loaded.tls.size != 0 && thread_set_tls(thread_pointer) != syscall::status::OK {
        process_exit(131);
    }
    let target_info = make_target_start_info(
        info,
        &arguments[1..],
        &tls_storage,
        thread_pointer,
        loaded.tls,
    )
    .unwrap_or_else(|| process_exit(132));
    let stack_top = allocate_target_stack().unwrap_or_else(|| process_exit(133));

    // С этого момента loader state намеренно не уничтожается: target вызывает
    // process_exit, после чего kernel освобождает весь address space разом.
    // SAFETY: loader проверил executable entry, vm_map создал stack,
    // а target_info лежит в зафиксированном startup mapping до process exit.
    unsafe {
        jump_to_image(
            loaded.entry,
            stack_top,
            target_info as u64,
            syscall::ABI_VERSION,
        )
    }
}

fn validate_start_info(address: u64, abi_version: u64) -> Option<&'static ProcessStartInfo> {
    if address != PROCESS_START_INFO_ADDRESS
        || abi_version != syscall::ABI_VERSION
        || !address.is_multiple_of(core::mem::align_of::<ProcessStartInfo>() as u64)
    {
        return None;
    }
    let info = unsafe { &*(address as *const ProcessStartInfo) };
    (info.version == PROCESS_ABI_VERSION
        && info.size as usize >= core::mem::size_of::<ProcessStartInfo>()
        && info.argument_count <= 256)
        .then_some(info)
}

fn argument_slices(info: &ProcessStartInfo) -> Option<Vec<&'static [u8]>> {
    let bytes = checked_startup_bytes(info.arguments_address, info.arguments_length as usize)?;
    let mut result = Vec::with_capacity(info.argument_count as usize);
    let mut cursor = 0;
    while cursor < bytes.len() {
        let tail = &bytes[cursor..];
        let length = tail.iter().position(|byte| *byte == 0)?;
        let argument = &tail[..length];
        str::from_utf8(argument).ok()?;
        result.push(argument);
        cursor = cursor.checked_add(length + 1)?;
    }
    (result.len() == info.argument_count as usize).then_some(result)
}

fn checked_startup_bytes(address: u64, length: usize) -> Option<&'static [u8]> {
    let end = address.checked_add(length as u64)?;
    if address < PROCESS_START_INFO_ADDRESS
        || end > PROCESS_START_INFO_ADDRESS.checked_add(64 * 1024)?
    {
        return None;
    }
    Some(unsafe { slice::from_raw_parts(address as *const u8, length) })
}

fn startup_handle(info: &ProcessStartInfo, role: StartupRole) -> Option<Handle> {
    if info.capability_count > 8 {
        return None;
    }
    let capabilities = unsafe {
        slice::from_raw_parts(
            info.capabilities_address as *const StartupCapability,
            info.capability_count as usize,
        )
    };
    capabilities
        .iter()
        .find(|capability| capability.role == role && capability.flags == 0)
        .map(|capability| capability.handle)
}

fn preload_graph(
    vfs: &mut VfsClient,
    root: &str,
    application_dir: &str,
    private_dir: &str,
) -> Result<Images, i32> {
    let mut entries = Vec::new();
    entries.push(Image {
        path: String::from(root),
        bytes: read_file(vfs, root)?,
    });
    let mut cursor = 0;
    while cursor < entries.len() {
        if entries.len() > MAX_MODULES {
            return Err(rustos_abi::vfs::status::LIMIT_REACHED);
        }
        let dependencies =
            dependency_names(&entries[cursor].bytes).ok_or(rustos_abi::vfs::status::IO)?;
        for name in dependencies {
            let candidates = [
                format!("{application_dir}/{name}"),
                format!("{private_dir}/{name}"),
                format!("/system/lib/{name}"),
            ];
            if candidates
                .iter()
                .any(|candidate| entries.iter().any(|image| image.path == *candidate))
            {
                continue;
            }
            let mut loaded = None;
            for candidate in candidates {
                if let Ok(bytes) = read_file(vfs, &candidate) {
                    loaded = Some(Image {
                        path: candidate,
                        bytes,
                    });
                    break;
                }
            }
            entries.push(loaded.ok_or(rustos_abi::vfs::status::NOT_FOUND)?);
        }
        cursor += 1;
    }
    Ok(Images { entries })
}

fn dependency_names(image: &[u8]) -> Option<Vec<String>> {
    let container = Container::parse(image).ok()?;
    let architecture = current_architecture();
    let mut names = Vec::new();
    for table in container.entries().filter(|entry| {
        entry.kind == record_kind::DEPENDENCIES && entry.architecture == architecture
    }) {
        let bytes = container.payload(table)?;
        if !bytes.len().is_multiple_of(DEPENDENCY_SIZE) {
            return None;
        }
        for raw in bytes.as_chunks::<DEPENDENCY_SIZE>().0 {
            let dependency = parse_dependency(raw)?;
            names.push(String::from(
                container.string(dependency.name_offset, dependency.name_length)?,
            ));
        }
    }
    Some(names)
}

fn read_file(vfs: &mut VfsClient, path: &str) -> Result<Vec<u8>, i32> {
    use rustos_abi::vfs::{open_flags, seek_from};

    let file: VfsObject = vfs.open(path, open_flags::READ)?;
    let size = vfs.seek(file, 0, seek_from::END)? as usize;
    if size == 0 || size > MAX_IMAGE_BYTES {
        let _ = vfs.close(file);
        return Err(rustos_abi::vfs::status::LIMIT_REACHED);
    }
    vfs.seek(file, 0, seek_from::START)?;
    let mut bytes = vec![0u8; size];
    let mut offset = 0;
    while offset < bytes.len() {
        let read = vfs.read(file, &mut bytes[offset..])?;
        if read == 0 {
            let _ = vfs.close(file);
            return Err(rustos_abi::vfs::status::IO);
        }
        offset += read;
    }
    vfs.close(file)?;
    Ok(bytes)
}

fn make_target_start_info(
    original: &ProcessStartInfo,
    arguments: &[&[u8]],
    tls_storage: &[u8],
    thread_pointer: u64,
    tls: rustos_rune_loader::TlsLayout,
) -> Option<*const ProcessStartInfo> {
    let argument_length = arguments.iter().try_fold(0usize, |total, argument| {
        total.checked_add(argument.len() + 1)
    })?;
    let environment = checked_startup_bytes(
        original.environment_address,
        original.environment_length as usize,
    )?;
    let capability_bytes =
        original.capability_count as usize * core::mem::size_of::<StartupCapability>();
    let header_size = core::mem::size_of::<ProcessStartInfo>();
    let capability_offset = align_up_usize(
        header_size
            .checked_add(argument_length)?
            .checked_add(environment.len())?,
        core::mem::align_of::<StartupCapability>(),
    )?;
    let total = capability_offset.checked_add(capability_bytes)?;
    if total > 64 * 1024 {
        return None;
    }
    let mut block = vec![0u8; total];
    let base = block.as_mut_ptr() as u64;
    let arguments_address = base + header_size as u64;
    let mut cursor = header_size;
    for argument in arguments {
        block[cursor..cursor + argument.len()].copy_from_slice(argument);
        cursor += argument.len() + 1;
    }
    let environment_address = base + cursor as u64;
    block[cursor..cursor + environment.len()].copy_from_slice(environment);
    let capabilities_address = base + capability_offset as u64;
    if capability_bytes != 0 {
        let capabilities = checked_startup_bytes(original.capabilities_address, capability_bytes)?;
        block[capability_offset..capability_offset + capability_bytes]
            .copy_from_slice(capabilities);
    }
    let tls_template_address = if tls.size == 0 {
        0
    } else {
        thread_pointer.checked_sub(tls.size)?
    };
    if tls.size != 0 {
        let storage_start = tls_storage.as_ptr() as u64;
        let storage_end = storage_start.checked_add(tls_storage.len() as u64)?;
        if tls_template_address < storage_start
            || tls_template_address.checked_add(tls.size)? > storage_end
        {
            return None;
        }
    }
    let info = ProcessStartInfo {
        version: PROCESS_ABI_VERSION,
        size: header_size as u32,
        pid: original.pid,
        tid: original.tid,
        page_size: original.page_size,
        monotonic_hz: original.monotonic_hz,
        arguments_address,
        arguments_length: argument_length as u32,
        argument_count: arguments.len() as u32,
        environment_address,
        environment_length: environment.len() as u32,
        environment_count: original.environment_count,
        capabilities_address,
        capability_count: original.capability_count,
        reserved: 0,
        tls_template_address,
        tls_file_size: tls.size,
        tls_memory_size: tls.size,
        tls_alignment: tls.alignment as u32,
        tls_variant: if tls.size == 0 { 0 } else { 2 },
        tls_reserved: 0,
    };
    unsafe { (block.as_mut_ptr() as *mut ProcessStartInfo).write(info) };
    let pointer = block.as_ptr() as *const ProcessStartInfo;
    core::mem::forget(block);
    Some(pointer)
}

fn allocate_target_stack() -> Option<u64> {
    let address = vm_map(&VmMapRequest {
        version: MEMORY_ABI_VERSION,
        reserved: 0,
        address: 0,
        length: TARGET_STACK_BYTES,
        flags: VmFlags::READ.union(VmFlags::WRITE),
    });
    (address > 0).then_some(address as u64 + TARGET_STACK_BYTES - 8)
}

fn parent_directory(path: &str) -> Option<String> {
    let index = path.rfind('/')?;
    Some(if index == 0 {
        String::from("/")
    } else {
        String::from(&path[..index])
    })
}

const fn current_architecture() -> u16 {
    #[cfg(target_arch = "x86_64")]
    {
        architecture::X86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        architecture::AARCH64
    }
}

fn align_up_usize(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[alloc_error_handler]
fn allocation_error(_layout: core::alloc::Layout) -> ! {
    process_exit(135)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(136)
}

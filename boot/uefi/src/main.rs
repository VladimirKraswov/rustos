//! # UEFI-загрузчик RustOS
//!
//! Последовательность операций (общий контекст — docs/ARCHITECTURE.md,
//! раздел «Загрузка»):
//!
//! 1. Логгер: crate `log` → UEFI ConOut (feature `logger`),
//!    `#[panic_handler]` предоставляет feature `panic_handler`.
//! 2. Memory map **до** exit — для выбора резерва ядра и перевода
//!    виртуального адреса GOP-framebuffer'а.
//! 3. Резерв ядра: ET_DYN (PIE) — верхний conventional-регион ниже 4 GiB;
//!    ET_EXEC (статическое ядро) — ровно с link base (минимальный vaddr
//!    сегментов; на AArch64 — 0x40000000, начало RAM QEMU `virt`).
//!    Раскладка блока:
//!    `[kernel ELF][initramfs][page tables (16 MiB)][BootInfo][scratch (1 MiB)][boot stack (512 KiB)]`.
//! 4. Загрузка ядра (ET_DYN: `R_*_RELATIVE`-релокации; ET_EXEC — сегменты
//!    по физическим vaddr, релокаций нет) и initramfs.
//! 5. ACPI RSDP: EFI config table (канонический способ), fallback — legacy scan.
//! 6. Identity page tables (PGD) — ядро стартует в identity-маппинге.
//! 7. Копия BootInfo **до** exit (память резерва переживает `ExitBootServices`).
//! 8. `ExitBootServices` → нормализованная карта памяти → `BootInfo.memmap`.
//! 9. Sanity-проверка: буфер финальной карты не должен пересекать резерв
//!    (иначе `debug_exit(0xE1)` — QEMU isa-debug-exit).
//! 10. `CR3 = PGD`, `RSP = top(boot stack) − 8` (эмуляция обычного `call`:
//!     по ABI в точке входа функции RSP ≡ 8 (mod 16)), `RDI = *const BootInfo`,
//!     `jmp _start`. Прерывания выключены (`cli`) — у ядра ещё нет IDT.
//!     Загрузчик не возвращается.
//!
//! ## Контракт ядра
//!
//! Ядро — ELF64 (PIE или статическое, см. `targets/*.json`), точка входа
//! `_start(boot_info: *const rustos_abi::BootInfo)` (x86 — в `RDI`,
//! AArch64 — в `X0`). Все адреса в BootInfo — физические.

#![no_main]
#![no_std]

mod arch;
mod bootinfo;
mod debug;
mod elf;
mod pagetable;

use core::time::Duration;

use rustos_abi::bootinfo::{
    BootFirmware, BootFramebuffer, BootInitramfs, BootStack, BOOT_FIRMWARE_NONE,
    FRAMEBUFFER_FORMAT_BGR, FRAMEBUFFER_FORMAT_RGB, KERNEL_STACK_SIZE, PAGE_TABLE_BUDGET,
};
use rustos_abi::{BootInfo, MemRegion, BOOT_INFO_MAGIC, BOOT_INFO_VERSION, MEMMAP_MAX_REGIONS};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::{boot::AllocateType, prelude::*};

/// Образ ядра, встроенный на этапе сборки (собирается до загрузчика).
/// Путь относительный от `boot/uefi/src/main.rs` → `boot/uefi/payload/`.
// Clippy анализирует загрузчик отдельно от полного build pipeline, поэтому в
// этом режиме ему не нужны сгенерированные бинарные payload-файлы. Обычная
// сборка по-прежнему жёстко требует реальный ELF и тем самым не сможет
// случайно выпустить пустой загрузочный образ.
#[cfg(not(clippy))]
static KERNEL_ELF: &[u8] = include_bytes!("../payload/kernel.elf");
#[cfg(clippy)]
static KERNEL_ELF: &[u8] = &[];
/// initramfs (RIFS v1, `tools/pack`), встроенный на этапе сборки.
#[cfg(not(clippy))]
static INITRAMFS: &[u8] = include_bytes!("../payload/initramfs.img");
#[cfg(clippy)]
static INITRAMFS: &[u8] = &[];

/// Минимальный адрес резерва ядра (не трогать низ памяти: SMBIOS/EBDA и т.п.).
const RESERVATION_MIN_ADDR: u64 = 16 * 1024 * 1024;
/// Максимальный адрес резерва: ниже 4 GiB — первое кольцо identity-маппинга.
const RESERVATION_MAX_ADDR: u64 = 4 * 1024 * 1024 * 1024;
/// Выравнивание начала резерва.
const RESERVATION_ALIGN: u64 = 64 * 1024;
/// Вспомогательная область после BootInfo (ранние нужды ядра;
/// в текущем срезе не используется, но место под неё зарезервировано).
const SCRATCH_SIZE: u64 = 1024 * 1024;

/// Ошибка загрузки (fatal: сообщение в ConOut + power-off).
#[derive(Debug)]
enum BootError {
    /// Не удалось получить UEFI memory map.
    MemoryMap(uefi::Error),
    /// Нет свободного conventional-региона ниже 4 GiB.
    NoKernelRegion,
    /// UEFI не смогла закрепить выбранные страницы за загрузчиком.
    ReservePages(uefi::Error),
    /// AllocateType::Address неожиданно вернул другой адрес.
    WrongReservationAddress,
    /// Статическое ядро линковано вне допустимого диапазона резерва
    /// (ниже 16 MiB или блок не помещается ниже 4 GiB).
    WrongLinkBase,
    /// Ошибка ELF-образа ядра.
    Elf(elf::ElfError),
    /// Ошибка построения page tables.
    PageTables(pagetable::PtError),
}

impl core::fmt::Display for BootError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BootError::MemoryMap(s) => write!(f, "failed to get UEFI memory map: {s}"),
            BootError::NoKernelRegion => write!(
                f,
                "no conventional memory region in [{RESERVATION_MIN_ADDR:#x}, {RESERVATION_MAX_ADDR:#x}) for kernel reservation"
            ),
            BootError::ReservePages(e) => write!(f, "failed to reserve kernel pages: {e}"),
            BootError::WrongReservationAddress => {
                write!(f, "UEFI returned an unexpected kernel reservation address")
            }
            BootError::WrongLinkBase => write!(
                f,
                "static kernel link base outside [{RESERVATION_MIN_ADDR:#x}, {RESERVATION_MAX_ADDR:#x})"
            ),
            BootError::Elf(e) => write!(f, "kernel ELF error: {e}"),
            BootError::PageTables(e) => write!(f, "page tables error: {e}"),
        }
    }
}

#[entry]
fn main() -> Status {
    // Liveness: до этого момента не работает даже println/serial —
    // только ConOut через `log` crate.
    uefi::helpers::init().expect("failed to initialize UEFI logger");
    log::info!(
        "RustOS UEFI bootloader v{} (uefi crate 0.36.1)",
        env!("CARGO_PKG_VERSION")
    );

    let result = unsafe { boot() };
    match result {
        // Unreachable: при успехе boot() передаёт управление ядру.
        Ok(()) => Status::SUCCESS,
        Err(e) => {
            log::error!("FATAL: {e}");
            // Даём время прочитать сообщение (в CI баннер виден в логе QEMU).
            uefi::boot::stall(Duration::from_secs(2));
            // Выключаем ВМ (в CI отсутствие баннера ядра = упавший тест).
            uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::LOAD_ERROR, None);
        }
    }
}

/// Раскладка блока резерва (смещения от начала блока).
///
/// Поля `scratch`/`stack` не читаются в текущем срезе (вершина стека =
/// `base + size`) — они существуют, чтобы документировать раскладку блока,
/// поэтому `allow(dead_code)`.
#[allow(dead_code)]
struct Layout {
    /// Общий размер блока, выровненный вверх до 64 KiB.
    size: u64,
    /// Начало образа ядра (ELF в памяти, после релокаций).
    kernel: u64,
    /// Начало initramfs.
    initramfs: u64,
    /// Бюджет под page tables.
    page_tables: u64,
    /// Копия BootInfo.
    bootinfo: u64,
    /// Вспомогательная область.
    scratch: u64,
    /// Boot-стек (заканчивает блок; верх = начало + size).
    stack: u64,
}

impl Layout {
    /// Расчёт раскладки по размеру образа ядра в памяти.
    fn new(kernel_mem_size: u64) -> Self {
        let kernel_end = align_up(kernel_mem_size, 4096);
        let initramfs_end = align_up(INITRAMFS.len() as u64, 4096);
        let bootinfo_end = align_up(core::mem::size_of::<BootInfo>() as u64, 4096);
        let size = align_up(
            kernel_end
                + initramfs_end
                + PAGE_TABLE_BUDGET
                + bootinfo_end
                + SCRATCH_SIZE
                + KERNEL_STACK_SIZE,
            RESERVATION_ALIGN,
        );
        Self {
            size,
            kernel: 0,
            initramfs: kernel_end,
            page_tables: kernel_end + initramfs_end,
            bootinfo: kernel_end + initramfs_end + PAGE_TABLE_BUDGET,
            scratch: kernel_end + initramfs_end + PAGE_TABLE_BUDGET + bootinfo_end,
            stack: size - KERNEL_STACK_SIZE,
        }
    }
}

/// Собственно загрузка ядра.
///
/// # Safety
///
/// Функция работает с необработанными указателями на identity-память UEFI;
/// все целевые области вычисляются из memory map и лежат в зарезервированном
/// блоке (см. [`find_reservation`]).
unsafe fn boot() -> Result<(), BootError> {
    // 0. UART (COM1) — диагностический канал. После `ExitBootServices`
    //    ConOut недоступен, поэтому критические контрольные точки
    //    (post-exit) дублируются сюда.
    debug::init();
    debug::put_str("[dbg] boot() entered\n");

    // 1. Memory map до exit.
    //    LOADER_DATA: `AllocatePool` не принимает EfiConventionalMemory
    //    (этот тип валиден только для `AllocatePages`) — OVMF отвечает
    //    INVALID_PARAMETER. Тип буфера влияет только на его запись в
    //    финальной карте (LOADER_DATA → kernel Reserved).
    //    ВАЖНО: все чтения pre_map (в т.ч. virt_to_phys) выполняются ДО
    //    копирования ядра в резерв — буфер самой карты (LOADER_DATA)
    //    может оказаться внутри выбранного резерва.
    let selection_map =
        uefi::boot::memory_map(MemoryType::LOADER_DATA).map_err(BootError::MemoryMap)?;

    // 2. Резерв ядра. Найденную область обязательно регистрируем в UEFI:
    //    иначе firmware вправе повторно выдать страницы, уже занятые ядром
    //    или page tables.
    //    ET_DYN (PIE) — верхний conventional-регион ниже 4 GiB;
    //    ET_EXEC (статическое ядро) — блок начинается ровно с link base:
    //    сегменты копируются по физическим vaddr, другой адрес означал бы
    //    загрузку в чужую память или fault при первом обращении.
    let kernel_mem_size = elf::image_size(KERNEL_ELF).map_err(BootError::Elf)?;
    let (is_static, link_base) = elf::load_base(KERNEL_ELF).map_err(BootError::Elf)?;
    let layout = Layout::new(kernel_mem_size);
    let base = if is_static {
        if link_base < RESERVATION_MIN_ADDR || link_base + layout.size > RESERVATION_MAX_ADDR {
            return Err(BootError::WrongLinkBase);
        }
        link_base
    } else {
        find_reservation(&selection_map, layout.size)?
    };
    drop(selection_map);
    let reserved = uefi::boot::allocate_pages(
        AllocateType::Address(base),
        MemoryType::LOADER_DATA,
        layout.size.div_ceil(4096) as usize,
    )
    .map_err(BootError::ReservePages)?;
    if reserved.as_ptr() as u64 != base {
        return Err(BootError::WrongReservationAddress);
    }

    // AllocatePages изменил карту, поэтому дальше используем только новый
    // снимок, включая перевод адреса GOP framebuffer.
    let pre_map = uefi::boot::memory_map(MemoryType::LOADER_DATA).map_err(BootError::MemoryMap)?;
    let max_descriptor_end = pre_map
        .entries()
        .map(|d| d.phys_start + d.page_count * 4096)
        .max()
        .unwrap_or(0);
    log::info!(
        "memory map (pre-exit): {} regions, max descriptor end = {:#x}",
        pre_map.len(),
        max_descriptor_end
    );
    log::info!(
        "kernel reservation: base={:#x} size={} KiB",
        base,
        layout.size / 1024
    );
    debug_assert!(base >= RESERVATION_MIN_ADDR && base + layout.size <= RESERVATION_MAX_ADDR);

    // 3. Firmware (ACPI RSDP / DTB) и GOP — до копирования ядра.
    let (firmware_root, firmware_size) = arch::find_firmware();
    log::info!("firmware: root={firmware_root:#x} size={firmware_size:#x}");
    let framebuffer = get_gop_framebuffer(&pre_map);

    // 4. Ядро (ELF64 PIE) и initramfs.
    let kernel_phys = base + layout.kernel;
    let entry = elf::load(KERNEL_ELF, kernel_phys).map_err(BootError::Elf)?;
    log::info!(
        "kernel loaded: phys={:#x} entry={:#x} size={:#x}",
        kernel_phys,
        entry,
        kernel_mem_size
    );

    let initramfs_phys = base + layout.initramfs;
    // SAFETY: initramfs_phys..+len — в пределах резерва (раскладка Layout).
    unsafe {
        core::ptr::copy_nonoverlapping(
            INITRAMFS.as_ptr(),
            initramfs_phys as *mut u8,
            INITRAMFS.len(),
        );
    }
    log::info!(
        "initramfs loaded: phys={:#x} size={} bytes",
        initramfs_phys,
        INITRAMFS.len()
    );

    // 5. Identity page tables (в бюджетной области резерва).
    let page_root = pagetable::build_identity_map(
        base + layout.page_tables,
        PAGE_TABLE_BUDGET,
        &pre_map,
        base,
        layout.size,
        firmware_root,
        firmware_size,
        &framebuffer,
    )
    .map_err(BootError::PageTables)?;
    log::info!("identity page tables ready: root={page_root:#x}");

    // 6. BootInfo — копируем в резерв ДО exit (memmap заполним после).
    let console = arch::boot_console();
    let bootinfo_phys = base + layout.bootinfo;
    let info = BootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        _pad: 0,
        memmap_count: 0,
        _pad2: 0,
        memmap: [MemRegion::ZERO; MEMMAP_MAX_REGIONS],
        framebuffer,
        console,
        firmware: BootFirmware {
            kind: if firmware_root == 0 {
                BOOT_FIRMWARE_NONE
            } else {
                arch::FIRMWARE_KIND
            },
            _reserved: 0,
            root: firmware_root,
        },
        initramfs: BootInitramfs {
            phys_addr: initramfs_phys,
            size: INITRAMFS.len() as u64,
        },
        kernel_phys,
        kernel_size: kernel_mem_size,
        boot_stack: BootStack {
            top: base + layout.size,
            size: KERNEL_STACK_SIZE,
        },
    };
    // SAFETY: bootinfo_phys..+size — в пределах резерва.
    unsafe {
        core::ptr::copy_nonoverlapping(
            &info as *const BootInfo as *const u8,
            bootinfo_phys as *mut u8,
            core::mem::size_of::<BootInfo>(),
        );
    }
    debug::put_str("[dbg] bootinfo copied; calling exit_boot_services\n");

    // 7. ExitBootServices.
    let final_map = uefi::boot::exit_boot_services(None);
    let reservation_is_reserved = final_map.entries().any(|descriptor| {
        let start = descriptor.phys_start;
        let end = start + descriptor.page_count * 4096;
        start <= base && end >= base + layout.size && descriptor.ty != MemoryType::CONVENTIONAL
    });
    if !reservation_is_reserved {
        debug::put_str("[dbg] FATAL: kernel reservation is still usable\n");
        arch::debug_exit(0xE2);
    }
    let (regions, count) = bootinfo::normalize_map(&final_map);
    // SAFETY: bootinfo_phys — копия BootInfo в резерве (записана выше).
    unsafe {
        bootinfo::write_memmap(bootinfo_phys as *mut BootInfo, &regions, count);
    }
    debug::put_str("[dbg] exit ok; memmap written\n");

    // 8. Sanity: буфер финальной карты не должен пересекать резерв ядра.
    let buf_virt = final_map.buffer().as_ptr() as u64;
    let buf_phys = arch::virt_to_phys(buf_virt, &final_map);
    let buf_end = buf_phys + final_map.buffer().len() as u64;
    if buf_phys < base + layout.size && buf_end > base {
        log::error!(
            "final memory map buffer [{buf_phys:#x}, {buf_end:#x}) overlaps kernel reservation [{base:#x}, {:#x}) — aborting",
            base + layout.size
        );
        arch::debug_exit(0xE1);
    }

    // 9. Передача управления ядру.
    debug::put_str("[dbg] sanity ok; jumping to kernel\n");
    let stack_top = base + layout.size;
    #[cfg(target_arch = "x86_64")]
    {
        // RSP = верх стека − 8: эмуляция `call` (RSP ≡ 8 mod 16 по SysV ABI).
        let kernel_rsp = stack_top - 8;
        // SAFETY: page_root/stack_top/boot_info — identity-адреса из раскладки.
        unsafe { arch::jump_to_kernel(page_root, kernel_rsp, bootinfo_phys, entry) }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SP_EL1 = верх стека (AAPCS64: SP%16==0 на входе, без −8).
        // Parking-векторы в scratch-области (1-KiB-выровнена).
        let vectors = base + layout.scratch;
        // SAFETY: vectors — в пределах резерва, 1-KiB-выровнена.
        unsafe { pagetable::fill_parking_vectors(vectors) };
        // SAFETY: все аргументы — валидные identity-адреса из раскладки.
        unsafe { arch::jump_to_kernel(page_root, stack_top, bootinfo_phys, entry, vectors) }
    }
}

/// Выбор верхнего выровненного адреса в conventional-регионе ниже 4 GiB.
/// Само закрепление выполняется через UEFI AllocatePages сразу после поиска.
fn find_reservation(map: &MemoryMapOwned, required: u64) -> Result<u64, BootError> {
    let mut best: Option<u64> = None;
    for d in map.entries() {
        if d.ty != MemoryType::CONVENTIONAL {
            continue;
        }
        let end = d.phys_start + d.page_count * 4096;
        if end <= RESERVATION_MIN_ADDR || d.phys_start >= RESERVATION_MAX_ADDR {
            continue;
        }
        let usable_start = d.phys_start.max(RESERVATION_MIN_ADDR);
        let usable_end = end.min(RESERVATION_MAX_ADDR);
        if usable_end < usable_start || usable_end - usable_start < required {
            continue;
        }
        let start = align_down(usable_end - required, RESERVATION_ALIGN);
        if start < usable_start || start + required > usable_end {
            continue;
        }
        best = Some(match best {
            Some(b) if b >= start => b,
            _ => start,
        });
    }
    best.ok_or(BootError::NoKernelRegion)
}

/// GOP-framebuffer: параметры активного режима + физический адрес.
///
/// Читаем до exit, пока `pre_map` валиден для virt→phys.
/// x86: OVMF маппит framebuffer 1:1. AArch64: через UEFI карту.
fn get_gop_framebuffer(pre_map: &MemoryMapOwned) -> BootFramebuffer {
    let gop_handle = match uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        Ok(h) => h,
        Err(e) => {
            log::info!("GOP not found ({e:?}); serial-only boot");
            return BootFramebuffer::ZERO;
        }
    };
    let mut gop = match uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle) {
        Ok(g) => g,
        Err(e) => {
            log::info!("GOP open failed ({e:?}); serial-only boot");
            return BootFramebuffer::ZERO;
        }
    };
    let mode = gop.current_mode_info();
    let format = match mode.pixel_format() {
        PixelFormat::Rgb => FRAMEBUFFER_FORMAT_RGB,
        PixelFormat::Bgr => FRAMEBUFFER_FORMAT_BGR,
        PixelFormat::Bitmask | PixelFormat::BltOnly => {
            log::info!("GOP pixel format is not directly supported; serial-only boot");
            return BootFramebuffer::ZERO;
        }
    };
    let (width, height) = mode.resolution();
    let stride = mode.stride();
    let fb_virt = gop.frame_buffer().as_mut_ptr() as u64;
    let fb_phys = arch::virt_to_phys(fb_virt, pre_map);
    log::info!("GOP framebuffer: virt={fb_virt:#x} phys={fb_phys:#x}");
    BootFramebuffer {
        phys_addr: fb_phys,
        width: width as u32,
        height: height as u32,
        // UEFI GOP сообщает stride в пикселях, ABI RustOS — в байтах.
        stride: (stride * 4) as u32,
        bpp: 32,
        format,
        _reserved: 0,
    }
}

/// Выравнивание вверх.
#[inline]
fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

/// Выравнивание вниз.
#[inline]
fn align_down(v: u64, align: u64) -> u64 {
    v & !(align - 1)
}

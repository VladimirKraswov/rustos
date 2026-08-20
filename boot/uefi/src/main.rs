//! # UEFI-загрузчик RustOS
//!
//! Последовательность операций (общий контекст — docs/ARCHITECTURE.md,
//! раздел «Загрузка»):
//!
//! 1. Логгер: crate `log` → UEFI ConOut (feature `logger`),
//!    `#[panic_handler]` предоставляет feature `panic_handler`.
//! 2. Memory map **до** exit — для выбора резерва ядра и перевода
//!    виртуального адреса GOP-framebuffer'а.
//! 3. Резерв ядра: верхний conventional-регион ниже 4 GiB.
//!    Раскладка блока:
//!    `[kernel ELF][initramfs][page tables (16 MiB)][BootInfo][scratch (1 MiB)][boot stack (512 KiB)]`.
//! 4. Загрузка ядра (ELF64 PIE, `R_X86_64_RELATIVE`) и initramfs.
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
//! Ядро — ELF64 PIE (см. `targets/x86_64-unknown-rustos.json`), точка входа
//! `_start(boot_info: *const rustos_abi::BootInfo)` читает `RDI`.
//! Все адреса в BootInfo — физические.

#![no_main]
#![no_std]

mod arch;
mod bootinfo;
mod debug;
mod elf;
mod pagetable;

use core::time::Duration;

use rustos_abi::bootinfo::{
    BootConsole, BootFirmware, BootFramebuffer, BootInitramfs, BootStack, BOOT_CONSOLE_16550_PORT,
    BOOT_FIRMWARE_ACPI, BOOT_FIRMWARE_NONE, FRAMEBUFFER_FORMAT_BGR, FRAMEBUFFER_FORMAT_RGB,
    KERNEL_STACK_SIZE, PAGE_TABLE_BUDGET,
};
use rustos_abi::{BootInfo, MemRegion, BOOT_INFO_MAGIC, BOOT_INFO_VERSION, MEMMAP_MAX_REGIONS};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::table::cfg::ConfigTableEntry;
use uefi::{boot::AllocateType, prelude::*, system};

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

    // 2. Резерв ядра (верх conventional-памяти ниже 4 GiB). Найденную
    //    область обязательно регистрируем в UEFI: иначе firmware вправе
    //    повторно выдать страницы, уже занятые ядром или page tables.
    let kernel_mem_size = elf::image_size(KERNEL_ELF).map_err(BootError::Elf)?;
    let layout = Layout::new(kernel_mem_size);
    let base = find_reservation(&selection_map, layout.size)?;
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

    // 3. RSDP и GOP — до копирования ядра (используют pre_map и UEFI-протоколы).
    let rsdp = find_rsdp();
    log::info!("ACPI RSDP: {rsdp:#x}");
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

    // 5. Identity page tables (PGD в бюджетной области резерва).
    let pgd = pagetable::build_identity_map(
        base + layout.page_tables,
        PAGE_TABLE_BUDGET,
        &pre_map,
        base,
        layout.size,
        rsdp,
        &framebuffer,
    )
    .map_err(BootError::PageTables)?;
    log::info!("identity page tables ready: PGD={:#x}", pgd);

    // 6. BootInfo — копируем в резерв ДО exit (memmap заполним после).
    let bootinfo_phys = base + layout.bootinfo;
    let info = BootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        _pad: 0,
        memmap_count: 0,
        _pad2: 0,
        memmap: [MemRegion::ZERO; MEMMAP_MAX_REGIONS],
        framebuffer,
        console: BootConsole {
            kind: BOOT_CONSOLE_16550_PORT,
            flags: 0,
            base: 0x3f8,
            clock_hz: 1_843_200,
            baud: 115_200,
        },
        firmware: BootFirmware {
            kind: if rsdp == 0 {
                BOOT_FIRMWARE_NONE
            } else {
                BOOT_FIRMWARE_ACPI
            },
            _reserved: 0,
            root: rsdp,
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

    // 7. ExitBootServices: после этого boot-протоколы (ConOut/GOP) свободны —
    //    дальше только serial (ядро) и isa-debug-exit (диагностика).
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

    // 8. Sanity: буфер финальной карты (LOADER_DATA) не должен пересекать
    //    резерв ядра. Теоретически возможно: UEFI pool-выделитель не знает
    //    о нашем резерве. На практике верх резерва исключает коллизию.
    // До SetVirtualAddressMap x86-64 firmware работает в identity mapping:
    // обычный pointer буфера уже равен физическому адресу. `virt_start` в
    // EFI descriptors на этом этапе равен нулю и не подходит для перевода.
    let buf_phys = final_map.buffer().as_ptr() as u64;
    let buf_end = buf_phys + final_map.buffer().len() as u64;
    if buf_phys < base + layout.size && buf_end > base {
        log::error!(
            "final memory map buffer [{buf_phys:#x}, {buf_end:#x}) overlaps kernel reservation [{base:#x}, {:#x}) — aborting",
            base + layout.size
        );
        arch::debug_exit(0xE1);
    }

    // 9. Передача управления ядру.
    //    Контракт: RDI = *const BootInfo, RSP = верх boot-стека, CR3 = PGD.
    //    cli: у ядра ещё нет IDT (этап 3). jmp: загрузчик не возвращается.
    debug::put_str("[dbg] sanity ok; jumping to kernel\n");
    // RSP = верх стека МИНУС 8: эмуляция обычного `call` — по ABI в точке
    // входа функции (после push return-address) RSP ≡ 8 (mod 16). Ядро
    // компилируется под стандартное состояние: кадры строятся от него, и
    // `movaps`/`sub $0x28`-прологи предполагают RSP ≡ 8 на входе. `_start`
    // — `-> !` (никакого `ret`), поэтому «ложный return-address» в верхних
    // 8 байтах не читается.
    let stack_top = base + layout.size;
    let kernel_rsp = stack_top - 8;
    // Intel-синтаксис (default rustc, подтверждено сборкой ядра): `mov dst, src`.
    // `mov cr3, {pgd}` — ЗАПИСЬ pgd в CR3 (AT&T-оригинал `%cr3, {pgd}` читал CR3).
    //
    // `jmp {entry}` — косвенный переход ЧЕРЕЗ РЕГИСТР (FF E0).
    // Формы `jmp [entry]` / `jmp qword ptr [entry]` = переход ЧЕРЕЗ ПАМЯТЬ
    // (FF 20): CPU прочитал бы 8 байт КОДА по адресу entry и прыгнул бы по
    // значению этих байт (полевая проверка: #GP с RIP = первые 8 байт .text).
    //
    // Регистры: asm реально модифицирует только RDI (GPR) и спец-регистры
    // CR3/RSP; прочие GPR не трогаем → не объявляем clobber (иначе 4 входа +
    // 12 clobber = 16 > 15 доступных GPR → «requires more registers than
    // available»). RDI объявлен явным output.
    // SAFETY: pgd/stack_top/boot_info — валидные identity-адреса из раскладки;
    // память не используется (nomem), push/pop нет (nostack — только setup
    // стека ядра), `jmp` не возвращается (unreachable_unchecked ниже).
    unsafe { arch::jump_to_kernel(pgd, kernel_rsp, bootinfo_phys, entry) }
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

/// ACPI RSDP: EFI config table (ACPI 2.0 GUID, затем ACPI 1.0),
/// fallback — legacy scan `0xE0000..0x100000` (шаг 16 байт) и EBDA.
///
/// Адрес в EFI config table — физический (до `SetVirtualAddressMap`
/// все таблицы в UEFI — по физическим адресам).
fn find_rsdp() -> u64 {
    let mut rsdp: u64 = 0;
    system::with_config_table(|entries: &[ConfigTableEntry]| {
        if rsdp == 0 {
            if let Some(e) = entries
                .iter()
                .find(|e| e.guid == ConfigTableEntry::ACPI2_GUID)
            {
                rsdp = e.address as u64;
            }
        }
        if rsdp == 0 {
            if let Some(e) = entries
                .iter()
                .find(|e| e.guid == ConfigTableEntry::ACPI_GUID)
            {
                rsdp = e.address as u64;
            }
        }
    });
    if rsdp != 0 {
        return rsdp;
    }
    // Legacy fallback (OVMF всегда отдаёт config table, но для надёжности).
    let sig = b"RSD PTR ";
    let mut addr = 0xE0000u64;
    while addr < 0x100_0000 {
        // SAFETY: identity-маппинг UEFI, регион 0xE0000..0x1000000 — ROM/RAM.
        if unsafe { has_signature(addr, sig) } {
            return addr;
        }
        addr += 16;
    }
    let ebda = unsafe { (0x40E as *const u16).read_volatile() };
    if (ebda as u32) > 0x40E {
        let base = (ebda as u64) & 0xFFFF_F000;
        let mut a = base;
        while a < base + 1024 {
            // SAFETY: EBDA-регион — валидная низкая память (identity-маппинг).
            if unsafe { has_signature(a, sig) } {
                return a;
            }
            a += 16;
        }
    }
    0
}

/// Проверка сигнатуры по физическому адресу (identity-маппинг UEFI).
unsafe fn has_signature(addr: u64, sig: &[u8]) -> bool {
    let p = addr as *const u8;
    for (i, &b) in sig.iter().enumerate() {
        if p.add(i).read_volatile() != b {
            return false;
        }
    }
    true
}

/// GOP-framebuffer: параметры активного режима + физический адрес.
///
/// Читаем до exit, пока `pre_map` валиден для virt→phys.
/// ОVMF на QEMU маппит framebuffer 1:1, но общий случай — через карту.
fn get_gop_framebuffer(_pre_map: &MemoryMapOwned) -> BootFramebuffer {
    // GOP живёт не на handle нашего image, а на handle устройства
    // платформы — ищем его через handle-базу (open по image давал
    // UNSUPPORTED).
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
    let fb_phys = gop.frame_buffer().as_mut_ptr() as u64;
    log::info!("GOP framebuffer identity address: {fb_phys:#x}");
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

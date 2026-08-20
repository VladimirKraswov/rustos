//! GRUB/Multiboot2 bootstrap для AMD64.
//!
//! GRUB входит в ядро в 32-битном protected mode. Короткая ассемблерная
//! прелюдия отображает первые 4 GiB страницами по 2 MiB, включает long mode
//! и передаёт управление безопасному Rust-разборщику тегов. Разборщик
//! нормализует memory map, framebuffer, ACPI и initramfs в общий [`BootInfo`].
//!
//! После этого строится уже полный identity map. На современных CPU с 1-GiB
//! pages он покрывает до 128 TiB физического адресного пространства. Компактный
//! 2-MiB pool не раздувает загрузочный ELF при минимальных 128 MiB RAM; fallback
//! для старых CPU использует страницы 2 MiB. Все диапазоны ядра, Multiboot info
//! и modules вырезаются из usable RAM до запуска frame allocator'а.

use core::{arch::global_asm, ptr};

use rustos_abi::bootinfo::{
    BootConsole, BootFirmware, BootFramebuffer, BootInitramfs, BootStack, BOOT_CONSOLE_16550_PORT,
    BOOT_FIRMWARE_ACPI, FRAMEBUFFER_FORMAT_BGR, FRAMEBUFFER_FORMAT_RGB, FRAMEBUFFER_SOURCE_GRUB,
    KERNEL_STACK_SIZE,
};
use rustos_abi::{
    BootInfo, MemRegion, MemRegionKind, BOOT_INFO_MAGIC, BOOT_INFO_VERSION, MEMMAP_MAX_REGIONS,
    PAGE_SIZE,
};

const MULTIBOOT2_BOOT_MAGIC: u32 = 0x36d7_6289;
const TAG_END: u32 = 0;
const TAG_MODULE: u32 = 3;
const TAG_MMAP: u32 = 6;
const TAG_FRAMEBUFFER: u32 = 8;
const TAG_ACPI_OLD: u32 = 14;
const TAG_ACPI_NEW: u32 = 15;
const MAX_BOOT_INFO_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IDENTITY_ADDRESS: u64 = 128 * 1024 * 1024 * 1024 * 1024;
// Для 128 TiB с 1-GiB huge pages нужны PML4 и 256 PDPT, то есть чуть больше
// 1 MiB. Двойной запас не раздувает ELF BSS и позволяет GRUB загрузиться уже
// при минимальных 128 MiB RAM. На старом CPU без 1-GiB pages этот же pool
// покрывает первые сотни GiB страницами 2 MiB; такие CPU всё равно не могут
// адресовать заявленные для современной платформы терабайты памяти.
const GRUB_PAGE_TABLE_BUDGET: u64 = 2 * 1024 * 1024;
const PAGE_2M: u64 = 2 * 1024 * 1024;
const PAGE_1G: u64 = 1024 * 1024 * 1024;
const PAGE_512G: u64 = 512 * PAGE_1G;
const PTE_PRESENT_WRITE: u64 = 0x003;
const PTE_HUGE: u64 = 0x080;

#[repr(C, align(4096))]
struct PageTablePool([u64; GRUB_PAGE_TABLE_BUDGET as usize / 8]);

// BootInfo и page tables являются частью kernel image, поэтому их диапазон
// автоматически исключается из выдаваемой allocator'у памяти.
static mut BOOT_INFO: BootInfo = empty_boot_info();
static mut PAGE_TABLE_POOL: PageTablePool = PageTablePool([0; GRUB_PAGE_TABLE_BUDGET as usize / 8]);

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
    static rustos_multiboot2_stack_bottom: u8;
    static rustos_multiboot2_stack_top: u8;
}

#[derive(Clone, Copy)]
struct Interval {
    start: u64,
    end: u64,
}

impl Interval {
    const EMPTY: Self = Self { start: 0, end: 0 };

    fn normalized(start: u64, end: u64) -> Option<Self> {
        let start = align_down(start, PAGE_SIZE);
        let end = align_up(end, PAGE_SIZE)?;
        (start < end).then_some(Self { start, end })
    }
}

#[derive(Clone, Copy)]
struct Tag {
    address: u64,
    kind: u32,
    size: u32,
}

#[derive(Clone, Copy, Debug)]
enum BootError {
    WrongMagic,
    InvalidInformation,
    MissingMemoryMap,
    MissingInitramfs,
    TooManyRegions,
    AddressSpaceTooLarge,
    PageTableBudget,
}

/// Первая 64-битная Rust-функция. Вызывается только ассемблерным bootstrap.
#[no_mangle]
unsafe extern "C" fn rustos_multiboot2_entry64(magic: u64, information: u64) -> ! {
    let (info, max_phys) = match unsafe { parse_multiboot(magic as u32, information) } {
        Ok(result) => result,
        Err(_) => early_halt(b"[grub] FATAL: invalid Multiboot2 information\n"),
    };

    // BOOT_INFO лежит внутри зарезервированного kernel range и живёт столько
    // же, сколько ядро. Записываем его до смены CR3.
    unsafe { ptr::addr_of_mut!(BOOT_INFO).write(info) };
    let info_ptr = ptr::addr_of!(BOOT_INFO);
    crate::serial::init(unsafe { &*info_ptr });
    crate::serial::put_str("[grub] Multiboot2 tags normalized; installing identity map\n");

    let root = match unsafe { build_identity_map(max_phys) } {
        Ok(root) => root,
        Err(error) => {
            crate::serial::put_str("[grub] FATAL: identity map failed: ");
            crate::serial::put_str(match error {
                BootError::AddressSpaceTooLarge => "physical address exceeds 128 TiB\n",
                BootError::PageTableBudget => "2 MiB page-table budget exhausted\n",
                _ => "invalid boot data\n",
            });
            loop {
                unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
            }
        }
    };
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags)) };
    crate::serial::put_str("[grub] long-mode identity map ready; entering kernel\n");
    unsafe { crate::_start(info_ptr) }
}

unsafe fn parse_multiboot(magic: u32, information: u64) -> Result<(BootInfo, u64), BootError> {
    if magic != MULTIBOOT2_BOOT_MAGIC || !information.is_multiple_of(8) {
        return Err(BootError::WrongMagic);
    }
    let total_size = u64::from(unsafe { read_u32(information)? });
    if !(16..=MAX_BOOT_INFO_BYTES).contains(&total_size) {
        return Err(BootError::InvalidInformation);
    }
    let information_end = information
        .checked_add(total_size)
        .ok_or(BootError::InvalidInformation)?;

    let kernel = Interval::normalized(
        ptr::addr_of!(__kernel_start) as u64,
        ptr::addr_of!(__kernel_end) as u64,
    )
    .ok_or(BootError::InvalidInformation)?;
    let mbi =
        Interval::normalized(information, information_end).ok_or(BootError::InvalidInformation)?;

    let mut module = None;
    let mut mmap = None;
    let mut framebuffer = BootFramebuffer::ZERO;
    let mut rsdp = 0u64;
    let mut max_phys = kernel.end.max(mbi.end);

    let mut cursor = information + 8;
    while cursor < information_end {
        let tag = unsafe { read_tag(cursor, information_end)? };
        match tag.kind {
            TAG_END => break,
            TAG_MODULE if module.is_none() => {
                if tag.size < 16 {
                    return Err(BootError::InvalidInformation);
                }
                let start = u64::from(unsafe { read_u32(tag.address + 8)? });
                let end = u64::from(unsafe { read_u32(tag.address + 12)? });
                if start >= end {
                    return Err(BootError::InvalidInformation);
                }
                module = Some((start, end));
                max_phys = max_phys.max(end);
            }
            TAG_MMAP => mmap = Some(tag),
            TAG_FRAMEBUFFER => {
                if let Some(parsed) = unsafe { parse_framebuffer(tag)? } {
                    let bytes = u64::from(parsed.stride)
                        .checked_mul(u64::from(parsed.height))
                        .ok_or(BootError::InvalidInformation)?;
                    max_phys = max_phys.max(parsed.phys_addr.saturating_add(bytes));
                    framebuffer = parsed;
                }
            }
            TAG_ACPI_OLD if rsdp == 0 && tag.size >= 28 => rsdp = tag.address + 8,
            // Предпочитаем ACPI 2.0+ RSDP, даже если old tag шёл раньше.
            TAG_ACPI_NEW if tag.size >= 44 => rsdp = tag.address + 8,
            _ => {}
        }
        cursor = next_tag(tag.address, tag.size)?;
    }

    let (module_start, module_end) = module.ok_or(BootError::MissingInitramfs)?;
    let module_interval =
        Interval::normalized(module_start, module_end).ok_or(BootError::InvalidInformation)?;
    let mmap = mmap.ok_or(BootError::MissingMemoryMap)?;
    let mut protected = [kernel, module_interval, mbi];
    protected.sort_unstable_by_key(|interval| interval.start);

    let mut info = empty_boot_info();
    info.framebuffer = framebuffer;
    info.console = BootConsole {
        kind: BOOT_CONSOLE_16550_PORT,
        flags: 0,
        base: 0x3f8,
        clock_hz: 1_843_200,
        baud: 115_200,
    };
    info.firmware = BootFirmware {
        kind: BOOT_FIRMWARE_ACPI,
        _reserved: 0,
        root: rsdp,
    };
    info.initramfs = BootInitramfs {
        phys_addr: module_start,
        size: module_end - module_start,
    };
    info.kernel_phys = kernel.start;
    info.kernel_size = kernel.end - kernel.start;
    info.boot_stack = BootStack {
        top: ptr::addr_of!(rustos_multiboot2_stack_top) as u64,
        size: KERNEL_STACK_SIZE,
    };

    unsafe { import_memory_map(&mut info, mmap, &protected, &mut max_phys)? };
    if rsdp == 0 {
        return Err(BootError::InvalidInformation);
    }
    Ok((info, max_phys))
}

unsafe fn import_memory_map(
    info: &mut BootInfo,
    tag: Tag,
    protected: &[Interval],
    max_phys: &mut u64,
) -> Result<(), BootError> {
    if tag.size < 16 {
        return Err(BootError::InvalidInformation);
    }
    let entry_size = u64::from(unsafe { read_u32(tag.address + 8)? });
    if entry_size < 24 || !entry_size.is_multiple_of(8) {
        return Err(BootError::InvalidInformation);
    }
    let end = tag.address + u64::from(tag.size);
    let mut cursor = tag.address + 16;
    while cursor
        .checked_add(entry_size)
        .is_some_and(|next| next <= end)
    {
        let base = unsafe { read_u64(cursor)? };
        let length = unsafe { read_u64(cursor + 8)? };
        let source_kind = unsafe { read_u32(cursor + 16)? };
        let raw_end = base
            .checked_add(length)
            .ok_or(BootError::InvalidInformation)?;
        *max_phys = (*max_phys).max(raw_end);
        if source_kind == 1 {
            let start = align_up(base, PAGE_SIZE).ok_or(BootError::InvalidInformation)?;
            let region_end = align_down(raw_end, PAGE_SIZE);
            append_usable_parts(info, start, region_end, protected)?;
        } else {
            let start = align_down(base, PAGE_SIZE);
            let region_end = align_up(raw_end, PAGE_SIZE).ok_or(BootError::InvalidInformation)?;
            let kind = match source_kind {
                3 => MemRegionKind::AcpiReclaim,
                4 => MemRegionKind::AcpiNvs,
                5 => MemRegionKind::Reserved,
                _ => MemRegionKind::Reserved,
            };
            append_region(info, start, region_end, kind)?;
        }
        cursor += entry_size;
    }
    Ok(())
}

fn append_usable_parts(
    info: &mut BootInfo,
    start: u64,
    end: u64,
    protected: &[Interval],
) -> Result<(), BootError> {
    if start >= end {
        return Ok(());
    }
    let mut current = start;
    for interval in protected {
        if interval.end <= current || interval.start >= end {
            continue;
        }
        append_region(
            info,
            current,
            interval.start.min(end),
            MemRegionKind::Usable,
        )?;
        current = current.max(interval.end);
        if current >= end {
            return Ok(());
        }
    }
    append_region(info, current, end, MemRegionKind::Usable)
}

fn append_region(
    info: &mut BootInfo,
    start: u64,
    end: u64,
    kind: MemRegionKind,
) -> Result<(), BootError> {
    if start >= end {
        return Ok(());
    }
    let index = info.memmap_count as usize;
    if index == MEMMAP_MAX_REGIONS {
        return Err(BootError::TooManyRegions);
    }
    info.memmap[index] = MemRegion {
        kind: kind as u32,
        _pad: 0,
        phys_start: start,
        size: end - start,
    };
    info.memmap_count += 1;
    Ok(())
}

unsafe fn parse_framebuffer(tag: Tag) -> Result<Option<BootFramebuffer>, BootError> {
    if tag.size < 38 {
        return Ok(None);
    }
    let address = unsafe { read_u64(tag.address + 8)? };
    let stride = unsafe { read_u32(tag.address + 16)? };
    let width = unsafe { read_u32(tag.address + 20)? };
    let height = unsafe { read_u32(tag.address + 24)? };
    let bpp = unsafe { read_u8(tag.address + 28)? };
    let framebuffer_type = unsafe { read_u8(tag.address + 29)? };
    if address == 0 || width == 0 || height == 0 || bpp != 32 || framebuffer_type != 1 {
        return Ok(None);
    }
    let red_position = unsafe { read_u8(tag.address + 32)? };
    let red_size = unsafe { read_u8(tag.address + 33)? };
    let green_position = unsafe { read_u8(tag.address + 34)? };
    let green_size = unsafe { read_u8(tag.address + 35)? };
    let blue_position = unsafe { read_u8(tag.address + 36)? };
    let blue_size = unsafe { read_u8(tag.address + 37)? };
    if red_size != 8 || green_size != 8 || blue_size != 8 || green_position != 8 {
        return Ok(None);
    }
    let format = match (red_position, blue_position) {
        (0, 16) => FRAMEBUFFER_FORMAT_RGB,
        (16, 0) => FRAMEBUFFER_FORMAT_BGR,
        _ => return Ok(None),
    };
    if stride < width.checked_mul(4).ok_or(BootError::InvalidInformation)? {
        return Ok(None);
    }
    Ok(Some(BootFramebuffer {
        phys_addr: address,
        width,
        height,
        stride,
        bpp: u32::from(bpp),
        format,
        _reserved: FRAMEBUFFER_SOURCE_GRUB,
    }))
}

unsafe fn build_identity_map(max_phys: u64) -> Result<u64, BootError> {
    let limit =
        align_up(max_phys.max(4 * PAGE_1G), PAGE_1G).ok_or(BootError::AddressSpaceTooLarge)?;
    if limit > MAX_IDENTITY_ADDRESS {
        return Err(BootError::AddressSpaceTooLarge);
    }
    let pool = unsafe { ptr::addr_of_mut!(PAGE_TABLE_POOL.0).cast::<u64>() };
    let entries = GRUB_PAGE_TABLE_BUDGET as usize / 8;
    unsafe { ptr::write_bytes(pool, 0, entries) };
    let mut tables = TableBuilder {
        base: pool,
        next_table: 0,
        table_capacity: GRUB_PAGE_TABLE_BUDGET as usize / PAGE_SIZE as usize,
    };
    let pml4 = tables.allocate()?;
    let supports_1g = core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 26) != 0;
    let pml4_count = limit.div_ceil(PAGE_512G);
    for pml4_index in 0..pml4_count {
        let pdpt = tables.allocate()?;
        unsafe {
            pml4.add(pml4_index as usize)
                .write(pdpt as u64 | PTE_PRESENT_WRITE)
        };
        let pdpt_base = pml4_index * PAGE_512G;
        let pdpt_count = (limit - pdpt_base).min(PAGE_512G).div_ceil(PAGE_1G);
        for pdpt_index in 0..pdpt_count {
            let gigabyte = pdpt_base + pdpt_index * PAGE_1G;
            if supports_1g {
                unsafe {
                    pdpt.add(pdpt_index as usize)
                        .write(gigabyte | PTE_PRESENT_WRITE | PTE_HUGE)
                };
            } else {
                let directory = tables.allocate()?;
                unsafe {
                    pdpt.add(pdpt_index as usize)
                        .write(directory as u64 | PTE_PRESENT_WRITE)
                };
                for index in 0..512u64 {
                    unsafe {
                        directory
                            .add(index as usize)
                            .write((gigabyte + index * PAGE_2M) | PTE_PRESENT_WRITE | PTE_HUGE)
                    };
                }
            }
        }
    }
    Ok(pml4 as u64)
}

struct TableBuilder {
    base: *mut u64,
    next_table: usize,
    table_capacity: usize,
}

impl TableBuilder {
    fn allocate(&mut self) -> Result<*mut u64, BootError> {
        if self.next_table == self.table_capacity {
            return Err(BootError::PageTableBudget);
        }
        let result = unsafe { self.base.add(self.next_table * 512) };
        self.next_table += 1;
        Ok(result)
    }
}

unsafe fn read_tag(address: u64, limit: u64) -> Result<Tag, BootError> {
    let header_end = address
        .checked_add(8)
        .ok_or(BootError::InvalidInformation)?;
    if header_end > limit || !address.is_multiple_of(8) {
        return Err(BootError::InvalidInformation);
    }
    let kind = unsafe { read_u32(address)? };
    let size = unsafe { read_u32(address + 4)? };
    if size < 8 || address + u64::from(size) > limit {
        return Err(BootError::InvalidInformation);
    }
    Ok(Tag {
        address,
        kind,
        size,
    })
}

fn next_tag(address: u64, size: u32) -> Result<u64, BootError> {
    align_up(
        address
            .checked_add(u64::from(size))
            .ok_or(BootError::InvalidInformation)?,
        8,
    )
    .ok_or(BootError::InvalidInformation)
}

unsafe fn read_u8(address: u64) -> Result<u8, BootError> {
    if address == 0 {
        return Err(BootError::InvalidInformation);
    }
    Ok(unsafe { ptr::read_unaligned(address as *const u8) })
}

unsafe fn read_u32(address: u64) -> Result<u32, BootError> {
    if address == 0 {
        return Err(BootError::InvalidInformation);
    }
    Ok(unsafe { ptr::read_unaligned(address as *const u32) })
}

unsafe fn read_u64(address: u64) -> Result<u64, BootError> {
    if address == 0 {
        return Err(BootError::InvalidInformation);
    }
    Ok(unsafe { ptr::read_unaligned(address as *const u64) })
}

fn early_halt(message: &[u8]) -> ! {
    for byte in message {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") 0x3f8u16,
                in("al") *byte,
                options(nomem, nostack, preserves_flags),
            )
        };
    }
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

const fn empty_boot_info() -> BootInfo {
    BootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        _pad: 0,
        memmap_count: 0,
        _pad2: 0,
        memmap: [MemRegion::ZERO; MEMMAP_MAX_REGIONS],
        framebuffer: BootFramebuffer::ZERO,
        console: BootConsole::NONE,
        firmware: BootFirmware::NONE,
        initramfs: BootInitramfs {
            phys_addr: 0,
            size: 0,
        },
        kernel_phys: 0,
        kernel_size: 0,
        boot_stack: BootStack { top: 0, size: 0 },
    }
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

const fn align_up(value: u64, alignment: u64) -> Option<u64> {
    match value.checked_add(alignment - 1) {
        Some(value) => Some(value & !(alignment - 1)),
        None => None,
    }
}

global_asm!(
    r#"
.section .multiboot2,"a"
.balign 8
.global rustos_multiboot2_header
rustos_multiboot2_header:
    .long 0xe85250d6
    .long 0
    .long rustos_multiboot2_header_end - rustos_multiboot2_header
    .long -(0xe85250d6 + (rustos_multiboot2_header_end - rustos_multiboot2_header))

    /* Optional information request: modules, mmap, framebuffer, ACPI. */
    .short 1
    .short 1
    .long 28
    .long 3
    .long 6
    .long 8
    .long 14
    .long 15
    .balign 8

    /* Нужен true-color scanout; конкретный wide mode выбирает grub.cfg. */
    .short 5
    .short 1
    .long 20
    .long 0
    .long 0
    .long 32
    .balign 8

    .short 6
    .short 1
    .long 8

    .short 0
    .short 0
    .long 8
rustos_multiboot2_header_end:

.section .boot.text,"ax"
.code32
.global rustos_multiboot2_entry
rustos_multiboot2_entry:
    cli
    mov dword ptr [rustos_multiboot2_magic], eax
    mov dword ptr [rustos_multiboot2_info], ebx
    lgdt [rustos_multiboot2_gdt_pointer]

    mov eax, cr4
    or eax, (1 << 4) | (1 << 5)
    mov cr4, eax
    mov eax, offset rustos_multiboot2_initial_pml4
    mov cr3, eax

    mov ecx, 0xc0000080
    rdmsr
    or eax, (1 << 8)
    wrmsr
    mov eax, cr0
    or eax, (1 << 31)
    mov cr0, eax
    /* LLVM integrated assembler не принимает mnemonic ljmp в mixed-mode
       object; это обычный ptr16:32 far jump (opcode EA). */
    .byte 0xea
    .long rustos_multiboot2_long
    .word 0x08

.code64
rustos_multiboot2_long:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    lea rsp, [rip + rustos_multiboot2_stack_top]
    xor rbp, rbp
    mov edi, dword ptr [rip + rustos_multiboot2_magic]
    mov esi, dword ptr [rip + rustos_multiboot2_info]
    call rustos_multiboot2_entry64
    ud2

.section .data.rustos_multiboot2,"aw"
.balign 8
rustos_multiboot2_magic:
    .long 0
rustos_multiboot2_info:
    .long 0

.balign 8
rustos_multiboot2_gdt:
    .quad 0
    .quad 0x00af9a000000ffff
    .quad 0x00cf92000000ffff
rustos_multiboot2_gdt_end:
rustos_multiboot2_gdt_pointer:
    .word rustos_multiboot2_gdt_end - rustos_multiboot2_gdt - 1
    .long rustos_multiboot2_gdt

/* Первые 4 GiB нужны, чтобы безопасно разобрать GRUB tags и module. */
.pushsection .data.rustos_multiboot2_page_values,"aw"
.balign 4096
rustos_multiboot2_initial_pml4:
    .quad rustos_multiboot2_initial_pdpt + 0x003
    .fill 511, 8, 0
rustos_multiboot2_initial_pdpt:
    .quad rustos_multiboot2_initial_pd0 + 0x003
    .quad rustos_multiboot2_initial_pd1 + 0x003
    .quad rustos_multiboot2_initial_pd2 + 0x003
    .quad rustos_multiboot2_initial_pd3 + 0x003
    .fill 508, 8, 0
.set rustos_mb2_page, 0
rustos_multiboot2_initial_pd0:
.rept 512
    .quad (rustos_mb2_page * 0x200000) + 0x083
    .set rustos_mb2_page, rustos_mb2_page + 1
.endr
rustos_multiboot2_initial_pd1:
.rept 512
    .quad (rustos_mb2_page * 0x200000) + 0x083
    .set rustos_mb2_page, rustos_mb2_page + 1
.endr
rustos_multiboot2_initial_pd2:
.rept 512
    .quad (rustos_mb2_page * 0x200000) + 0x083
    .set rustos_mb2_page, rustos_mb2_page + 1
.endr
rustos_multiboot2_initial_pd3:
.rept 512
    .quad (rustos_mb2_page * 0x200000) + 0x083
    .set rustos_mb2_page, rustos_mb2_page + 1
.endr
.popsection

.section .bss.rustos_multiboot2_stack,"aw",@nobits
.balign 16
.global rustos_multiboot2_stack_bottom
rustos_multiboot2_stack_bottom:
    .skip {kernel_stack_size}
.global rustos_multiboot2_stack_top
rustos_multiboot2_stack_top:
.code64
"#,
    kernel_stack_size = const KERNEL_STACK_SIZE,
);

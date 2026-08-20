//! Запуск application processors по ACPI MADT и INIT-SIPI-SIPI.
//!
//! AP входит в long mode через копируемый trampoline ниже 1 MiB, получает
//! отдельный stack и подтверждает APIC ID. Пока per-CPU TSS/IDT не готовы,
//! ядро оставляет AP в `cli; hlt`: это реальный запуск CPU, но ещё не ложное
//! заявление о готовом SMP scheduler.

use core::{
    arch::{asm, global_asm},
    ptr,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use super::apic;

const TRAMPOLINE_PHYS: u64 = 0x8000;
const SIPI_VECTOR: u32 = (TRAMPOLINE_PHYS >> 12) as u32;
/// Совпадает с первой 64-битной affinity mask scheduler'а. Расширение выше
/// 64 CPU потребует иерархических CPU sets, а не молчаливого обрезания MADT.
const MAX_CPUS: usize = 64;
const MAX_APS: usize = MAX_CPUS - 1;
const AP_STACK_SIZE: usize = 32 * 1024;
const INIT_ASSERT: u32 = (5 << 8) | (1 << 14) | (1 << 15);
const INIT_DEASSERT: u32 = (5 << 8) | (1 << 15);
const STARTUP_IPI: u32 = (6 << 8) | SIPI_VECTOR;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmpError {
    InvalidRsdp,
    MissingMadt,
    InvalidMadt,
    TooManyCpus,
    TrampolineTooLarge,
    ApTimeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmpInfo {
    /// BSP + успешно запущенные AP.
    pub online_cpus: usize,
    /// Число APIC IDs, объявленных firmware как enabled/online-capable.
    pub discovered_cpus: usize,
}

#[repr(C, align(4096))]
struct ApStack([u8; AP_STACK_SIZE]);

static mut AP_STACKS: [ApStack; MAX_APS] = [const { ApStack([0; AP_STACK_SIZE]) }; MAX_APS];
static STARTING_SLOT: AtomicUsize = AtomicUsize::new(usize::MAX);
static READY_MASK: AtomicU64 = AtomicU64::new(0);
static AP_IDS: [AtomicU64; MAX_APS] = [const { AtomicU64::new(u64::MAX) }; MAX_APS];

extern "C" {
    static rustos_ap_trampoline_start: u8;
    static rustos_ap_trampoline_end: u8;
    static rustos_ap_trampoline_cr3: u8;
    static rustos_ap_trampoline_stack: u8;
    static rustos_ap_trampoline_entry: u8;
    static rustos_ap_trampoline_gdt: u8;
    static rustos_ap_trampoline_gdt_base: u8;
    static rustos_ap_trampoline_gdt_operand: u8;
    static rustos_ap_trampoline_gdt_ptr: u8;
    static rustos_ap_trampoline_cr3_operand: u8;
    static rustos_ap_trampoline_protected: u8;
    static rustos_ap_trampoline_protected_operand: u8;
    static rustos_ap_trampoline_long: u8;
    static rustos_ap_trampoline_long_operand: u8;
}

/// Находит CPU в MADT и последовательно запускает каждый AP. Последовательный
/// patch одного trampoline исключает race между AP, читающими stack address.
pub fn start_application_processors(rsdp: u64, tsc_hz: u64) -> Result<SmpInfo, SmpError> {
    let bsp = apic::local_id();
    let cpus = unsafe { discover_cpus(rsdp)? };
    prepare_trampoline()?;
    READY_MASK.store(0, Ordering::Release);

    let mut started = 0usize;
    for apic_id in cpus.ids[..cpus.len].iter().copied().filter(|id| *id != bsp) {
        if started == MAX_APS {
            return Err(SmpError::TooManyCpus);
        }
        patch_ap_boot_values(started)?;
        AP_IDS[started].store(u64::MAX, Ordering::Release);
        STARTING_SLOT.store(started, Ordering::Release);

        apic::send_ipi(apic_id, INIT_ASSERT);
        apic::delay_microseconds(tsc_hz, 10_000);
        apic::send_ipi(apic_id, INIT_DEASSERT);
        apic::delay_microseconds(tsc_hz, 200);
        apic::send_ipi(apic_id, STARTUP_IPI);
        apic::delay_microseconds(tsc_hz, 200);
        apic::send_ipi(apic_id, STARTUP_IPI);

        let bit = 1u64 << started;
        let deadline = apic::read_tsc().saturating_add(tsc_hz / 5);
        while READY_MASK.load(Ordering::Acquire) & bit == 0 {
            if apic::read_tsc() >= deadline {
                return Err(SmpError::ApTimeout);
            }
            core::hint::spin_loop();
        }
        if AP_IDS[started].load(Ordering::Acquire) as u32 != apic_id {
            return Err(SmpError::ApTimeout);
        }
        started += 1;
    }
    STARTING_SLOT.store(usize::MAX, Ordering::Release);
    Ok(SmpInfo {
        online_cpus: 1 + started,
        discovered_cpus: cpus.len,
    })
}

fn prepare_trampoline() -> Result<(), SmpError> {
    let source = ptr::addr_of!(rustos_ap_trampoline_start) as usize;
    let end = ptr::addr_of!(rustos_ap_trampoline_end) as usize;
    let size = end
        .checked_sub(source)
        .ok_or(SmpError::TrampolineTooLarge)?;
    if size == 0 || size > 4096 {
        return Err(SmpError::TrampolineTooLarge);
    }
    unsafe {
        ptr::copy_nonoverlapping(source as *const u8, TRAMPOLINE_PHYS as *mut u8, size);
    }
    patch_u32(
        ptr::addr_of!(rustos_ap_trampoline_gdt_base) as usize - source,
        trampoline_symbol(ptr::addr_of!(rustos_ap_trampoline_gdt)) as u32,
    );
    patch_u32(
        ptr::addr_of!(rustos_ap_trampoline_cr3) as usize - source,
        super::read_cr3() as u32,
    );
    patch_u16(
        ptr::addr_of!(rustos_ap_trampoline_gdt_operand) as usize - source,
        trampoline_symbol(ptr::addr_of!(rustos_ap_trampoline_gdt_ptr)) as u16,
    );
    patch_u32(
        ptr::addr_of!(rustos_ap_trampoline_cr3_operand) as usize - source,
        trampoline_symbol(ptr::addr_of!(rustos_ap_trampoline_cr3)) as u32,
    );
    patch_u32(
        ptr::addr_of!(rustos_ap_trampoline_protected_operand) as usize - source,
        trampoline_symbol(ptr::addr_of!(rustos_ap_trampoline_protected)) as u32,
    );
    patch_u32(
        ptr::addr_of!(rustos_ap_trampoline_long_operand) as usize - source,
        trampoline_symbol(ptr::addr_of!(rustos_ap_trampoline_long)) as u32,
    );
    Ok(())
}

fn patch_ap_boot_values(slot: usize) -> Result<(), SmpError> {
    if slot >= MAX_APS {
        return Err(SmpError::TooManyCpus);
    }
    let source = ptr::addr_of!(rustos_ap_trampoline_start) as usize;
    let stack = ptr::addr_of_mut!(AP_STACKS) as *mut ApStack;
    let stack_top = unsafe { stack.add(slot) } as u64 + AP_STACK_SIZE as u64;
    patch_u64(
        ptr::addr_of!(rustos_ap_trampoline_stack) as usize - source,
        stack_top,
    );
    patch_u64(
        ptr::addr_of!(rustos_ap_trampoline_entry) as usize - source,
        rustos_ap_entry as *const () as u64,
    );
    unsafe { asm!("mfence", options(nostack, preserves_flags)) };
    Ok(())
}

fn trampoline_symbol(symbol: *const u8) -> u64 {
    symbol as u64 - ptr::addr_of!(rustos_ap_trampoline_start) as u64 + TRAMPOLINE_PHYS
}

fn patch_u32(offset: usize, value: u32) {
    unsafe {
        (TRAMPOLINE_PHYS as *mut u8)
            .add(offset)
            .cast::<u32>()
            .write_unaligned(value)
    };
}

fn patch_u16(offset: usize, value: u16) {
    unsafe {
        (TRAMPOLINE_PHYS as *mut u8)
            .add(offset)
            .cast::<u16>()
            .write_unaligned(value)
    };
}

fn patch_u64(offset: usize, value: u64) {
    unsafe {
        (TRAMPOLINE_PHYS as *mut u8)
            .add(offset)
            .cast::<u64>()
            .write_unaligned(value)
    };
}

/// Первый Rust-код AP. Trampoline уже установил long mode, CR3 и отдельный
/// stack. AP включает собственный local APIC, публикует ID и паркуется.
#[no_mangle]
extern "C" fn rustos_ap_entry() -> ! {
    let slot = STARTING_SLOT.load(Ordering::Acquire);
    if slot < MAX_APS {
        if let Ok(info) = apic::initialize_local() {
            AP_IDS[slot].store(u64::from(info.id), Ordering::Release);
            READY_MASK.fetch_or(1u64 << slot, Ordering::AcqRel);
        }
    }
    loop {
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

struct CpuList {
    ids: [u32; MAX_CPUS],
    len: usize,
}

impl CpuList {
    fn push_unique(&mut self, id: u32) -> Result<(), SmpError> {
        if self.ids[..self.len].contains(&id) {
            return Ok(());
        }
        if self.len == MAX_CPUS {
            return Err(SmpError::TooManyCpus);
        }
        self.ids[self.len] = id;
        self.len += 1;
        Ok(())
    }
}

/// Читает только проверенные checksum/length ACPI tables из identity map.
unsafe fn discover_cpus(rsdp: u64) -> Result<CpuList, SmpError> {
    if rsdp == 0 {
        return Err(SmpError::InvalidRsdp);
    }
    let rsdp_bytes = unsafe { core::slice::from_raw_parts(rsdp as *const u8, 36) };
    if &rsdp_bytes[..8] != b"RSD PTR " || checksum(&rsdp_bytes[..20]) != 0 {
        return Err(SmpError::InvalidRsdp);
    }
    let revision = rsdp_bytes[15];
    let (root, entry_size) = if revision >= 2 {
        let length = read_u32(rsdp_bytes, 20).ok_or(SmpError::InvalidRsdp)? as usize;
        if !(36..=4096).contains(&length) {
            return Err(SmpError::InvalidRsdp);
        }
        let extended = unsafe { core::slice::from_raw_parts(rsdp as *const u8, length) };
        if checksum(extended) != 0 {
            return Err(SmpError::InvalidRsdp);
        }
        (read_u64(extended, 24).ok_or(SmpError::InvalidRsdp)?, 8usize)
    } else {
        (
            u64::from(read_u32(rsdp_bytes, 16).ok_or(SmpError::InvalidRsdp)?),
            4usize,
        )
    };
    let root_table = unsafe { table_bytes(root).ok_or(SmpError::InvalidRsdp)? };
    let mut madt = None;
    let mut cursor = 36usize;
    while cursor + entry_size <= root_table.len() {
        let address = if entry_size == 8 {
            read_u64(root_table, cursor).ok_or(SmpError::InvalidRsdp)?
        } else {
            u64::from(read_u32(root_table, cursor).ok_or(SmpError::InvalidRsdp)?)
        };
        if let Some(table) = unsafe { table_bytes(address) } {
            if table.get(..4) == Some(b"APIC") {
                madt = Some(table);
                break;
            }
        }
        cursor += entry_size;
    }
    parse_madt(madt.ok_or(SmpError::MissingMadt)?)
}

fn parse_madt(table: &[u8]) -> Result<CpuList, SmpError> {
    if table.len() < 44 {
        return Err(SmpError::InvalidMadt);
    }
    let mut result = CpuList {
        ids: [0; MAX_CPUS],
        len: 0,
    };
    let mut cursor = 44usize;
    while cursor + 2 <= table.len() {
        let kind = table[cursor];
        let length = table[cursor + 1] as usize;
        if length < 2 || cursor + length > table.len() {
            return Err(SmpError::InvalidMadt);
        }
        match kind {
            0 if length >= 8 => {
                let flags = read_u32(table, cursor + 4).ok_or(SmpError::InvalidMadt)?;
                if flags & 3 != 0 {
                    result.push_unique(u32::from(table[cursor + 3]))?;
                }
            }
            9 if length >= 16 => {
                let id = read_u32(table, cursor + 4).ok_or(SmpError::InvalidMadt)?;
                let flags = read_u32(table, cursor + 8).ok_or(SmpError::InvalidMadt)?;
                if flags & 3 != 0 {
                    result.push_unique(id)?;
                }
            }
            _ => {}
        }
        cursor += length;
    }
    if result.len == 0 {
        return Err(SmpError::InvalidMadt);
    }
    Ok(result)
}

unsafe fn table_bytes(address: u64) -> Option<&'static [u8]> {
    if address == 0 {
        return None;
    }
    let header = unsafe { core::slice::from_raw_parts(address as *const u8, 36) };
    let length = read_u32(header, 4)? as usize;
    if !(36..=1024 * 1024).contains(&length) {
        return None;
    }
    let table = unsafe { core::slice::from_raw_parts(address as *const u8, length) };
    (checksum(table) == 0).then_some(table)
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .copied()
        .fold(0u8, |sum, value| sum.wrapping_add(value))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

global_asm!(
    r#"
.pushsection .text.rustos_ap_trampoline,"ax"
.code16
.global rustos_ap_trampoline_start
rustos_ap_trampoline_start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    .byte 0xbb
.global rustos_ap_trampoline_gdt_operand
rustos_ap_trampoline_gdt_operand:
    .word 0
    lgdt [bx]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    .byte 0x66, 0xea
.global rustos_ap_trampoline_protected_operand
rustos_ap_trampoline_protected_operand:
    .long 0
    .word 0x08

.code32
.global rustos_ap_trampoline_protected
rustos_ap_trampoline_protected:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, cr4
    or eax, (1 << 5) | (1 << 9) | (1 << 10)
    mov cr4, eax
    mov eax, cr0
    and eax, ~(1 << 2)
    or eax, (1 << 1)
    mov cr0, eax
    .byte 0xbb
.global rustos_ap_trampoline_cr3_operand
rustos_ap_trampoline_cr3_operand:
    .long 0
    mov eax, dword ptr [ebx]
    mov cr3, eax
    mov ecx, 0xc0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)
    wrmsr
    mov eax, cr0
    or eax, (1 << 31)
    mov cr0, eax
    .byte 0xea
.global rustos_ap_trampoline_long_operand
rustos_ap_trampoline_long_operand:
    .long 0
    .word 0x18

.code64
.global rustos_ap_trampoline_long
rustos_ap_trampoline_long:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov rsp, qword ptr [rip + rustos_ap_trampoline_stack]
    xor rbp, rbp
    mov rax, qword ptr [rip + rustos_ap_trampoline_entry]
    call rax
    ud2

.align 8
.global rustos_ap_trampoline_gdt
rustos_ap_trampoline_gdt:
    .quad 0x0000000000000000
    .quad 0x00cf9a000000ffff
    .quad 0x00cf92000000ffff
    .quad 0x00af9a000000ffff
rustos_ap_trampoline_gdt_end:

.global rustos_ap_trampoline_gdt_ptr
rustos_ap_trampoline_gdt_ptr:
    .word rustos_ap_trampoline_gdt_end - rustos_ap_trampoline_gdt - 1
.global rustos_ap_trampoline_gdt_base
rustos_ap_trampoline_gdt_base:
    .long 0

.global rustos_ap_trampoline_cr3
rustos_ap_trampoline_cr3:
    .long 0
.global rustos_ap_trampoline_stack
rustos_ap_trampoline_stack:
    .quad 0
.global rustos_ap_trampoline_entry
rustos_ap_trampoline_entry:
    .quad 0

.global rustos_ap_trampoline_end
rustos_ap_trampoline_end:
.code64
.popsection
"#
);

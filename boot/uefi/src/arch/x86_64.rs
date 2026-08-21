//! AMD64 UEFI handoff. Вне этого файла загрузчик не содержит ASM.

use rustos_abi::bootinfo::{BootConsole, BOOT_CONSOLE_16550_PORT, BOOT_FIRMWARE_ACPI};
use uefi::mem::memory_map::MemoryMapOwned;
use uefi::system;
use uefi::table::cfg::ConfigTableEntry;

/// BootInfo.firmware.kind для x86-64.
pub const FIRMWARE_KIND: u32 = BOOT_FIRMWARE_ACPI;

/// ACPI RSDP: EFI config table (ACPI 2.0 GUID, затем ACPI 1.0),
/// fallback — legacy scan `0xE0000..0x100000` (шаг 16 байт) и EBDA.
pub fn find_firmware() -> (u64, u64) {
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
        return (rsdp, 4096);
    }
    // Legacy fallback.
    let sig = b"RSD PTR ";
    let mut addr = 0xE0000u64;
    while addr < 0x100_0000 {
        // SAFETY: identity-маппинг UEFI, регион 0xE0000..0x1000000 — ROM/RAM.
        if unsafe { has_signature(addr, sig) } {
            return (addr, 4096);
        }
        addr += 16;
    }
    let ebda = unsafe { (0x40E as *const u16).read_volatile() };
    if (ebda as u32) > 0x40E {
        let base = (ebda as u64) & 0xFFFF_F000;
        let mut a = base;
        while a < base + 1024 {
            // SAFETY: EBDA — валидная низкая память (identity-маппинг).
            if unsafe { has_signature(a, sig) } {
                return (a, 4096);
            }
            a += 16;
        }
    }
    (0, 0)
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

/// BootInfo.console: COM1 16550.
pub fn boot_console() -> BootConsole {
    BootConsole {
        kind: BOOT_CONSOLE_16550_PORT,
        flags: 0,
        base: 0x3f8,
        clock_hz: 1_843_200,
        baud: 115_200,
    }
}

/// Virt→phys: OVMF identity-maps RAM, virt == phys.
pub fn virt_to_phys(virt: u64, _map: &MemoryMapOwned) -> u64 {
    virt
}

/// # Safety
///
/// Все адреса identity-mapped, `page_root` — валидная PML4, `stack_pointer`
/// соответствует SysV ABI, `entry` — точка входа загруженного kernel ELF.
pub unsafe fn jump_to_kernel(page_root: u64, stack_pointer: u64, boot_info: u64, entry: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "cli",
            "mov cr3, {page_root}",
            "mov rsp, {stack_pointer}",
            "mov rdi, {boot_info}",
            "jmp {entry}",
            page_root = in(reg) page_root,
            stack_pointer = in(reg) stack_pointer,
            boot_info = in(reg) boot_info,
            entry = in(reg) entry,
            out("rdi") _,
            options(nostack, nomem),
        );
    }
    unsafe { core::hint::unreachable_unchecked() }
}

pub fn debug_exit(code: u8) -> ! {
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("eax") code as u32,
            in("dx") 0xF4u16,
            options(nomem, nostack),
        );
    }
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

pub fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack),
        );
    }
    value
}

pub fn outb(port: u16, value: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("al") value,
            in("dx") port,
            options(nomem, nostack),
        );
    }
}

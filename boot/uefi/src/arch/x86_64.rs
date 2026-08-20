//! AMD64 UEFI handoff. Вне этого файла загрузчик не содержит ASM.

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

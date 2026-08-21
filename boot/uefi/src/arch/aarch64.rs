//! AArch64 UEFI handoff: EL2→EL1 transition, PL011 diagnostics, DTB
//! discovery, and virt→phys translation.
//!
//! Вне этого файла загрузчик не содержит AArch64 ASM.

use rustos_abi::bootinfo::{BootConsole, BOOT_CONSOLE_PL011, BOOT_FIRMWARE_DEVICE_TREE};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned};
use uefi::system;
use uefi::table::cfg::ConfigTableEntry;
use uefi::Guid;

/// QEMU virt: PL011 UART.
const PL011_BASE: u64 = 0x0900_0000;
/// PL011: TX FIFO full (offset 0x18, bit 5) — семантика ядра serial.rs.
const PL011_TXFF_OFFSET: u64 = 0x18;
const PL011_TXFF_BIT: u32 = 1 << 5;

/// EFI_DEVICE_TREE_GUID (AAVMF публикует DTB в config table).
const DTB_GUID: Guid = Guid::new(
    [0xd5, 0x21, 0xb6, 0xb1],
    [0x9c, 0xf1],
    [0xa5, 0x41],
    0x83,
    0x0b,
    [0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0],
);

/// Handoff AAVMF→EL1.
///
/// Разные сборки AAVMF запускают UEFI application либо в EL1,
/// либо в EL2, поэтому входной exception level нельзя угадывать.
/// Важная деталь ARM: `eret`, выполненный в EL2, берёт адрес и состояние из
/// `ELR_EL2`/`SPSR_EL2`, а `HCR_EL2.RW` выбирает AArch64 для нижнего
/// exception level. Запись `ELR_EL1` или обнуление `HCR_EL2` здесь
/// приводят к немедленному и почти безмолвному exception.
///
/// Последовательность настраивает EL1 translation regime, сбрасывает
/// старые TLB/I-cache entries, разрешает EL1 physical timer и только
/// после этого включает MMU в EL1. EL2 MMU мы не выключаем: она
/// нужна до самого `eret`, а после перехода больше не участвует в
/// EL1 stage-1 translation.
///
/// # Safety
///
/// `page_root` — валидная L0-таблица, `stack_pointer` — 16-byte aligned
/// вершина стека, `vectors` — 1-KiB-выровненная parking-таблица,
/// `entry` — точка входа kernel ELF. Все — из раскладки резерва.
pub unsafe fn jump_to_kernel(
    page_root: u64,
    stack_pointer: u64,
    boot_info: u64,
    entry: u64,
    vectors: u64,
) -> ! {
    // Диагностика перед ERET: дамп регистров входа в ядро в PL011.
    // Если ядро «умирает молча», это последний наблюдаемый срез:
    // PL011 ещё жив (MMU до ERET — маппинг AAVMF).
    let mut current_el: u64;
    let mut tcr: u64;
    let mut mair: u64;
    let mut sctlr: u64;
    // HCR_EL2 нельзя читать из EL1: это само по себе
    // синхронное exception. Сначала узнаём CurrentEL и только потом
    // касаемся EL2-only регистров.
    let mut hcr: u64 = 0;
    // SAFETY: CurrentEL и EL1 translation registers читаются из
    // обоих поддержанных входных уровней.
    unsafe {
        core::arch::asm!(
            "mrs {current_el}, CurrentEL",
            "mrs {tcr}, tcr_el1",
            "mrs {mair}, mair_el1",
            "mrs {sctlr}, sctlr_el1",
            current_el = out(reg) current_el,
            tcr = out(reg) tcr,
            mair = out(reg) mair,
            sctlr = out(reg) sctlr,
            options(nostack, nomem),
        );
    }
    if current_el == 2 << 2 {
        // SAFETY: CurrentEL точно указал EL2.
        unsafe {
            core::arch::asm!(
                "mrs {value}, hcr_el2",
                value = out(reg) hcr,
                options(nostack, nomem),
            );
        }
    }
    dump_u64(b"[handoff] CurrentEL=", current_el);
    dump_u64(b"[handoff] ttbr0=", page_root);
    dump_u64(b"[handoff] vbar  =", vectors);
    dump_u64(b"[handoff] sp_el1=", stack_pointer);
    dump_u64(b"[handoff] elr   =", entry);
    dump_u64(b"[handoff] x0    =", boot_info);
    dump_u64(b"[handoff] tcr   =", tcr);
    dump_u64(b"[handoff] mair  =", mair);
    dump_u64(b"[handoff] sctlr1=", sctlr);
    dump_u64(b"[handoff] hcr   =", hcr);
    match current_el >> 2 {
        // SAFETY: аргументы проверены раскладкой загрузчика.
        1 => unsafe { jump_from_el1(page_root, stack_pointer, boot_info, entry, vectors) },
        // SAFETY: то же; дополнительно CurrentEL подтвердил EL2.
        2 => unsafe { jump_from_el2(page_root, stack_pointer, boot_info, entry, vectors) },
        _ => {
            pl011_put_str(b"[handoff] FATAL: unsupported CurrentEL\n");
            debug_exit(0xe3)
        }
    }
}

/// Устанавливает общую EL1 translation regime и возвращается в
/// ядро через `eret` на том же EL1.
///
/// # Safety
///
/// Все адреса — identity-mapped физические адреса; `page_root`
/// содержит текущий код и стек до самого `eret`.
unsafe fn jump_from_el1(
    page_root: u64,
    stack_pointer: u64,
    boot_info: u64,
    entry: u64,
    vectors: u64,
) -> ! {
    // SAFETY: см. контракт функции. UEFI до SetVirtualAddressMap и
    // наша bootstrap-карта обе используют identity addresses.
    unsafe {
        core::arch::asm!(
            // Сначала выключаем текущую EL1 MMU. PC/SP остаются
            // теми же identity-адресами, поэтому execution продолжается.
            "mrs x9, sctlr_el1",
            "bic x9, x9, #1",
            "msr sctlr_el1, x9",
            "isb",
            "msr ttbr0_el1, {page_root}",
            "msr ttbr1_el1, xzr",
            "msr vbar_el1, {vectors}",
            // В EL1h текущий `sp` и есть SP_EL1. Прямая системная
            // запись `msr sp_el1, ...` из EL1 не определена и даёт #UNDEF.
            "mov sp, {stack_pointer}",
            "msr elr_el1, {entry}",
            "mov x0, {boot_info}",
            // Attr0 = normal WB/WA, Attr1 = device-nGnRE.
            "mov x9, #0x04ff",
            "msr mair_el1, x9",
            // 0x4_0080_3510: 4 KiB, 48-bit lower VA, WB/WA
            // inner-shareable, TTBR1 walks disabled, 44-bit PA. Firmware
            // использовала T0SZ=20 (44-bit VA), но native RUNE load base
            // 0x4000_0000_0000 требует полные 48 бит.
            "mov x9, #0x3510",
            "movk x9, #0x0080, lsl #16",
            "movk x9, #0x0004, lsl #32",
            "msr tcr_el1, x9",
            "mov x9, #0x3c5",
            "msr spsr_el1, x9",
            // Обязательные RES1 биты + M|C|I. Не полагаемся на
            // случайно оставшееся после firmware значение SCTLR_EL1.
            "mov x9, #0x1805",
            "movk x9, #0x30d0, lsl #16",
            "ic iallu",
            "tlbi vmalle1",
            "dsb sy",
            "isb",
            "msr sctlr_el1, x9",
            "isb",
            "eret",
            page_root = in(reg) page_root,
            vectors = in(reg) vectors,
            stack_pointer = in(reg) stack_pointer,
            entry = in(reg) entry,
            boot_info = in(reg) boot_info,
            out("x0") _,
            out("x9") _,
            options(nostack, nomem),
        );
    }
    unsafe { core::hint::unreachable_unchecked() }
}

/// Настраивает EL1 и делает exception return из EL2.
///
/// # Safety
///
/// См. [`jump_from_el1`]; вызывающий дополнительно проверил CurrentEL=2.
unsafe fn jump_from_el2(
    page_root: u64,
    stack_pointer: u64,
    boot_info: u64,
    entry: u64,
    vectors: u64,
) -> ! {
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {page_root}",
            "msr ttbr1_el1, xzr",
            "msr vbar_el1, {vectors}",
            "msr sp_el1, {stack_pointer}",
            "msr elr_el2, {entry}",
            "mov x0, {boot_info}",
            // RW=1 — lower EL AArch64; VM=0 — stage-2 translation выключена.
            "mov x9, #1",
            "lsl x9, x9, #31",
            "msr hcr_el2, x9",
            // EL1PCTEN | EL1PCEN.
            "mov x9, #3",
            "msr cnthctl_el2, x9",
            "msr cntvoff_el2, xzr",
            "isb",
            "mov x9, #0x04ff",
            "msr mair_el1, x9",
            // Та же 48-bit translation regime, что и для EL1 entry.
            "mov x9, #0x3510",
            "movk x9, #0x0080, lsl #16",
            "movk x9, #0x0004, lsl #32",
            "msr tcr_el1, x9",
            "mov x9, #0x3c5",
            "msr spsr_el2, x9",
            "mov x9, #0x1805",
            "movk x9, #0x30d0, lsl #16",
            "ic iallu",
            "tlbi vmalle1",
            "dsb sy",
            "isb",
            "msr sctlr_el1, x9",
            "isb",
            "eret",
            page_root = in(reg) page_root,
            vectors = in(reg) vectors,
            stack_pointer = in(reg) stack_pointer,
            entry = in(reg) entry,
            boot_info = in(reg) boot_info,
            out("x0") _,
            out("x9") _,
            options(nostack, nomem),
        );
    }
    unsafe { core::hint::unreachable_unchecked() }
}

/// Power-off: PL011 marker + PSCI SYSTEM_OFF (HVC, fn 0x84000008).
/// QEMU завершает VM с exit code 0.
pub fn debug_exit(code: u8) -> ! {
    // Kernel уже напечатал "exit code=N" в serial; маркер для CI-ассерта.
    let mut marker = [0u8; 32];
    let prefix = b"RUSTOS_BOOT_EXIT_";
    let hex_digits = b"0123456789abcdef";
    let hi = hex_digits[(code >> 4) as usize];
    let lo = hex_digits[(code & 0xf) as usize];
    let len = prefix.len() + 3; // prefix + 2 hex + '\n'
    marker[..prefix.len()].copy_from_slice(prefix);
    marker[prefix.len()] = hi;
    marker[prefix.len() + 1] = lo;
    marker[prefix.len() + 2] = b'\n';
    for &b in &marker[..len] {
        pl011_put(b);
    }
    // PSCI SYSTEM_OFF: не возвращается.
    unsafe {
        core::arch::asm!(
            "mov w0, #0x0008",
            "movk w0, #0x8400, lsl #16",
            "hvc #0",
            options(nomem, nostack),
        );
    }
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

/// PL011: строка (диагностика pre-ERET, без форматирования).
fn pl011_put_str(s: &[u8]) {
    for &b in s {
        pl011_put(b);
    }
}

/// PL011: `name` + `0x<value 16 hex>` + `\n`.
fn dump_u64(name: &[u8], value: u64) {
    pl011_put_str(name);
    pl011_put_str(b"0x");
    for nibble in (0..16).rev() {
        let d = ((value >> (nibble * 4)) & 0xf) as usize;
        pl011_put(b"0123456789abcdef"[d]);
    }
    pl011_put(b'\n');
}

/// PL011: один байт (TXFF busy-wait + DR write).
#[inline]
fn pl011_put(byte: u8) {
    // SAFETY: PL011 0x09000000 — MMIO QEMU virt, доступен в identity
    // mapping AAVMF.
    unsafe {
        loop {
            let txff_ptr = (PL011_BASE + PL011_TXFF_OFFSET) as *const u32;
            if txff_ptr.read_volatile() & PL011_TXFF_BIT == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        let dr_ptr = PL011_BASE as *mut u32;
        dr_ptr.write_volatile(u32::from(byte));
    }
}

/// DTB из EFI config table. Возвращает `(phys_addr, size)`; `(0, 0)` — нет.
pub fn find_firmware() -> (u64, u64) {
    let mut dtb_addr: u64 = 0;
    system::with_config_table(|entries: &[ConfigTableEntry]| {
        if dtb_addr == 0 {
            if let Some(e) = entries.iter().find(|e| e.guid == DTB_GUID) {
                dtb_addr = e.address as u64;
            }
        }
    });
    if dtb_addr == 0 {
        return (0, 0);
    }
    // FDT header: magic u32 BE (0xd00dfeed) + total_size u32 BE.
    let ptr = dtb_addr as *const u8;
    // SAFETY: AAVMF публикует валидный FDT по адресу из config table.
    let magic = u32::from_be_bytes(unsafe {
        [
            ptr.read_volatile(),
            ptr.add(1).read_volatile(),
            ptr.add(2).read_volatile(),
            ptr.add(3).read_volatile(),
        ]
    });
    if magic != 0xd00d_feed {
        return (0, 0);
    }
    let size = u32::from_be_bytes(unsafe {
        [
            ptr.add(4).read_volatile(),
            ptr.add(5).read_volatile(),
            ptr.add(6).read_volatile(),
            ptr.add(7).read_volatile(),
        ]
    }) as u64;
    // Clamp: минимум 1 страница, максимум 4 MiB (защита от мусора).
    let size = size.clamp(4096, 4 * 1024 * 1024);
    (dtb_addr, size)
}

/// BootInfo.console: PL011 на QEMU virt.
pub fn boot_console() -> BootConsole {
    BootConsole {
        kind: BOOT_CONSOLE_PL011,
        flags: 0,
        base: PL011_BASE,
        clock_hz: 24_576_000,
        baud: 115_200,
    }
}

/// Virt→phys через UEFI memory map descriptors. Fallback: identity.
pub fn virt_to_phys(virt: u64, map: &MemoryMapOwned) -> u64 {
    for d in map.entries() {
        let virt_start = d.virt_start;
        let size = d.page_count * 4096;
        if virt_start <= virt && virt < virt_start + size {
            return d.phys_start + (virt - virt_start);
        }
    }
    virt
}

/// BootInfo.firmware.kind для AArch64.
pub const FIRMWARE_KIND: u32 = BOOT_FIRMWARE_DEVICE_TREE;

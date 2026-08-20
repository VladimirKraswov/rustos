//! Ранняя инициализация ядра: banner, разбор BootInfo, self-test.

use crate::{arch, gui, memory, process, serial};
use rustos_abi::{BootInfo, MemRegion, MemRegionKind, BOOT_INFO_MAGIC, BOOT_INFO_VERSION};

/// Главная функция ядра: serial → валидация BootInfo → banner → self-test,
/// после чего графическая сессия GUI (или немедленный exit в режиме
/// `feature = "boot-test"`).
///
/// Успех boot-теста завершает VM кодом 0 (QEMU isa-debug-exit), сбой —
/// диагностическим кодом (см. [`self_test`]).
pub fn kernel_main(info: &BootInfo) -> ! {
    // 1. Serial — первичный диагностический канал. Загрузчик использовал
    //    UEFI ConOut; UART (COM1, 0x3F8) настроен только здесь.
    serial::init();
    serial::early_put_str("K1: serial init ok\n");

    // 2. Валидация BootInfo до использования полей (ABI-контракт).
    if !info.validate() {
        serial::put_str("[boot] FATAL: BootInfo invalid (magic/version/memmap)\n");
        exit_kernel(0x01);
    }
    serial::early_put_str("K2: bootinfo valid\n");

    print_banner();
    print_bootinfo(info);
    serial::early_put_str("K3: bootinfo printed\n");

    // До первого CPL3 перехода kernel обязан владеть таблицами сегментов,
    // trap stacks и защитой страниц. Прерывания остаются выключенными до
    // настройки local APIC, но синхронные exceptions/syscalls уже изолированы.
    arch::enable_memory_protection();
    let ring0_stack = arch::segmentation::initialize();
    arch::traps::initialize();
    serial::put_str("[arch] GDT/TSS/IDT ready; ring0 stack=0x");
    serial::put_hex(ring0_stack);
    serial::put_str("\n");

    let allocator = match memory::initialize(info) {
        Ok(stats) => stats,
        Err(_) => {
            serial::put_str("[memory] FATAL: frame allocator initialization failed\n");
            exit_kernel(0x40);
        }
    };
    serial::put_str("[memory] frame allocator: free=");
    serial::put_u32((allocator.free_frames * 4 / 1024) as u32);
    serial::put_str(" MiB extents=");
    serial::put_u32(allocator.extents as u32);
    serial::put_str("\n");

    match self_test(info) {
        Ok(()) => {
            serial::early_put_str("K4: selftest ok\n");
            if process::run_bootstrap_milestone(info.initramfs).is_err() {
                serial::put_str("[process] FATAL: ring3 bootstrap milestone failed\n");
                exit_kernel(0x50);
            }
            if !rustos_microkernel::boot_self_test() {
                serial::put_str("[scheduler] FATAL: lifecycle/policy self-test failed\n");
                exit_kernel(0x51);
            }
            serial::put_str(
                "[scheduler] priority, affinity and fault-containment policy verified\n",
            );
            serial::put_str("[microkernel] RING3_MILESTONE_OK\n");
            if cfg!(feature = "boot-test") {
                print_idle_notice();
                exit_kernel(0);
            }
            gui::session::run(info)
        }
        Err(code) => exit_kernel(code),
    }
}

/// Завершение работы ядра: код в QEMU isa-debug-exit, затем idle-цикл.
///
/// В VM без устройства запись в порт — no-op, и машина остаётся живой
/// (графический режим; позже здесь будет yield в scheduler — см.
/// docs/ARCHITECTURE.md, «Путь к микроядру»).
pub(crate) fn exit_kernel(code: u8) -> ! {
    serial::put_str("\n[boot] kernel test done, exit code=");
    serial::put_u32(code as u32);
    serial::put_str("\n");
    arch::debug_exit(code);
    loop {
        arch::halt();
    }
}

/// Banner запуска — первая строка, которую видит CI и пользователь.
pub fn print_banner() {
    serial::put_str("\n");
    serial::put_str("==================================================\n");
    serial::put_str("  RustOS 0.1.0 — educational microkernel (x86-64)\n");
    serial::put_str("==================================================\n");
}

/// Вывод сводки BootInfo в serial (диагностический канал).
pub fn print_bootinfo(info: &BootInfo) {
    serial::put_str("[boot] BootInfo v");
    serial::put_u32(info.version);
    serial::put_str(" ok\n");

    let ram = info.total_usable_ram();
    serial::put_str("[boot] usable RAM: ");
    serial::put_u32((ram / (1024 * 1024)) as u32);
    serial::put_str(" MiB\n");

    serial::put_str("[boot] memory map: ");
    serial::put_u32(info.memmap_count);
    serial::put_str(" regions\n");
    for i in 0..info.memmap_count as usize {
        let r = &info.memmap[i];
        let kind = match r.kind {
            k if k == MemRegionKind::Usable as u32 => "usable  ",
            k if k == MemRegionKind::Reserved as u32 => "reserved",
            k if k == MemRegionKind::AcpiReclaim as u32 => "acpi-rm ",
            k if k == MemRegionKind::AcpiNvs as u32 => "acpi-nvs",
            k if k == MemRegionKind::Mmio as u32 => "mmio    ",
            k if k == MemRegionKind::RuntimeServices as u32 => "runtime ",
            _ => "unknown ",
        };
        print_region(kind, r);
    }

    if info.framebuffer.phys_addr != 0 {
        serial::put_str("[boot] GOP framebuffer @ 0x");
        serial::put_hex(info.framebuffer.phys_addr);
        serial::put_str(" ");
        serial::put_u32(info.framebuffer.width);
        serial::put_str("x");
        serial::put_u32(info.framebuffer.height);
        serial::put_str(" bpp=");
        serial::put_u32(info.framebuffer.bpp);
        serial::put_str("\n");
    } else {
        serial::put_str("[boot] no GOP framebuffer (serial-only mode)\n");
    }

    if info.acpi_rsdp != 0 {
        serial::put_str("[boot] ACPI RSDP @ 0x");
        serial::put_hex(info.acpi_rsdp);
        serial::put_str("\n");
    } else {
        serial::put_str("[boot] WARNING: ACPI RSDP not found\n");
    }

    if info.initramfs.size > 0 {
        serial::put_str("[boot] initramfs: ");
        serial::put_u32((info.initramfs.size / 1024) as u32);
        serial::put_str(" KiB @ 0x");
        serial::put_hex(info.initramfs.phys_addr);
        serial::put_str("\n");
    }
}

/// Self-test: проверка целостности начального состояния после загрузки.
///
/// Возвращаемый код (если `Err`) передаётся в QEMU isa-debug-exit.
pub fn self_test(info: &BootInfo) -> Result<(), u8> {
    // 1. Магия и версия уже проверены validate(); дополнительно фиксируем
    //    значение в логе — упрощает диагностицию несовместимых builds.
    if info.magic != BOOT_INFO_MAGIC {
        return Err(0x10);
    }
    if info.version != BOOT_INFO_VERSION {
        return Err(0x11);
    }

    // 2. Должна быть хотя бы одна usable-область и она должна быть ненулевой.
    if info.total_usable_ram() == 0 {
        serial::put_str("[selftest] FAIL: no usable RAM in memory map\n");
        return Err(0x20);
    }

    // 3. ACPI RSDP: подпись "RSD PTR " по физическому адресу.
    //    (identity-маппинг: физический адрес доступен напрямую.)
    if info.acpi_rsdp != 0 {
        // RSDP не гарантирован 8-байтовым выравниванием (у OVMF встречается
        // 4-байтное: 0x..014), поэтому подпись читаем побайтно.
        // SAFETY: RSDP лежит в RAM, identity-маппинг гарантирует доступность
        // виртуального адреса == физического; 8 байт подписи в пределах RAM.
        const RSDP_SIG: [u8; 8] = *b"RSD PTR ";
        let p = info.acpi_rsdp as *const u8;
        let mut sig_ok = true;
        for (i, expected) in RSDP_SIG.iter().copied().enumerate() {
            // SAFETY: см. выше — `p.add(i)` в пределах 8 байт подписи.
            if unsafe { p.add(i).read_volatile() } != expected {
                sig_ok = false;
                break;
            }
        }
        if !sig_ok {
            serial::put_str("[selftest] FAIL: RSDP signature mismatch\n");
            return Err(0x30);
        }
    }

    // 4. Framebuffer: если заявлен — чтение первых 4 байт должно пройти
    //    (проверяет корректность identity-маппинга окна MMIO).
    if info.framebuffer.phys_addr != 0 {
        let fb = info.framebuffer.phys_addr as *const u32;
        // SAFETY: загрузчик отображает framebuffer в identity-карте
        // (rustos-boot, pagetable.rs), поэтому адрес доступен.
        let first = unsafe { fb.read_volatile() };
        serial::put_str("[selftest] framebuffer first pixel = 0x");
        serial::put_hex(first as u64);
        serial::put_str("\n");
    }

    // 5. Письмо в serial уже проверено всеми строками выше (если мы читаем
    //    эти строки — UART работает).
    Ok(())
}

/// Сообщение об idle-режиме (графическая VM без isa-debug-exit).
pub fn print_idle_notice() {
    serial::put_str("[boot] entering idle loop (APIC preemption not enabled yet)\n");
}

/// Вывод региона памяти в serial (без heap: по частям через `put_hex`).
fn print_region(kind: &str, r: &MemRegion) {
    serial::put_str(kind);
    serial::put_str(" [");
    serial::put_hex(r.phys_start);
    serial::put_str(" ");
    serial::put_hex(r.size);
    serial::put_str("]\n");
}

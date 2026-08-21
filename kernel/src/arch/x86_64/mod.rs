//! Полностью изолированный backend AMD64: privilege levels, traps, APIC,
//! page-table switch и legacy port I/O PC-платформы.
//!
//! Все `unsafe`-блоки документируются: ядро — единственный код с ring 0
//! привилегиями, и каждый доступ к порту/регистру осознан.
//! Инициализация не нужна (порты x86 работают «из коробки» в long mode).

// Часть функций (например, `outw` под ACPI PM) пока не используется —
// модуль расширяется по мере появления драйверов, поэтому allow на уровень модуля.
#![allow(dead_code)]

mod apic;
mod multiboot2;
mod segmentation;
mod smp;
mod traps;

use core::sync::atomic::{AtomicU64, Ordering};
use rustos_abi::BootInfo;

use super::{ArchError, EarlyInit, SchedulerHardware, SmpInfo};

pub use traps::TrapFrame;

/// Читаемое имя ISA для banner'ов и диагностических сообщений.
pub const ARCH_NAME: &str = "x86-64";
/// `#UD` — архитектурная причина тестового illegal instruction.
pub const ILLEGAL_INSTRUCTION_EXCEPTION: u16 = 6;

/// Частота invariant TSC калибруется APIC backend'ом. Значение 1 ГГц —
/// безопасный ранний fallback до scheduler milestone.
static MONOTONIC_HZ: AtomicU64 = AtomicU64::new(1_000_000_000);

/// Сохранённый пользовательский контекст. Его раскладка является деталью
/// AMD64 backend'а и не видна scheduler/process manager.
#[derive(Clone, Copy)]
pub struct UserContext {
    registers: [u64; 15],
    instruction_pointer: u64,
    flags: u64,
    stack_pointer: u64,
    thread_pointer: u64,
}

impl UserContext {
    pub fn initial(entry: u64, stack: u64, arguments: [u64; 3]) -> Self {
        let mut registers = [0; 15];
        // Порядок совпадает с TrapFrame: r15..r8, rdi, rsi, rbp,
        // rdx, rcx, rbx, rax.
        registers[8] = arguments[0];
        registers[9] = arguments[1];
        registers[11] = arguments[2];
        Self {
            registers,
            instruction_pointer: entry,
            flags: 0x202,
            stack_pointer: stack,
            thread_pointer: 0,
        }
    }

    pub fn entry(&self) -> u64 {
        self.instruction_pointer
    }

    pub fn stack_pointer(&self) -> u64 {
        self.stack_pointer
    }

    pub fn arguments(&self) -> [u64; 3] {
        [self.registers[8], self.registers[9], self.registers[11]]
    }

    pub const fn thread_pointer(&self) -> u64 {
        self.thread_pointer
    }

    pub fn set_thread_pointer(&mut self, address: u64) {
        self.thread_pointer = address;
    }

    pub fn save(&mut self, frame: &TrapFrame) {
        self.registers = frame.general_registers();
        self.instruction_pointer = frame.instruction_pointer();
        self.flags = frame.rflags;
        self.stack_pointer = frame.rsp;
    }

    pub fn restore(&self, frame: &mut TrapFrame) {
        frame.set_general_registers(self.registers);
        frame.rip = self.instruction_pointer;
        frame.cs = u64::from(segmentation::USER_CODE_SELECTOR);
        frame.rflags = self.flags | 0x202;
        frame.rsp = self.stack_pointer;
        frame.ss = u64::from(segmentation::USER_DATA_SELECTOR);
    }

    pub fn set_syscall_result(&mut self, result: i64) {
        self.registers[14] = result as u64;
    }
}

/// Устанавливает user FS base перед `iretq`. GS остаётся зарезервирован ядру
/// для будущих per-CPU данных и не является частью пользовательского TLS ABI.
pub fn set_user_thread_pointer(address: u64) {
    const IA32_FS_BASE: u32 = 0xc000_0100;
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_FS_BASE,
            in("eax") address as u32,
            in("edx") (address >> 32) as u32,
            options(nomem, nostack),
        );
    }
}

/// Читает invariant TSC, используемый clock syscall после калибровки APIC.
pub fn read_monotonic_counter() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Монотонное время для input gestures и GUI-анимаций.
pub fn monotonic_milliseconds() -> u64 {
    let frequency = MONOTONIC_HZ.load(Ordering::Acquire).max(1);
    let ticks = read_monotonic_counter();
    ticks / frequency * 1_000 + (ticks % frequency) * 1_000 / frequency
}

/// Настраивает GDT/TSS/IDT и аппаратную защиту страниц до первого ring 3.
pub fn initialize_early(_info: &BootInfo) -> Result<EarlyInit, ArchError> {
    enable_memory_protection();
    let kernel_stack_top = segmentation::initialize();
    traps::initialize();
    Ok(EarlyInit {
        kernel_stack_top,
        exception_backend: "GDT/TSS/IDT",
    })
}

pub fn initialize_scheduler_hardware() -> Result<SchedulerHardware, ArchError> {
    let info = apic::initialize_local().map_err(|_| ArchError::InterruptController)?;
    MONOTONIC_HZ.store(info.tsc_hz.max(1), Ordering::Release);
    super::set_counter_frequency(info.tsc_hz.max(1));
    Ok(SchedulerHardware {
        boot_cpu_id: info.id,
        counter_hz: info.tsc_hz,
        interrupt_controller: if info.uses_x2apic { "x2APIC" } else { "xAPIC" },
        timer: if info.uses_tsc_deadline {
            "tsc-deadline"
        } else {
            "periodic"
        },
    })
}

pub fn start_secondary_cpus(info: &BootInfo, counter_hz: u64) -> Result<SmpInfo, ArchError> {
    if info.firmware.kind != rustos_abi::bootinfo::BOOT_FIRMWARE_ACPI {
        return Err(ArchError::FirmwareDescription);
    }
    let report = smp::start_application_processors(info.firmware.root, counter_hz)
        .map_err(|_| ArchError::SecondaryCpuStartup)?;
    Ok(SmpInfo {
        online_cpus: report.online_cpus,
        discovered_cpus: report.discovered_cpus,
        discovery: "ACPI MADT",
    })
}

pub fn start_scheduler_timer(counter_hz: u64) {
    apic::start_timer(counter_hz);
}

pub fn stop_scheduler_timer() {
    apic::stop_timer();
}

pub fn rearm_scheduler_timer(counter_hz: u64) {
    apic::rearm_timer(counter_hz);
}

pub fn end_of_interrupt() {
    apic::end_of_interrupt();
}

pub fn current_address_space_root() -> u64 {
    read_cr3()
}

/// # Safety
///
/// `root` обязан быть физическим адресом валидной PML4 с kernel mappings.
pub unsafe fn switch_address_space(root: u64) {
    unsafe { write_cr3(root) };
}

pub fn set_user_run_result(result: u64) {
    traps::set_user_result(result);
}

/// # Safety
///
/// Все адреса и таблица страниц принадлежат подготовленному процессу.
pub unsafe fn enter_user(
    entry: u64,
    stack: u64,
    arguments: [u64; 3],
    root: u64,
    interrupts: bool,
) -> u64 {
    unsafe { traps::enter_user(entry, stack, arguments, root, interrupts) }
}

pub const fn initial_user_stack(top: u64) -> u64 {
    // System V AMD64 имитирует состояние после `call`.
    top - 8
}

pub fn power_off() -> ! {
    // Стандартный ACPI PM control порт QEMU/OVMF.
    unsafe { outw(0x604, 0x2000) };
    loop {
        halt();
    }
}

/// Остановка текущего CPU до следующего прерывания.
///
/// # Safety
///
/// HLT требует ring 0: функция вызывается только из ядра.
pub fn halt() {
    unsafe {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

/// Текущий PML4 physical address (CR3 без PCID bits).
pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack)) };
    value & 0x000f_ffff_ffff_f000
}

/// Переключает address space текущего CPU.
///
/// # Safety
///
/// `root` должен быть physical address валидного PML4, содержащего mappings
/// исполняемого kernel-кода и текущего стека.
pub unsafe fn write_cr3(root: u64) {
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack)) };
}

/// Адрес последнего page fault.
pub fn read_cr2() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr2", out(reg) value, options(nomem, nostack)) };
    value
}

/// Включает NX pages (EFER.NXE) и защиту read-only supervisor pages (CR0.WP).
pub fn enable_memory_protection() {
    const EFER: u32 = 0xC000_0080;
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") EFER,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        );
        let value = (u64::from(high) << 32) | u64::from(low) | (1 << 11);
        core::arch::asm!(
            "wrmsr",
            in("ecx") EFER,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack),
        );
        let mut cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 |= 1 << 16;
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nostack));
    }
}

/// Завершение VM через QEMU isa-debug-exit: запись 32-битного значения в
/// IO-порт 0xF4 заставляет QEMU выйти с этим значением как кодом возврата.
/// В системе без устройства запись игнорируется (безопасный no-op).
pub fn debug_exit(code: u8) {
    // SAFETY: 0xF4 — отдельный тестовый порт QEMU; запись u32 допустима.
    unsafe {
        // Intel-синтаксис (default в rustc asm!): явные регистры пишутся
        // в шаблон напрямую, без плейсхолдеров. Операнды явных регистров
        // нужны, чтобы аллокатор узнал об их занятости.
        core::arch::asm!(
            "out dx, eax",
            in("eax") (code as u32),
            in("dx") 0xF4u16,
            options(nomem, nostack)
        );
    }
}

/// Чтение 8-битного значения из IO-порта (для serial и ранней диагностики).
///
/// # Safety
///
/// Порт выбирает вызывающий; в ring 0 чтение любого порта выполнимо,
/// но эффект на оборудование зависит от порта — вызывающий отвечает за это.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: порт выбирает вызывающий (контракт функции);
    // в ring 0 чтение любого порта выполнимо.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") val,
            in("dx") port,
            options(nomem, nostack)
        );
    }
    val
}

/// Чтение 16-битного I/O-регистра PCI/virtio устройства.
#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    unsafe {
        core::arch::asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack))
    };
    value
}

/// Чтение 32-битного I/O-регистра PCI/virtio устройства.
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack))
    };
    value
}

/// Запись 8-битного значения в IO-порт.
///
/// # Safety
///
/// См. [`inb`]: вызывающий отвечает за корректность порта.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    // SAFETY: см. [`inb`]: вызывающий отвечает за корректность порта.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("al") val,
            in("dx") port,
            options(nomem, nostack)
        );
    }
}

/// Запись 16-битного значения в I/O-порт (ACPI PM control и драйверы).
///
/// # Safety
///
/// Вызывающий обязан выбрать устройство и допустимое для него значение.
#[inline]
pub unsafe fn outw(port: u16, val: u16) {
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("ax") val,
            in("dx") port,
            options(nomem, nostack)
        );
    }
}

/// Запись 32-битного I/O-регистра PCI/virtio устройства.
#[inline]
pub unsafe fn outl(port: u16, val: u32) {
    unsafe {
        core::arch::asm!("out dx, eax", in("eax") val, in("dx") port, options(nomem, nostack))
    };
}

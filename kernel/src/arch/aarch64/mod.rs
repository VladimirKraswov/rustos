//! AArch64 porting backend.
//!
//! Здесь намеренно нет Raspberry Pi- или phone-specific адресов. CPU-механика
//! (EL0/EL1, TTBR0, ESR/FAR, `svc`, Generic Timer) общая для ARMv8-A, а GIC,
//! UART, framebuffer и способ запуска CPU описываются firmware (DT/ACPI) и
//! будут подключаться platform-драйверами. Благодаря этому ядро не станет
//! «ядром только для одной Raspberry Pi».

mod gic;
mod smp;
mod timer;
mod traps;

use core::arch::asm;

use rustos_abi::BootInfo;

use super::{ArchError, EarlyInit, SchedulerHardware, SmpInfo, TrapKind};

pub const ARCH_NAME: &str = "AArch64";
/// Exception Class `BRK instruction execution in AArch64 state`.
pub const ILLEGAL_INSTRUCTION_EXCEPTION: u16 = 0x3c;

const ESR_EC_SHIFT: u64 = 26;
const ESR_EC_MASK: u64 = 0x3f;
const ESR_EC_SVC64: u16 = 0x15;
const TRAP_SYNC: u64 = 0;
const TRAP_IRQ: u64 = 1;
const TRAP_SPURIOUS: u64 = 2;

/// Публикует записи CPU внешнему DMA-устройству.
///
/// Rust atomic fences синхронизируют обычные CPU observers, но virtqueue
/// разделяется ещё и с устройством за пределами inner-shareable domain.
/// Поэтому AArch64-драйверу нужен именно outer-shareable barrier перед
/// публикацией индекса и перед MMIO doorbell.
#[inline]
pub fn dma_write_barrier() {
    // SAFETY: `dmb oshst` не обращается к памяти сам и допустим на EL1. Без
    // `nomem` asm одновременно служит compiler barrier для окружающих DMA
    // буферов.
    unsafe { asm!("dmb oshst", options(nostack, preserves_flags)) }
}

/// Запрещает читать DMA-результат раньше замеченного device-owned индекса.
#[inline]
pub fn dma_read_barrier() {
    // SAFETY: `dmb oshld` — архитектурный load barrier для outer-shareable
    // domain; инструкция допустима на EL1 и не меняет регистры процесса.
    unsafe { asm!("dmb oshld", options(nostack, preserves_flags)) }
}

/// Кадр, который vector stub сохраняет при входе EL0 -> EL1.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr_el1: u64,
    pub spsr_el1: u64,
    pub esr_el1: u64,
    pub far_el1: u64,
    pub source: u64,
    /// Явный padding сохраняет 16-байтное выравнивание Q-регистров.
    pub simd_padding: u64,
    /// Полный Advanced SIMD/FP context q0..q31, сохранённый до входа в Rust.
    pub vector: [[u64; 2]; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}

const _: () = assert!(core::mem::size_of::<TrapFrame>() == 832);
const _: () = assert!(core::mem::offset_of!(TrapFrame, vector) == 304);
const _: () = assert!(core::mem::offset_of!(TrapFrame, fpcr) == 816);
const _: () = assert!(core::mem::offset_of!(TrapFrame, fpsr) == 824);

impl TrapFrame {
    pub const fn is_from_user(&self) -> bool {
        // M[3:0] == 0 — EL0t.
        self.spsr_el1 & 0xf == 0
    }

    pub fn kind(&self) -> TrapKind {
        if self.source == TRAP_SPURIOUS {
            return TrapKind::Spurious;
        }
        if self.source == TRAP_IRQ {
            return TrapKind::Timer;
        }
        let exception_class = ((self.esr_el1 >> ESR_EC_SHIFT) & ESR_EC_MASK) as u16;
        if self.source == TRAP_SYNC && exception_class == ESR_EC_SVC64 {
            TrapKind::Syscall
        } else {
            TrapKind::Exception {
                number: exception_class,
                code: self.esr_el1 as u16,
                instruction_pointer: self.elr_el1,
                fault_address: self.far_el1,
            }
        }
    }

    pub const fn instruction_pointer(&self) -> u64 {
        self.elr_el1
    }

    pub const fn syscall_number(&self) -> u64 {
        self.x[8]
    }

    pub const fn syscall_arguments(&self) -> [u64; 3] {
        [self.x[0], self.x[1], self.x[2]]
    }

    pub fn set_syscall_result(&mut self, result: i64) {
        self.x[0] = result as u64;
    }
}

#[derive(Clone, Copy)]
pub struct UserContext {
    x: [u64; 31],
    vector: [[u64; 2]; 32],
    stack_pointer: u64,
    instruction_pointer: u64,
    processor_state: u64,
    thread_pointer: u64,
    fpcr: u64,
    fpsr: u64,
}

impl UserContext {
    pub fn initial(entry: u64, stack: u64, arguments: [u64; 3]) -> Self {
        let mut x = [0; 31];
        x[..3].copy_from_slice(&arguments);
        Self {
            x,
            vector: [[0; 2]; 32],
            stack_pointer: stack,
            instruction_pointer: entry,
            // EL0t, interrupts unmasked when `eret` executes.
            processor_state: 0,
            thread_pointer: 0,
            fpcr: 0,
            fpsr: 0,
        }
    }

    pub const fn thread_pointer(&self) -> u64 {
        self.thread_pointer
    }

    pub fn set_thread_pointer(&mut self, address: u64) {
        self.thread_pointer = address;
    }

    pub fn save(&mut self, frame: &TrapFrame) {
        self.x = frame.x;
        self.vector = frame.vector;
        self.stack_pointer = frame.sp_el0;
        self.instruction_pointer = frame.elr_el1;
        self.processor_state = frame.spsr_el1;
        self.fpcr = frame.fpcr;
        self.fpsr = frame.fpsr;
    }

    pub fn restore(&self, frame: &mut TrapFrame) {
        frame.x = self.x;
        frame.vector = self.vector;
        frame.sp_el0 = self.stack_pointer;
        frame.elr_el1 = self.instruction_pointer;
        frame.spsr_el1 = self.processor_state;
        frame.fpcr = self.fpcr;
        frame.fpsr = self.fpsr;
    }

    pub fn set_syscall_result(&mut self, result: i64) {
        self.x[0] = result as u64;
    }
}

pub fn set_user_thread_pointer(address: u64) {
    unsafe {
        asm!(
            "msr tpidr_el0, {value}",
            "isb",
            value = in(reg) address,
            options(nostack),
        );
    }
}

pub fn read_monotonic_counter() -> u64 {
    let value: u64;
    unsafe {
        asm!(
            "mrs {value}, cntvct_el0",
            value = out(reg) value,
            options(nomem, nostack),
        );
    }
    value
}

/// Монотонное время на architected Generic Timer, без platform MMIO.
pub fn monotonic_milliseconds() -> u64 {
    let frequency: u64;
    unsafe {
        asm!(
            "mrs {value}, cntfrq_el0",
            value = out(reg) frequency,
            options(nomem, nostack),
        );
    }
    let frequency = frequency.max(1);
    let ticks = read_monotonic_counter();
    ticks / frequency * 1_000 + (ticks % frequency) * 1_000 / frequency
}

/// Настраивает VBAR_EL1 и возвращает вершину kernel stack.
pub fn initialize_early(info: &BootInfo) -> Result<EarlyInit, ArchError> {
    // Rust/LLVM вправе использовать обязательный для AArch64 Advanced SIMD
    // даже для целочисленных memcpy. Разрешаем FP/SIMD на EL0 и EL1 до
    // установки vectors: SAVE_FRAME ниже использует q0..q31 немедленно.
    unsafe {
        let mut control: u64;
        asm!(
            "mrs {control}, cpacr_el1",
            control = out(reg) control,
            options(nomem, nostack),
        );
        control |= 0b11 << 20;
        asm!(
            "msr cpacr_el1, {control}",
            "dsb sy",
            "isb",
            control = in(reg) control,
            options(nostack, preserves_flags),
        );
    }
    traps::initialize();
    // EL0VCTEN=1: пользовательский runtime читает architected virtual
    // counter (`cntvct_el0`) без дорогого syscall на каждом обращении к
    // монотонным часам. Доступ к programming registers timer остаётся EL1.
    unsafe {
        let mut control: u64;
        asm!(
            "mrs {control}, cntkctl_el1",
            control = out(reg) control,
            options(nomem, nostack),
        );
        control |= 1 << 1;
        asm!(
            "msr cntkctl_el1, {control}",
            "isb",
            control = in(reg) control,
            options(nomem, nostack),
        );
    }
    Ok(EarlyInit {
        kernel_stack_top: info.boot_stack.top,
        exception_backend: "EL1/VBAR",
    })
}

/// Инициализация GICv3 + Generic Timer.
pub fn initialize_scheduler_hardware() -> Result<SchedulerHardware, ArchError> {
    gic::initialize().map_err(|_| ArchError::InterruptController)?;
    timer::initialize();
    let counter_hz = timer::counter_frequency().max(1);
    super::set_counter_frequency(counter_hz);
    Ok(SchedulerHardware {
        boot_cpu_id: 0,
        counter_hz,
        interrupt_controller: "GICv3",
        timer: "generic-one-shot",
    })
}

/// Запускает все объявленные Device Tree CPU через PSCI и ждёт их реального
/// подтверждения. AP пока безопасно припаркованы до per-CPU scheduler.
pub fn start_secondary_cpus(info: &BootInfo, counter_hz: u64) -> Result<SmpInfo, ArchError> {
    if info.firmware.kind != rustos_abi::bootinfo::BOOT_FIRMWARE_DEVICE_TREE
        || info.firmware.root == 0
    {
        return Err(ArchError::FirmwareDescription);
    }
    let report = smp::start_application_processors(info.firmware.root, counter_hz)
        .map_err(|_| ArchError::SecondaryCpuStartup)?;
    Ok(SmpInfo {
        online_cpus: report.online_cpus,
        discovered_cpus: report.discovered_cpus,
        discovery: "Device Tree + PSCI",
    })
}

pub fn start_scheduler_timer(counter_hz: u64) {
    timer::start(counter_hz);
}

pub fn stop_scheduler_timer() {
    timer::stop();
}

pub fn rearm_scheduler_timer(counter_hz: u64) {
    let period = counter_hz.div_ceil(1000).max(1);
    timer::rearm(period);
}

pub fn end_of_interrupt() {
    // GIC EOI уже выполнен в traps.rs перед return.
}

pub fn current_address_space_root() -> u64 {
    let value: u64;
    unsafe {
        asm!(
            "mrs {value}, ttbr0_el1",
            value = out(reg) value,
            options(nomem, nostack),
        );
    }
    value & 0x0000_ffff_ffff_f000
}

/// # Safety
///
/// `root` должен описывать валидные translation tables текущей конфигурации
/// TCR_EL1 и сохранять kernel mappings.
pub unsafe fn switch_address_space(root: u64) {
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr0_el1, {root}",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            root = in(reg) root,
            options(nostack),
        );
    }
}

pub fn set_user_run_result(result: u64) {
    traps::set_user_result(result);
}

/// Вход в user mode: ERET в EL0t.
///
/// # Safety
///
/// `entry`, `stack` и `arguments` принадлежат подготовленному процессу.
pub unsafe fn enter_user(
    entry: u64,
    stack: u64,
    arguments: [u64; 3],
    root: u64,
    interrupts: bool,
) -> u64 {
    unsafe { traps::enter_user(entry, stack, arguments, root, interrupts) }
}

/// Возобновляет полный сохранённый EL0 context после scheduler stop/run.
///
/// В отличие от начального [`enter_user`], эта граница восстанавливает все
/// GPR, FP/SIMD, flags и TLS. Терять callee-saved регистры между двумя
/// `ProcessManager::run` недопустимо для постоянных ring-3 сервисов.
///
/// # Safety
///
/// `context` принадлежит выбранному runnable thread, а `root` — его
/// действующая таблица страниц с общими supervisor mappings.
pub unsafe fn enter_user_context(context: &UserContext, root: u64) -> u64 {
    let mut frame = TrapFrame {
        x: [0; 31],
        sp_el0: 0,
        elr_el1: 0,
        spsr_el1: 0,
        esr_el1: 0,
        far_el1: 0,
        source: 0,
        simd_padding: 0,
        vector: [[0; 2]; 32],
        fpcr: 0,
        fpsr: 0,
    };
    context.restore(&mut frame);
    unsafe { traps::enter_user_frame(&mut frame, root) }
}

pub const fn initial_user_stack(top: u64) -> u64 {
    // AAPCS64 требует SP % 16 == 0 на публичной границе.
    top & !15
}

pub fn halt() {
    unsafe {
        asm!("wfi", options(nomem, nostack));
    }
}

/// Завершение VM: PSCI SYSTEM_OFF через HVC conduit QEMU `virt`.
pub fn debug_exit(_code: u8) {
    // В эталонной QEMU-платформе PSCI method = "hvc". SMC из
    // non-secure EL1 здесь порождает synchronous exception.
    unsafe {
        asm!(
            "mov w0, #0x0008",
            "movk w0, #0x8400, lsl #16",
            "hvc #0",
            options(nomem, nostack),
        );
    }
    loop {
        halt();
    }
}

/// Публикует записанный kernel'ом диапазон для последующего исполнения.
///
/// AArch64 не обещает автоматическую когерентность D-cache и I-cache. Сначала
/// очищаем каждую data line до point of unification, затем инвалидируем
/// соответствующие instruction lines. Размеры линий берутся из CTR_EL0, а не
/// зашиваются под QEMU/Apple Silicon.
pub fn synchronize_executable_memory(address: u64, length: usize) {
    if length == 0 {
        return;
    }
    let mut ctr: u64;
    unsafe {
        asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr, options(nomem, nostack));
    }
    let data_line = 4u64 << ((ctr >> 16) & 0xf);
    let instruction_line = 4u64 << (ctr & 0xf);
    let end = address.saturating_add(length as u64);
    let mut cursor = address & !(data_line - 1);
    while cursor < end {
        unsafe { asm!("dc cvau, {line}", line = in(reg) cursor, options(nostack)) };
        cursor = cursor.saturating_add(data_line);
    }
    unsafe { asm!("dsb ish", options(nostack)) };
    cursor = address & !(instruction_line - 1);
    while cursor < end {
        unsafe { asm!("ic ivau, {line}", line = in(reg) cursor, options(nostack)) };
        cursor = cursor.saturating_add(instruction_line);
    }
    unsafe {
        asm!("dsb ish", "isb", options(nostack));
    }
}

pub fn power_off() -> ! {
    debug_exit(0);
    loop {
        halt();
    }
}

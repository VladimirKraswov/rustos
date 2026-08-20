//! AArch64 porting backend.
//!
//! Здесь намеренно нет Raspberry Pi- или phone-specific адресов. CPU-механика
//! (EL0/EL1, TTBR0, ESR/FAR, `svc`, Generic Timer) общая для ARMv8-A, а GIC,
//! UART, framebuffer и способ запуска CPU описываются firmware (DT/ACPI) и
//! будут подключаться platform-драйверами. Благодаря этому ядро не станет
//! «ядром только для одной Raspberry Pi».

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

/// Кадр, который будущий vector stub сохраняет при входе EL0 -> EL1.
///
/// Массив `x` сохраняет x0..x30 без предположений process manager о роли
/// отдельных регистров. `source` отделяет synchronous exception от IRQ.
#[repr(C)]
#[derive(Debug)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr_el1: u64,
    pub spsr_el1: u64,
    pub esr_el1: u64,
    pub far_el1: u64,
    pub source: u64,
}

impl TrapFrame {
    pub const fn is_from_user(&self) -> bool {
        // M[3:0] == 0 — EL0t.
        self.spsr_el1 & 0xf == 0
    }

    pub fn kind(&self) -> TrapKind {
        if self.source == TRAP_IRQ {
            // После подключения GIC backend уточнит ID и сможет отличать
            // timer/spurious. До этого единственный разрешённый IRQ — timer.
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
        // RustOS AArch64 ABI следует обычной конвенции: x8 — номер, x0..x2 — args.
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
    stack_pointer: u64,
    instruction_pointer: u64,
    processor_state: u64,
}

impl UserContext {
    pub fn initial(entry: u64, stack: u64, arguments: [u64; 3]) -> Self {
        let mut x = [0; 31];
        x[..3].copy_from_slice(&arguments);
        Self {
            x,
            stack_pointer: stack,
            instruction_pointer: entry,
            // EL0t, interrupts unmasked when `eret` executes.
            processor_state: 0,
        }
    }

    pub const fn entry(&self) -> u64 {
        self.instruction_pointer
    }

    pub const fn stack_pointer(&self) -> u64 {
        self.stack_pointer
    }

    pub const fn arguments(&self) -> [u64; 3] {
        [self.x[0], self.x[1], self.x[2]]
    }

    pub fn save(&mut self, frame: &TrapFrame) {
        self.x = frame.x;
        self.stack_pointer = frame.sp_el0;
        self.instruction_pointer = frame.elr_el1;
        self.processor_state = frame.spsr_el1;
    }

    pub fn restore(&self, frame: &mut TrapFrame) {
        frame.x = self.x;
        frame.sp_el0 = self.stack_pointer;
        frame.elr_el1 = self.instruction_pointer;
        frame.spsr_el1 = self.processor_state;
    }

    pub fn set_syscall_result(&mut self, result: i64) {
        self.x[0] = result as u64;
    }
}

pub fn initialize_early(info: &BootInfo) -> Result<EarlyInit, ArchError> {
    // Синхронизируем записи загрузчика перед будущей установкой VBAR_EL1.
    unsafe { asm!("dsb sy", "isb", options(nostack, preserves_flags)) };
    Ok(EarlyInit {
        kernel_stack_top: info.boot_stack.top,
        exception_backend: "EL1/VBAR porting backend",
    })
}

pub fn initialize_scheduler_hardware() -> Result<SchedulerHardware, ArchError> {
    // Частота architected counter уже доступна без platform-specific адресов.
    let counter_hz: u64;
    unsafe {
        asm!("mrs {value}, cntfrq_el0", value = out(reg) counter_hz, options(nomem, nostack))
    };
    let _ = counter_hz;
    // Разрешать timer IRQ до обнаружения/настройки GIC нельзя.
    Err(ArchError::InterruptController)
}

pub fn start_secondary_cpus(_info: &BootInfo, _counter_hz: u64) -> Result<SmpInfo, ArchError> {
    // PSCI CPU_ON требует MPIDR из DT/ACPI и выбранный firmware conduit.
    Err(ArchError::FirmwareDescription)
}

pub fn start_scheduler_timer(_counter_hz: u64) {}
pub fn stop_scheduler_timer() {}
pub fn rearm_scheduler_timer(_counter_hz: u64) {}
pub fn end_of_interrupt() {}

pub fn current_address_space_root() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {value}, ttbr0_el1", value = out(reg) value, options(nomem, nostack)) };
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

static mut USER_RUN_RESULT: u64 = 0;

pub fn set_user_run_result(result: u64) {
    unsafe { USER_RUN_RESULT = result };
}

/// Временная безопасная граница porting target: до установки VBAR/GIC вход
/// в EL0 запрещён и CPU остаётся в low-power wait вместо выполнения с неверной
/// таблицей исключений. Реальный `eret` stub будет единственным ASM-файлом
/// следующего ARM milestone.
pub unsafe fn enter_user(
    _entry: u64,
    _stack: u64,
    _arguments: [u64; 3],
    _root: u64,
    _interrupts: bool,
) -> u64 {
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

pub const fn initial_user_stack(top: u64) -> u64 {
    // AAPCS64 требует SP % 16 == 0 на публичной границе.
    top & !15
}

pub fn halt() {
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

pub fn debug_exit(_code: u8) {}

pub fn power_off() -> ! {
    // PSCI SYSTEM_OFF, HVC conduit (QEMU `virt` и многие гипервизоры).
    // Реальный platform layer выберет HVC/SMC по DT `method`.
    unsafe {
        asm!(
            "hvc #0",
            in("x0") 0x8400_0008u64,
            options(nostack),
        );
    }
    loop {
        halt();
    }
}

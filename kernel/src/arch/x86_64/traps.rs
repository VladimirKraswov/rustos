//! IDT и общий assembly trap frame для exceptions/syscalls.

use core::{
    arch::{asm, global_asm},
    mem::size_of,
    ptr::addr_of_mut,
};

use super::{
    apic::{SPURIOUS_VECTOR, TIMER_VECTOR},
    segmentation::{KERNEL_CODE_SELECTOR, USER_CODE_SELECTOR, USER_DATA_SELECTOR},
};

#[repr(C)]
#[derive(Debug)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    /// Сохраняются CPU только при переходе CPL3 -> CPL0.
    pub rsp: u64,
    /// User stack selector; существует в frame только для user trap.
    pub ss: u64,
}

impl TrapFrame {
    pub const fn is_from_user(&self) -> bool {
        self.cs & 3 == 3
    }

    pub fn kind(&self) -> crate::arch::TrapKind {
        if self.vector == u64::from(SPURIOUS_VECTOR) {
            crate::arch::TrapKind::Spurious
        } else if self.vector == u64::from(TIMER_VECTOR) {
            crate::arch::TrapKind::Timer
        } else if self.vector == u64::from(rustos_abi::syscall::INTERRUPT_VECTOR) {
            crate::arch::TrapKind::Syscall
        } else {
            crate::arch::TrapKind::Exception {
                number: self.vector as u16,
                code: self.error_code as u16,
                instruction_pointer: self.rip,
                fault_address: if self.vector == 14 {
                    super::read_cr2()
                } else {
                    self.rip
                },
            }
        }
    }

    pub const fn instruction_pointer(&self) -> u64 {
        self.rip
    }

    pub const fn syscall_number(&self) -> u64 {
        self.rax
    }

    pub const fn syscall_arguments(&self) -> [u64; 3] {
        [self.rdi, self.rsi, self.rdx]
    }

    pub fn set_syscall_result(&mut self, result: i64) {
        self.rax = result as u64;
    }

    pub(super) const fn general_registers(&self) -> [u64; 15] {
        [
            self.r15, self.r14, self.r13, self.r12, self.r11, self.r10, self.r9, self.r8, self.rdi,
            self.rsi, self.rbp, self.rdx, self.rcx, self.rbx, self.rax,
        ]
    }

    pub(super) fn set_general_registers(&mut self, registers: [u64; 15]) {
        self.r15 = registers[0];
        self.r14 = registers[1];
        self.r13 = registers[2];
        self.r12 = registers[3];
        self.r11 = registers[4];
        self.r10 = registers[5];
        self.r9 = registers[6];
        self.r8 = registers[7];
        self.rdi = registers[8];
        self.rsi = registers[9];
        self.rbp = registers[10];
        self.rdx = registers[11];
        self.rcx = registers[12];
        self.rbx = registers[13];
        self.rax = registers[14];
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const MISSING: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    };

    fn interrupt(handler: usize, dpl: u8, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: KERNEL_CODE_SELECTOR,
            ist: ist & 0x7,
            attributes: 0x8e | ((dpl & 3) << 5),
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::MISSING; 256];

extern "C" {
    fn rustos_vector_0();
    fn rustos_vector_1();
    fn rustos_vector_2();
    fn rustos_vector_3();
    fn rustos_vector_4();
    fn rustos_vector_5();
    fn rustos_vector_6();
    fn rustos_vector_7();
    fn rustos_vector_8();
    fn rustos_vector_9();
    fn rustos_vector_10();
    fn rustos_vector_11();
    fn rustos_vector_12();
    fn rustos_vector_13();
    fn rustos_vector_14();
    fn rustos_vector_15();
    fn rustos_vector_16();
    fn rustos_vector_17();
    fn rustos_vector_18();
    fn rustos_vector_19();
    fn rustos_vector_20();
    fn rustos_vector_21();
    fn rustos_vector_22();
    fn rustos_vector_23();
    fn rustos_vector_24();
    fn rustos_vector_25();
    fn rustos_vector_26();
    fn rustos_vector_27();
    fn rustos_vector_28();
    fn rustos_vector_29();
    fn rustos_vector_30();
    fn rustos_vector_31();
    fn rustos_vector_64();
    fn rustos_vector_128();
    fn rustos_vector_255();
    fn rustos_enter_user_asm(
        entry: u64,
        stack: u64,
        arg0: u64,
        arg1: u64,
        arg2: u64,
        root: u64,
        interrupts: u64,
    ) -> u64;
}

/// Устанавливает exception gates и доступный из CPL3 syscall gate 0x80.
pub fn initialize() {
    let handlers: [unsafe extern "C" fn(); 32] = [
        rustos_vector_0,
        rustos_vector_1,
        rustos_vector_2,
        rustos_vector_3,
        rustos_vector_4,
        rustos_vector_5,
        rustos_vector_6,
        rustos_vector_7,
        rustos_vector_8,
        rustos_vector_9,
        rustos_vector_10,
        rustos_vector_11,
        rustos_vector_12,
        rustos_vector_13,
        rustos_vector_14,
        rustos_vector_15,
        rustos_vector_16,
        rustos_vector_17,
        rustos_vector_18,
        rustos_vector_19,
        rustos_vector_20,
        rustos_vector_21,
        rustos_vector_22,
        rustos_vector_23,
        rustos_vector_24,
        rustos_vector_25,
        rustos_vector_26,
        rustos_vector_27,
        rustos_vector_28,
        rustos_vector_29,
        rustos_vector_30,
        rustos_vector_31,
    ];
    unsafe {
        for (vector, handler) in handlers.iter().enumerate() {
            let dpl = if vector == 3 { 3 } else { 0 };
            let ist = if vector == 8 { 1 } else { 0 };
            IDT[vector] = IdtEntry::interrupt(*handler as usize, dpl, ist);
        }
        IDT[128] = IdtEntry::interrupt(rustos_vector_128 as *const () as usize, 3, 0);
        IDT[TIMER_VECTOR as usize] =
            IdtEntry::interrupt(rustos_vector_64 as *const () as usize, 0, 0);
        IDT[SPURIOUS_VECTOR as usize] =
            IdtEntry::interrupt(rustos_vector_255 as *const () as usize, 0, 0);
        let pointer = IdtPointer {
            limit: (size_of::<[IdtEntry; 256]>() - 1) as u16,
            base: addr_of_mut!(IDT) as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }
}

/// Синхронно запускает первое user context. Возвращается только через
/// syscall exit либо user exception; kernel stack восстанавливает assembly.
pub unsafe fn enter_user(
    entry: u64,
    stack: u64,
    arguments: [u64; 3],
    root: u64,
    interrupts: bool,
) -> u64 {
    unsafe {
        rustos_enter_user_asm(
            entry,
            stack,
            arguments[0],
            arguments[1],
            arguments[2],
            root,
            u64::from(interrupts),
        )
    }
}

#[no_mangle]
static mut rustos_user_result: u64 = 0;

pub fn set_user_result(result: u64) {
    unsafe { rustos_user_result = result };
}

global_asm!(
    r#"
.macro NOERR number
.global rustos_vector_\number
rustos_vector_\number:
    push 0
    push \number
    jmp rustos_trap_common
.endm

.macro ERROR number
.global rustos_vector_\number
rustos_vector_\number:
    push \number
    jmp rustos_trap_common
.endm

NOERR 0
NOERR 1
NOERR 2
NOERR 3
NOERR 4
NOERR 5
NOERR 6
NOERR 7
ERROR 8
NOERR 9
ERROR 10
ERROR 11
ERROR 12
ERROR 13
ERROR 14
NOERR 15
NOERR 16
ERROR 17
NOERR 18
NOERR 19
NOERR 20
ERROR 21
NOERR 22
NOERR 23
NOERR 24
NOERR 25
NOERR 26
NOERR 27
NOERR 28
ERROR 29
ERROR 30
NOERR 31
NOERR 64
NOERR 128
NOERR 255

.global rustos_trap_common
rustos_trap_common:
    cld
    push rax
    push rbx
    push rcx
    push rdx
    push rbp
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp
    call rustos_handle_trap
    test rax, rax
    jnz rustos_abort_user
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rbp
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    iretq

.bss
.align 8
rustos_saved_kernel_rsp:
    .quad 0
rustos_saved_kernel_cr3:
    .quad 0
.text

.global rustos_enter_user_asm
rustos_enter_user_asm:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov qword ptr [rip + rustos_saved_kernel_rsp], rsp
    mov rax, cr3
    mov qword ptr [rip + rustos_saved_kernel_cr3], rax
    mov r10, qword ptr [rsp + 56]
    mov cr3, r9
    mov ax, {user_data}
    mov ds, ax
    mov es, ax
    push {user_data}
    push rsi
    pushfq
    pop rax
    test r10, r10
    jz 3f
    or rax, 512
    jmp 4f
3:
    and rax, -513
4:
    push rax
    push {user_code}
    push rdi
    mov rdi, rdx
    mov rsi, rcx
    mov rdx, r8
    iretq

rustos_abort_user:
    mov rax, qword ptr [rip + rustos_saved_kernel_cr3]
    mov cr3, rax
    mov ax, {kernel_data}
    mov ds, ax
    mov es, ax
    mov rsp, qword ptr [rip + rustos_saved_kernel_rsp]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    mov rax, qword ptr [rip + rustos_user_result]
    ret
"#,
    user_data = const USER_DATA_SELECTOR,
    user_code = const USER_CODE_SELECTOR,
    kernel_data = const super::segmentation::KERNEL_DATA_SELECTOR,
);

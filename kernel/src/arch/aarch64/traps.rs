//! AArch64 exception vectors, полный `TrapFrame` и синхронный EL0 runner.
//!
//! CPU сам не сохраняет general-purpose registers. Каждый рабочий vector
//! поэтому сначала выделяет 832 байта, сохраняет x0..x30, q0..q31,
//! FPCR/FPSR и системные регистры и лишь затем вызывает переносимый
//! `rustos_handle_trap`. Без SIMD-состояния вытесняемые Rust-процессы
//! повреждали бы регистры друг друга даже при обычном `memcpy`.
//!
//! Синхронный bootstrap runner устроен так же, как AMD64 backend: перед
//! `eret` он запоминает kernel SP/TTBR0. Нормальный syscall возвращается в
//! EL0, а process exit или user fault восстанавливает kernel address space и
//! возвращается обычным Rust-вызовом из `enter_user`. Падение EL0 тем самым
//! никогда не оставляет CPU на пользовательской таблице страниц.

use core::arch::global_asm;

extern "C" {
    static rustos_vectors: u8;
    fn rustos_enter_user_asm(
        entry: u64,
        stack: u64,
        argument0: u64,
        argument1: u64,
        argument2: u64,
        root: u64,
        interrupts: u64,
    ) -> u64;
    fn rustos_enter_user_frame_asm(frame: *mut super::TrapFrame, root: u64) -> u64;
}

/// Результат завершившегося синхронного user run. Записывается только из
/// trap handler перед веткой `rustos_abort_user`.
#[no_mangle]
static mut rustos_user_result: u64 = 0;

/// Устанавливает VBAR_EL1 на 2-KiB-выровненную таблицу в kernel `.text`.
pub fn initialize() {
    // SAFETY: linker сохраняет выравнивание символа из `global_asm`; таблица
    // содержит ровно 16 архитектурных слотов по 128 байт.
    let vectors = unsafe { &rustos_vectors } as *const u8 as u64;
    unsafe {
        core::arch::asm!(
            "msr vbar_el1, {address}",
            "isb",
            address = in(reg) vectors,
            options(nostack),
        );
    }
}

/// Синхронно запускает EL0 context и возвращается после exit/fault.
///
/// # Safety
///
/// `root` — валидная process translation table с общими kernel mappings;
/// `entry` и `stack` отображены в ней с подходящими правами.
pub unsafe fn enter_user(
    entry: u64,
    stack: u64,
    arguments: [u64; 3],
    root: u64,
    interrupts: bool,
) -> u64 {
    // SAFETY: контракт функции полностью совпадает с assembly boundary.
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

/// Входит в уже сохранённый scheduler context через готовый TrapFrame.
pub unsafe fn enter_user_frame(frame: &mut super::TrapFrame, root: u64) -> u64 {
    // SAFETY: frame живёт на kernel stack до возврата через abort boundary.
    unsafe { rustos_enter_user_frame_asm(frame, root) }
}

pub fn set_user_result(result: u64) {
    // SAFETY: bootstrap runner на текущем этапе один на CPU; handler пишет
    // значение до перехода на сохранённый kernel stack.
    unsafe { rustos_user_result = result };
}

global_asm!(
    r#"
    .text

    // 16 слотов по 128 байт. VBAR требует 2-KiB alignment всей таблицы.
    .align 11
    .global rustos_vectors
rustos_vectors:
    // 0x000: Current EL, SP_EL0.
    b rustos_park
    .balign 128
    b rustos_park
    .balign 128
    b rustos_park
    .balign 128
    b rustos_park
    .balign 128

    // 0x200: Current EL, SP_EL1. Sync нужен для диагностики kernel fault,
    // IRQ — для короткого окна между rearm и следующим eret.
    b rustos_sync_entry
    .balign 128
    b rustos_irq_entry
    .balign 128
    b rustos_park
    .balign 128
    b rustos_park
    .balign 128

    // 0x400: Lower EL, AArch64 — syscall/fault и timer preemption.
    b rustos_sync_entry
    .balign 128
    b rustos_irq_entry
    .balign 128
    b rustos_park
    .balign 128
    b rustos_park
    .balign 128

    // 0x600: Lower EL, AArch32 не поддерживается.
    b rustos_park
    .balign 128
    b rustos_park
    .balign 128
    b rustos_park
    .balign 128
    b rustos_park

    .global rustos_park
rustos_park:
    wfi
    b rustos_park

    // Frame size = 832 и кратен 16. q0 начинается с offset 304.
    .macro SAVE_FRAME source
    sub sp, sp, #832
    stp x0,  x1,  [sp, #0]
    stp x2,  x3,  [sp, #16]
    stp x4,  x5,  [sp, #32]
    stp x6,  x7,  [sp, #48]
    stp x8,  x9,  [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x9, sp_el0
    str x9, [sp, #248]
    mrs x9, elr_el1
    str x9, [sp, #256]
    mrs x9, spsr_el1
    str x9, [sp, #264]
    mrs x9, esr_el1
    str x9, [sp, #272]
    mrs x9, far_el1
    str x9, [sp, #280]
    mov x9, #\source
    str x9, [sp, #288]
    str xzr, [sp, #296]
    stp q0,  q1,  [sp, #304]
    stp q2,  q3,  [sp, #336]
    stp q4,  q5,  [sp, #368]
    stp q6,  q7,  [sp, #400]
    stp q8,  q9,  [sp, #432]
    stp q10, q11, [sp, #464]
    stp q12, q13, [sp, #496]
    stp q14, q15, [sp, #528]
    stp q16, q17, [sp, #560]
    stp q18, q19, [sp, #592]
    stp q20, q21, [sp, #624]
    stp q22, q23, [sp, #656]
    stp q24, q25, [sp, #688]
    stp q26, q27, [sp, #720]
    stp q28, q29, [sp, #752]
    stp q30, q31, [sp, #784]
    mrs x9, fpcr
    str x9, [sp, #816]
    mrs x9, fpsr
    str x9, [sp, #824]
    .endm

    .macro RESTORE_FRAME
    ldr x9, [sp, #816]
    msr fpcr, x9
    ldr x9, [sp, #824]
    msr fpsr, x9
    ldp q0,  q1,  [sp, #304]
    ldp q2,  q3,  [sp, #336]
    ldp q4,  q5,  [sp, #368]
    ldp q6,  q7,  [sp, #400]
    ldp q8,  q9,  [sp, #432]
    ldp q10, q11, [sp, #464]
    ldp q12, q13, [sp, #496]
    ldp q14, q15, [sp, #528]
    ldp q16, q17, [sp, #560]
    ldp q18, q19, [sp, #592]
    ldp q20, q21, [sp, #624]
    ldp q22, q23, [sp, #656]
    ldp q24, q25, [sp, #688]
    ldp q26, q27, [sp, #720]
    ldp q28, q29, [sp, #752]
    ldp q30, q31, [sp, #784]
    ldr x9, [sp, #248]
    msr sp_el0, x9
    ldr x9, [sp, #256]
    msr elr_el1, x9
    ldr x9, [sp, #264]
    msr spsr_el1, x9
    ldp x0,  x1,  [sp, #0]
    ldp x2,  x3,  [sp, #16]
    ldp x4,  x5,  [sp, #32]
    ldp x6,  x7,  [sp, #48]
    ldp x8,  x9,  [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #832
    eret
    .endm

    .global rustos_sync_entry
rustos_sync_entry:
    SAVE_FRAME 0
    mov x0, sp
    bl rustos_handle_trap
    cbnz x0, rustos_abort_user
    RESTORE_FRAME

    .global rustos_irq_entry
rustos_irq_entry:
    SAVE_FRAME 1
    bl rustos_gic_acknowledge
    // PPI 27 — architected EL1 virtual timer (`CNTV_*`).
    cmp w0, #27
    b.eq 1f
    // Любой неожиданный/spurious INTID не считается scheduler tick.
    mov x9, #2
    str x9, [sp, #288]
1:
    mov x0, sp
    bl rustos_handle_trap
    // Сохраняем disposition через EOI-вызов в callee-saved x19; исходный
    // x19 уже находится в TrapFrame.
    mov x19, x0
    bl rustos_gic_eoi
    mov x0, x19
    cbnz x0, rustos_abort_user
    RESTORE_FRAME

    .bss
    .align 3
rustos_saved_kernel_sp:
    .quad 0
rustos_saved_kernel_ttbr0:
    .quad 0

    .text
    .global rustos_enter_user_asm
rustos_enter_user_asm:
    // Сохраняем callee-saved Rust ABI state на kernel stack.
    stp x19, x20, [sp, #-16]!
    stp x21, x22, [sp, #-16]!
    stp x23, x24, [sp, #-16]!
    stp x25, x26, [sp, #-16]!
    stp x27, x28, [sp, #-16]!
    stp x29, x30, [sp, #-16]!

    adrp x9, rustos_saved_kernel_sp
    add x9, x9, :lo12:rustos_saved_kernel_sp
    mov x10, sp
    str x10, [x9]
    mrs x10, ttbr0_el1
    adrp x9, rustos_saved_kernel_ttbr0
    add x9, x9, :lo12:rustos_saved_kernel_ttbr0
    str x10, [x9]

    // Переносим ABI-аргументы до переключения TTBR0.
    mov x9, x0
    mov x10, x1
    mov x0, x2
    mov x1, x3
    mov x2, x4

    dsb ishst
    msr ttbr0_el1, x5
    tlbi vmalle1is
    dsb ish
    isb

    msr sp_el0, x10
    msr elr_el1, x9
    cbnz x6, 1f
    mov x11, #0xc0
    b 2f
1:
    mov x11, xzr
2:
    msr spsr_el1, x11
    // Новый процесс не должен видеть kernel return address в link register.
    mov x30, xzr
    eret

    .global rustos_enter_user_frame_asm
rustos_enter_user_frame_asm:
    // Тот же kernel continuation, но EL0 state уже полностью подготовлен
    // scheduler'ом в TrapFrame на kernel stack.
    stp x19, x20, [sp, #-16]!
    stp x21, x22, [sp, #-16]!
    stp x23, x24, [sp, #-16]!
    stp x25, x26, [sp, #-16]!
    stp x27, x28, [sp, #-16]!
    stp x29, x30, [sp, #-16]!

    mov x12, x0
    adrp x9, rustos_saved_kernel_sp
    add x9, x9, :lo12:rustos_saved_kernel_sp
    mov x10, sp
    str x10, [x9]
    mrs x10, ttbr0_el1
    adrp x9, rustos_saved_kernel_ttbr0
    add x9, x9, :lo12:rustos_saved_kernel_ttbr0
    str x10, [x9]

    dsb ishst
    msr ttbr0_el1, x1
    tlbi vmalle1is
    dsb ish
    isb

    // В отличие от x86 TSS, AArch64 не подставляет отдельный SP_EL1 при
    // исключении из EL0. Поэтому нельзя оставить SP над временным frame:
    // большой Rust trap-handler перезапишет сохранённый kernel continuation.
    // x30 держит адрес frame до самой последней загрузки user link register.
    mov x30, x12
    ldr x9, [x30, #816]
    msr fpcr, x9
    ldr x9, [x30, #824]
    msr fpsr, x9
    ldp q0,  q1,  [x30, #304]
    ldp q2,  q3,  [x30, #336]
    ldp q4,  q5,  [x30, #368]
    ldp q6,  q7,  [x30, #400]
    ldp q8,  q9,  [x30, #432]
    ldp q10, q11, [x30, #464]
    ldp q12, q13, [x30, #496]
    ldp q14, q15, [x30, #528]
    ldp q16, q17, [x30, #560]
    ldp q18, q19, [x30, #592]
    ldp q20, q21, [x30, #624]
    ldp q22, q23, [x30, #656]
    ldp q24, q25, [x30, #688]
    ldp q26, q27, [x30, #720]
    ldp q28, q29, [x30, #752]
    ldp q30, q31, [x30, #784]
    ldr x9, [x30, #248]
    msr sp_el0, x9
    ldr x9, [x30, #256]
    msr elr_el1, x9
    ldr x9, [x30, #264]
    msr spsr_el1, x9
    adrp x9, rustos_saved_kernel_sp
    add x9, x9, :lo12:rustos_saved_kernel_sp
    ldr x9, [x9]
    mov sp, x9
    ldp x0,  x1,  [x30, #0]
    ldp x2,  x3,  [x30, #16]
    ldp x4,  x5,  [x30, #32]
    ldp x6,  x7,  [x30, #48]
    ldp x8,  x9,  [x30, #64]
    ldp x10, x11, [x30, #80]
    ldp x12, x13, [x30, #96]
    ldp x14, x15, [x30, #112]
    ldp x16, x17, [x30, #128]
    ldp x18, x19, [x30, #144]
    ldp x20, x21, [x30, #160]
    ldp x22, x23, [x30, #176]
    ldp x24, x25, [x30, #192]
    ldp x26, x27, [x30, #208]
    ldp x28, x29, [x30, #224]
    ldr x30, [x30, #240]
    eret

    // Общая ветка exit/fault: frame уже больше не нужен. Сначала возвращаем
    // kernel translation root, затем точный Rust ABI stack.
rustos_abort_user:
    adrp x9, rustos_saved_kernel_ttbr0
    add x9, x9, :lo12:rustos_saved_kernel_ttbr0
    ldr x10, [x9]
    dsb ishst
    msr ttbr0_el1, x10
    tlbi vmalle1is
    dsb ish
    isb

    adrp x9, rustos_saved_kernel_sp
    add x9, x9, :lo12:rustos_saved_kernel_sp
    ldr x10, [x9]
    mov sp, x10
    ldp x29, x30, [sp], #16
    ldp x27, x28, [sp], #16
    ldp x25, x26, [sp], #16
    ldp x23, x24, [sp], #16
    ldp x21, x22, [sp], #16
    ldp x19, x20, [sp], #16
    adrp x9, rustos_user_result
    add x9, x9, :lo12:rustos_user_result
    ldr x0, [x9]
    ret
    "#,
);

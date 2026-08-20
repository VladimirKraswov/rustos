//! Собственные GDT/TSS kernel'а.
//!
//! TSS нужен не для hardware task switching, а для `RSP0`: при trap из CPL3
//! CPU автоматически переходит на отдельный kernel stack. Double fault
//! получает независимый IST1, поэтому переполнение обычного стека не ведёт
//! сразу к triple fault.

use core::{arch::asm, mem::size_of, ptr::addr_of_mut};

pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_DATA_SELECTOR: u16 = 0x18 | 3;
pub const USER_CODE_SELECTOR: u16 = 0x20 | 3;
const TSS_SELECTOR: u16 = 0x28;

// ELF spawn пока конструирует фиксированный bootstrap `AddressSpace` на стеке
// syscall'а. LLVM в dev-профиле держит несколько промежуточных значений page
// metadata одновременно, поэтому оставляем 512 KiB с запасом для
// вложенного trap. После перехода metadata в kernel slabs это станет малым
// per-CPU stack + guard page без изменения пользовательского ABI.
const RING0_STACK_SIZE: usize = 512 * 1024;
const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    io_map_base: u16,
}

impl TaskStateSegment {
    const fn empty() -> Self {
        Self {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            io_map_base: size_of::<Self>() as u16,
        }
    }
}

#[repr(C, align(16))]
struct KernelStack<const N: usize>([u8; N]);

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

static mut GDT: [u64; 7] = [0; 7];
static mut TSS: TaskStateSegment = TaskStateSegment::empty();
static mut RING0_STACK: KernelStack<RING0_STACK_SIZE> = KernelStack([0; RING0_STACK_SIZE]);
static mut DOUBLE_FAULT_STACK: KernelStack<DOUBLE_FAULT_STACK_SIZE> =
    KernelStack([0; DOUBLE_FAULT_STACK_SIZE]);

/// Устанавливает GDT, TSS и возвращает вершину ring-0 stack.
pub fn initialize() -> u64 {
    let ring0_top = addr_of_mut!(RING0_STACK) as u64 + RING0_STACK_SIZE as u64;
    let double_fault_top = addr_of_mut!(DOUBLE_FAULT_STACK) as u64 + DOUBLE_FAULT_STACK_SIZE as u64;
    let tss_base = addr_of_mut!(TSS) as u64;

    unsafe {
        // Packed TSS заполняем raw pointer writes, не создавая unaligned refs.
        let tss = addr_of_mut!(TSS);
        (*tss).rsp[0] = ring0_top;
        (*tss).ist[0] = double_fault_top;

        GDT[0] = 0;
        GDT[1] = 0x00af_9a00_0000_ffff; // kernel 64-bit code, DPL0
        GDT[2] = 0x00cf_9200_0000_ffff; // kernel data, DPL0
        GDT[3] = 0x00cf_f200_0000_ffff; // user data, DPL3
        GDT[4] = 0x00af_fa00_0000_ffff; // user 64-bit code, DPL3
        let limit = (size_of::<TaskStateSegment>() - 1) as u64;
        GDT[5] = limit
            | ((tss_base & 0x00ff_ffff) << 16)
            | (0x89 << 40)
            | (((limit >> 16) & 0x0f) << 48)
            | (((tss_base >> 24) & 0xff) << 56);
        GDT[6] = tss_base >> 32;

        let pointer = DescriptorTablePointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: addr_of_mut!(GDT) as u64,
        };
        asm!(
            "lgdt [{}]",
            "push {kernel_code}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            "mov ax, {kernel_data}",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "mov ax, {tss}",
            "ltr ax",
            in(reg) &pointer,
            kernel_code = const KERNEL_CODE_SELECTOR,
            kernel_data = const KERNEL_DATA_SELECTOR,
            tss = const TSS_SELECTOR,
            out("rax") _,
            options(preserves_flags),
        );
    }
    ring0_top
}

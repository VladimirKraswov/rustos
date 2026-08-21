//! Запуск application processors через Device Tree и PSCI.
//!
//! Firmware остаётся источником истины: ядро читает MPIDR всех включённых
//! CPU из `/cpus`, способ вызова PSCI из `/psci/method`, а затем запускает
//! каждый AP настоящим `CPU_ON`. Вторичные ядра получают отдельные стеки,
//! включают ту же identity translation table и подтверждают готовность
//! атомарным битом. До появления per-CPU scheduler они намеренно остаются в
//! `WFI`: это честный bring-up, а не фиктивное увеличение счётчика CPU.

use core::{
    arch::{asm, global_asm},
    slice,
    sync::atomic::{AtomicU64, Ordering},
};

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Совпадает с первой 64-битной affinity mask планировщика.
const MAX_CPUS: usize = 64;
const MAX_APS: usize = MAX_CPUS - 1;
const AP_STACK_SIZE: usize = 32 * 1024;
const MAX_DTB_SIZE: usize = 4 * 1024 * 1024;
const PSCI_CPU_ON_64: u64 = 0xc400_0003;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmpError {
    InvalidDeviceTree,
    MissingPsci,
    UnsupportedPsciMethod,
    TooManyCpus,
    PsciCallFailed,
    ApTimeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmpInfo {
    /// BSP + AP, которые действительно выполнили `rustos_ap_online`.
    pub online_cpus: usize,
    /// Все enabled CPU, объявленные Device Tree.
    pub discovered_cpus: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PsciConduit {
    Hvc,
    Smc,
}

#[derive(Clone, Copy)]
struct Platform {
    mpidrs: [u64; MAX_CPUS],
    cpu_count: usize,
    conduit: Option<PsciConduit>,
    cpu_on: u64,
}

impl Platform {
    const fn empty() -> Self {
        Self {
            mpidrs: [0; MAX_CPUS],
            cpu_count: 0,
            conduit: None,
            cpu_on: PSCI_CPU_ON_64,
        }
    }

    fn push_cpu(&mut self, mpidr: u64) -> Result<(), SmpError> {
        let mpidr = affinity(mpidr);
        if self.mpidrs[..self.cpu_count].contains(&mpidr) {
            return Ok(());
        }
        if self.cpu_count == MAX_CPUS {
            return Err(SmpError::TooManyCpus);
        }
        self.mpidrs[self.cpu_count] = mpidr;
        self.cpu_count += 1;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct CpuNode {
    depth: usize,
    mpidr: Option<u64>,
    is_cpu: bool,
    enabled: bool,
    uses_psci: bool,
}

#[repr(C, align(4096))]
struct ApStacks([u8; MAX_APS * AP_STACK_SIZE]);

// Символы читаются AP до включения MMU, поэтому им нужны стабильные имена и
// физические адреса внутри статически слинкованного kernel image.
#[no_mangle]
static mut rustos_ap_stacks: ApStacks = ApStacks([0; MAX_APS * AP_STACK_SIZE]);
#[no_mangle]
static mut rustos_ap_ttbr0: u64 = 0;

/// bit 0 — BSP; bit N — AP slot N-1.
static ONLINE_MASK: AtomicU64 = AtomicU64::new(1);

extern "C" {
    fn rustos_ap_entry();
}

/// Находит CPU в FDT, запускает AP и ждёт реального подтверждения каждого.
pub fn start_application_processors(
    device_tree: u64,
    counter_hz: u64,
) -> Result<SmpInfo, SmpError> {
    let platform = unsafe { parse_device_tree(device_tree)? };
    if platform.cpu_count == 0 {
        return Err(SmpError::InvalidDeviceTree);
    }
    let conduit = platform.conduit.ok_or(SmpError::MissingPsci)?;
    let bsp = current_mpidr();
    if !platform.mpidrs[..platform.cpu_count].contains(&bsp) {
        return Err(SmpError::InvalidDeviceTree);
    }

    ONLINE_MASK.store(1, Ordering::Release);
    // SAFETY: AP ещё не запущены; запись публикуется `dsb` перед CPU_ON.
    unsafe { rustos_ap_ttbr0 = super::current_address_space_root() };
    unsafe { asm!("dsb sy", options(nostack)) };

    for (slot, target) in platform.mpidrs[..platform.cpu_count]
        .iter()
        .copied()
        .filter(|mpidr| *mpidr != bsp)
        .enumerate()
    {
        if slot == MAX_APS {
            return Err(SmpError::TooManyCpus);
        }
        // SAFETY: entry находится в identity-mapped kernel image, context id
        // индексирует заранее выделенный AP stack.
        unsafe {
            cpu_on(
                conduit,
                platform.cpu_on,
                target,
                rustos_ap_entry as *const () as u64,
                slot as u64,
            )?;
        }

        let ready_bit = 1u64 << (slot + 1);
        let started_at = super::read_monotonic_counter();
        let timeout = (counter_hz / 5).max(1);
        while ONLINE_MASK.load(Ordering::Acquire) & ready_bit == 0 {
            if super::read_monotonic_counter().wrapping_sub(started_at) >= timeout {
                return Err(SmpError::ApTimeout);
            }
            core::hint::spin_loop();
        }
    }

    Ok(SmpInfo {
        online_cpus: ONLINE_MASK.load(Ordering::Acquire).count_ones() as usize,
        discovered_cpus: platform.cpu_count,
    })
}

/// Первая Rust-функция AP. Здесь уже включена MMU и установлен отдельный stack.
#[no_mangle]
extern "C" fn rustos_ap_online(slot: usize) -> ! {
    if slot < MAX_APS {
        ONLINE_MASK.fetch_or(1u64 << (slot + 1), Ordering::Release);
    }
    unsafe { asm!("dsb sy", "sev", options(nostack)) };
    loop {
        // Пока scheduler однопроцессорный, AP не получает задачи и IRQ.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

unsafe fn cpu_on(
    conduit: PsciConduit,
    function: u64,
    target: u64,
    entry: u64,
    context: u64,
) -> Result<(), SmpError> {
    let mut status = function;
    match conduit {
        PsciConduit::Hvc => unsafe {
            asm!(
                "hvc #0",
                inout("x0") status,
                in("x1") target,
                in("x2") entry,
                in("x3") context,
                options(nostack),
            );
        },
        PsciConduit::Smc => unsafe {
            asm!(
                "smc #0",
                inout("x0") status,
                in("x1") target,
                in("x2") entry,
                in("x3") context,
                options(nostack),
            );
        },
    }
    if status == 0 {
        Ok(())
    } else {
        Err(SmpError::PsciCallFailed)
    }
}

fn current_mpidr() -> u64 {
    let value: u64;
    unsafe {
        asm!(
            "mrs {value}, mpidr_el1",
            value = out(reg) value,
            options(nomem, nostack),
        );
    }
    affinity(value)
}

/// Оставляет только Aff3..Aff0; U/MT и резервные биты не идентифицируют CPU.
const fn affinity(mpidr: u64) -> u64 {
    mpidr & 0x0000_00ff_00ff_ffff
}

/// Минимальный bounded FDT parser для `/cpus` и `/psci`.
///
/// # Safety
///
/// `root` обязан указывать на identity-mapped FDT header, сохранённый UEFI
/// loader'ом после ExitBootServices.
unsafe fn parse_device_tree(root: u64) -> Result<Platform, SmpError> {
    if root == 0 {
        return Err(SmpError::InvalidDeviceTree);
    }
    let header = unsafe { slice::from_raw_parts(root as *const u8, 40) };
    if be32(header, 0)? != FDT_MAGIC {
        return Err(SmpError::InvalidDeviceTree);
    }
    let total_size = be32(header, 4)? as usize;
    if !(40..=MAX_DTB_SIZE).contains(&total_size) {
        return Err(SmpError::InvalidDeviceTree);
    }
    let blob = unsafe { slice::from_raw_parts(root as *const u8, total_size) };
    let struct_offset = be32(blob, 8)? as usize;
    let strings_offset = be32(blob, 12)? as usize;
    let strings_size = be32(blob, 32)? as usize;
    let struct_size = be32(blob, 36)? as usize;
    let structure = range(blob, struct_offset, struct_size)?;
    let strings = range(blob, strings_offset, strings_size)?;

    let mut platform = Platform::empty();
    let mut cursor = 0usize;
    let mut depth = 0usize;
    let mut cpus_depth = None;
    let mut psci_depth = None;
    let mut cpu_address_cells = 1usize;
    let mut cpu_node: Option<CpuNode> = None;

    while cursor + 4 <= structure.len() {
        let token = be32(structure, cursor)?;
        cursor += 4;
        match token {
            FDT_BEGIN_NODE => {
                let (name, consumed) = c_string(&structure[cursor..])?;
                cursor = align4(
                    cursor
                        .checked_add(consumed)
                        .ok_or(SmpError::InvalidDeviceTree)?,
                )?;
                depth = depth.checked_add(1).ok_or(SmpError::InvalidDeviceTree)?;
                if depth == 2 && name == b"cpus" {
                    cpus_depth = Some(depth);
                } else if cpus_depth.is_some_and(|parent| depth == parent + 1)
                    && name.starts_with(b"cpu@")
                {
                    cpu_node = Some(CpuNode {
                        depth,
                        mpidr: None,
                        is_cpu: false,
                        enabled: true,
                        uses_psci: false,
                    });
                }
                if depth == 2 && name == b"psci" {
                    psci_depth = Some(depth);
                }
            }
            FDT_END_NODE => {
                if cpu_node.is_some_and(|node| node.depth == depth) {
                    let node = cpu_node.take().ok_or(SmpError::InvalidDeviceTree)?;
                    if node.is_cpu && node.enabled && node.uses_psci {
                        platform.push_cpu(node.mpidr.ok_or(SmpError::InvalidDeviceTree)?)?;
                    }
                }
                if cpus_depth == Some(depth) {
                    cpus_depth = None;
                }
                if psci_depth == Some(depth) {
                    psci_depth = None;
                }
                depth = depth.checked_sub(1).ok_or(SmpError::InvalidDeviceTree)?;
            }
            FDT_PROP => {
                let length = be32(structure, cursor)? as usize;
                let name_offset = be32(structure, cursor + 4)? as usize;
                cursor = cursor.checked_add(8).ok_or(SmpError::InvalidDeviceTree)?;
                let data = range(structure, cursor, length)?;
                cursor = align4(
                    cursor
                        .checked_add(length)
                        .ok_or(SmpError::InvalidDeviceTree)?,
                )?;
                let (name, _) = c_string(
                    strings
                        .get(name_offset..)
                        .ok_or(SmpError::InvalidDeviceTree)?,
                )?;

                if cpus_depth == Some(depth) && name == b"#address-cells" {
                    cpu_address_cells = be32(data, 0)? as usize;
                    if !(1..=2).contains(&cpu_address_cells) {
                        return Err(SmpError::InvalidDeviceTree);
                    }
                }
                if let Some(node) = cpu_node.as_mut().filter(|node| node.depth == depth) {
                    match name {
                        b"device_type" => node.is_cpu = c_string(data)?.0 == b"cpu",
                        b"enable-method" => node.uses_psci = c_string(data)?.0 == b"psci",
                        b"status" => {
                            let status = c_string(data)?.0;
                            node.enabled = status == b"okay" || status == b"ok";
                        }
                        b"reg" => {
                            node.mpidr = Some(match cpu_address_cells {
                                1 => u64::from(be32(data, 0)?),
                                2 => (u64::from(be32(data, 0)?) << 32) | u64::from(be32(data, 4)?),
                                _ => return Err(SmpError::InvalidDeviceTree),
                            });
                        }
                        _ => {}
                    }
                }
                if psci_depth == Some(depth) {
                    match name {
                        b"method" => {
                            platform.conduit = Some(match c_string(data)?.0 {
                                b"hvc" => PsciConduit::Hvc,
                                b"smc" => PsciConduit::Smc,
                                _ => return Err(SmpError::UnsupportedPsciMethod),
                            });
                        }
                        b"cpu_on" => platform.cpu_on = u64::from(be32(data, 0)?),
                        _ => {}
                    }
                }
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return Err(SmpError::InvalidDeviceTree),
        }
    }

    if depth != 0 || platform.cpu_count == 0 {
        return Err(SmpError::InvalidDeviceTree);
    }
    Ok(platform)
}

fn range(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], SmpError> {
    let end = offset
        .checked_add(length)
        .ok_or(SmpError::InvalidDeviceTree)?;
    bytes.get(offset..end).ok_or(SmpError::InvalidDeviceTree)
}

fn be32(bytes: &[u8], offset: usize) -> Result<u32, SmpError> {
    let value = range(bytes, offset, 4)?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn c_string(bytes: &[u8]) -> Result<(&[u8], usize), SmpError> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(SmpError::InvalidDeviceTree)?;
    Ok((&bytes[..end], end + 1))
}

fn align4(value: usize) -> Result<usize, SmpError> {
    value
        .checked_add(3)
        .map(|value| value & !3)
        .ok_or(SmpError::InvalidDeviceTree)
}

global_asm!(
    r#"
    .text
    .align 7
    .global rustos_ap_entry
rustos_ap_entry:
    // PSCI передаёт context_id в x0. Любое обращение к Rust stack возможно
    // только после выбора отдельного слота.
    msr daifset, #0xf
    mov x20, x0
    mrs x1, CurrentEL
    cmp x1, #4
    b.ne rustos_ap_park_forever

    adrp x1, rustos_ap_stacks
    add x1, x1, :lo12:rustos_ap_stacks
    mov x2, #1
    lsl x2, x2, #15
    madd x1, x20, x2, x1
    add sp, x1, x2

    // AP приходит с выключенной stage-1 MMU. Повторяем translation regime
    // boot CPU и включаем identity map, опубликованный перед CPU_ON.
    mrs x3, sctlr_el1
    bic x3, x3, #1
    msr sctlr_el1, x3
    isb
    adrp x1, rustos_ap_ttbr0
    add x1, x1, :lo12:rustos_ap_ttbr0
    ldr x1, [x1]
    msr ttbr0_el1, x1
    msr ttbr1_el1, xzr
    adrp x1, rustos_vectors
    add x1, x1, :lo12:rustos_vectors
    msr vbar_el1, x1
    mov x1, #0x04ff
    msr mair_el1, x1
    mov x1, #0x3510
    movk x1, #0x0080, lsl #16
    movk x1, #0x0004, lsl #32
    msr tcr_el1, x1
    mov x1, #0x1805
    movk x1, #0x30d0, lsl #16
    ic iallu
    tlbi vmalle1
    dsb sy
    isb
    msr sctlr_el1, x1
    isb

    mov x0, x20
    bl rustos_ap_online

rustos_ap_park_forever:
    wfi
    b rustos_ap_park_forever
    "#,
);

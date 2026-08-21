//! Загрузка ELF64-образа ядра (PIE или статический).
//!
//! ## Контракт
//!
//! * Образ — **ELF64, little-endian**, один из двух типов:
//!   * **ET_DYN (PIE)**: rustc собирает ядро с `relocation-model=pic` и
//!     `file-type=pie` (см. targets/*.json). Загрузчик копирует сегменты
//!     `PT_LOAD` в `base + (p_vaddr - min_vaddr)` и применяет динамические
//!     релокации типа RELATIVE целевой архитектуры
//!     (`R_X86_64_RELATIVE` = 8 / `R_AARCH64_RELATIVE` = 2):
//!     `*loc = base + addend - min_v`. Другие типы релокаций не ожидаются
//!     (статический PIE без dynsym-ссылок) — их появление = ошибка сборки.
//!   * **ET_EXEC (статический, фиксированная линковка)**: сегменты
//!     загружаются по физическим vaddr — то же самое выражение при
//!     `base = min_vaddr`. Динамических релокаций нет (и нет
//!     `.rela.dyn`); их появление = ошибка сборки.
//! * Точка входа = `base + (e_entry - min_vaddr)` (для ET_EXEC —
//!   абсолютный `e_entry`).
//!
//! Раскладка резерва, в который кладётся образ: `main.rs` (`Layout`,
//! `find_reservation` для PIE; для ET_EXEC резерв начинается ровно в
//! `min_vaddr`). Связанный формат initramfs: `tools/pack` (RIFS v1).

/// Максимум PT_LOAD-сегментов в образе ядра (реалистично: десятки).
const MAX_LOADS: usize = 64;

/// Тип ELF-образа.
const ET_EXEC: u16 = 2;
/// PIE-образ (movable, min_vaddr обычно 0).
const ET_DYN: u16 = 3;
/// Oжидаемый e_machine ядра: зависит от целевой архитектуры.
#[cfg(target_arch = "x86_64")]
const EM_KERNEL: u16 = 0x3E; // EM_X86_64
#[cfg(target_arch = "aarch64")]
const EM_KERNEL: u16 = 183; // EM_AARCH64
const EI_CLASS_64: u8 = 2;
const EI_DATA_LE: u8 = 1;

/// Program header types.
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

/// Dynamic tags.
const DT_REL: i64 = 17;
const DT_RELSZ: i64 = 18;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;

/// RELATIVE-релокация целевой архитектуры — единственная ожидаемая в
/// статическом PIE-ядре.
#[cfg(target_arch = "x86_64")]
/// `R_X86_64_RELATIVE` = **8** (не 1024! — 1024/1025/1026 это секционные
/// флаги `R_X86_64_GNU_STACK`/`GNU_RELRO`, а не релокации).
const R_RELATIVE: u32 = 8;
#[cfg(target_arch = "aarch64")]
/// `R_AARCH64_RELATIVE` = **2**.
const R_RELATIVE: u32 = 2;

/// Ошибка разбора/загрузки ELF.
#[derive(Debug)]
pub enum ElfError {
    /// Образ короче ELF-заголовка.
    TooShort,
    /// Неверная магия \x7fELF.
    BadMagic,
    /// Не 64-битный / не little-endian.
    BadEncoding,
    /// Не x86-64.
    BadMachine,
    /// Не ET_DYN и не ET_EXEC (ядро — PIE или статический).
    BadType(u16),
    /// Нет PT_LOAD-сегментов.
    NoLoadSegments,
    /// Слишком много PT_LOAD-сегментов.
    TooManySegments,
    /// Повреждённые сегменты (выход за границы образа).
    BadSegment,
    /// Встречена не R_X86_64_RELATIVE релокация.
    BadRelocation(u32),
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ElfError::TooShort => write!(f, "image shorter than ELF header"),
            ElfError::BadMagic => write!(f, "bad ELF magic"),
            ElfError::BadEncoding => write!(f, "expected ELF64 little-endian"),
            ElfError::BadMachine => write!(
                f,
                "unexpected ELF e_machine (expected kernel target architecture)"
            ),
            ElfError::BadType(t) => {
                write!(
                    f,
                    "unexpected ELF e_type {t} (kernel must be ET_DYN or ET_EXEC)"
                )
            }
            ElfError::NoLoadSegments => write!(f, "no PT_LOAD segments"),
            ElfError::TooManySegments => write!(f, "too many PT_LOAD segments"),
            ElfError::BadSegment => write!(f, "segment out of bounds"),
            ElfError::BadRelocation(t) => write!(f, "unsupported relocation type {t}"),
        }
    }
}

/// ELF64-заголовок (модель данных для разбора, не `repr(C)`-маппинг).
struct Ehdr {
    /// e_type: ET_DYN (PIE) или ET_EXEC (статический).
    etype: u16,
    entry: u64,
    phoff: u64,
    phentsize: u16,
    phnum: u16,
}

/// PT_LOAD-сегмент.
#[derive(Clone, Copy)]
struct PhdrLoad {
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

/// PT_DYNAMIC: начало и размер таблиц REL/RELA.
#[derive(Clone, Copy, Default)]
struct DynInfo {
    rel_off: u64,
    rel_size: u64,
    rela_off: u64,
    rela_size: u64,
}

/// Чтение целочисленного типа (u16/u32/u64/i64) little-endian из среза
/// с проверкой границ.
fn read_at<T>(buf: &[u8], off: usize) -> Option<T>
where
    T: Copy,
{
    let n = core::mem::size_of::<T>();
    let start = off.checked_add(n)?;
    if start > buf.len() {
        return None;
    }
    let mut v: T = unsafe { core::mem::zeroed() };
    // SAFETY: T — простой целочисленный тип (u16/u32/u64/i64) без padding,
    // x86-64 little-endian; `v` — валидная локальная переменная на n байт,
    // `buf[off..start]` — валидные n байт (проверено выше).
    unsafe {
        (&mut v as *mut T)
            .cast::<u8>()
            .copy_from_nonoverlapping(buf[off..start].as_ptr(), n);
    }
    Some(v)
}

/// Разбор ELF-заголовка.
fn parse_ehdr(buf: &[u8]) -> Result<Ehdr, ElfError> {
    if buf.len() < 64 {
        return Err(ElfError::TooShort);
    }
    if &buf[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if buf[4] != EI_CLASS_64 || buf[5] != EI_DATA_LE {
        return Err(ElfError::BadEncoding);
    }
    let e_type = read_at::<u16>(buf, 16).ok_or(ElfError::TooShort)?;
    let e_machine = read_at::<u16>(buf, 18).ok_or(ElfError::TooShort)?;
    if e_machine != EM_KERNEL {
        return Err(ElfError::BadMachine);
    }
    // ET_DYN — PIE-ядро (movable base), ET_EXEC — статическое ядро с
    // фиксированной линковкой (загружается по vaddr).
    if e_type != ET_DYN && e_type != ET_EXEC {
        return Err(ElfError::BadType(e_type));
    }
    Ok(Ehdr {
        etype: e_type,
        entry: read_at::<u64>(buf, 24).ok_or(ElfError::TooShort)?,
        phoff: read_at::<u64>(buf, 32).ok_or(ElfError::TooShort)?,
        phentsize: read_at::<u16>(buf, 54).ok_or(ElfError::TooShort)?,
        phnum: read_at::<u16>(buf, 56).ok_or(ElfError::TooShort)?,
    })
}

/// Информация о базе загрузки: `(статический?, min_vaddr PT_LOAD)`.
///
/// Для ET_EXEC загрузчик резервирует блок ровно с `min_vaddr` (сегменты
/// копируются по физическим vaddr); для ET_DYN — по свободной верхней
/// conventional-области (см. `main::find_reservation`).
pub fn load_base(elf: &[u8]) -> Result<(bool, u64), ElfError> {
    let eh = parse_ehdr(elf)?;
    let (loads, nloads, _) = parse_phdrs(elf, &eh)?;
    let min_v = loads[..nloads].iter().map(|p| p.vaddr).min().unwrap_or(0);
    Ok((eh.etype == ET_EXEC, min_v))
}

/// Разбор program headers: PT_LOAD (массив по значению + счётчик) + PT_DYNAMIC.
///
/// Массив возвращаем по значению (фиксированный размер, без аллокаций в
/// no_std-загрузчике); вызывающий срезает его до `nloads`.
fn parse_phdrs(
    buf: &[u8],
    eh: &Ehdr,
) -> Result<([PhdrLoad; MAX_LOADS], usize, Option<DynInfo>), ElfError> {
    let mut loads = [PhdrLoad {
        offset: 0,
        vaddr: 0,
        filesz: 0,
        memsz: 0,
    }; MAX_LOADS];
    let mut nloads = 0usize;
    let mut dyninfo = None;
    for i in 0..eh.phnum as usize {
        let off = eh.phoff as usize + i * eh.phentsize as usize;
        let p_type = read_at::<u32>(buf, off).ok_or(ElfError::BadSegment)?;
        match p_type {
            PT_LOAD => {
                if nloads == MAX_LOADS {
                    return Err(ElfError::TooManySegments);
                }
                loads[nloads] = PhdrLoad {
                    offset: read_at::<u64>(buf, off + 8).ok_or(ElfError::BadSegment)?,
                    vaddr: read_at::<u64>(buf, off + 16).ok_or(ElfError::BadSegment)?,
                    filesz: read_at::<u64>(buf, off + 32).ok_or(ElfError::BadSegment)?,
                    memsz: read_at::<u64>(buf, off + 40).ok_or(ElfError::BadSegment)?,
                };
                nloads += 1;
            }
            PT_DYNAMIC => {
                let p_offset = read_at::<u64>(buf, off + 8).ok_or(ElfError::BadSegment)?;
                let p_size = read_at::<u64>(buf, off + 32).ok_or(ElfError::BadSegment)?;
                let mut info = DynInfo::default();
                for j in 0..(p_size as usize / 16) {
                    let doff = p_offset as usize + j * 16;
                    let d_tag = read_at::<i64>(buf, doff).ok_or(ElfError::BadSegment)?;
                    let d_val = read_at::<u64>(buf, doff + 8).ok_or(ElfError::BadSegment)?;
                    match d_tag {
                        DT_REL => info.rel_off = d_val,
                        DT_RELSZ => info.rel_size = d_val,
                        DT_RELA => info.rela_off = d_val,
                        DT_RELASZ => info.rela_size = d_val,
                        _ => {}
                    }
                }
                dyninfo = Some(info);
            }
            _ => {}
        }
    }
    if nloads == 0 {
        return Err(ElfError::NoLoadSegments);
    }
    Ok((loads, nloads, dyninfo))
}

/// Размер образа в памяти: максимум `vaddr + memsz` минус `min(vaddr)`.
pub fn image_size(elf: &[u8]) -> Result<u64, ElfError> {
    let eh = parse_ehdr(elf)?;
    let (loads, nloads, _) = parse_phdrs(elf, &eh)?;
    let loads = &loads[..nloads];
    let min_v = loads.iter().map(|p| p.vaddr).min().unwrap_or(0);
    let max_end = loads
        .iter()
        .map(|p| p.vaddr + p.memsz)
        .max()
        .ok_or(ElfError::NoLoadSegments)?;
    Ok(max_end - min_v)
}

/// Загрузка ELF в память по `base` (identity-адресное пространство UEFI).
///
/// Возвращает физический адрес точки входа.
///
/// # Safety
///
/// `base` должен указывать на доступную для записи RAM достаточного
/// размера (загрузчик закрепляет регион через `allocate_pages` до вызова —
/// см. `main::boot`, шаг 2).
pub unsafe fn load(elf: &[u8], base: u64) -> Result<u64, ElfError> {
    let eh = parse_ehdr(elf)?;
    let (loads_arr, nloads, dyninfo) = parse_phdrs(elf, &eh)?;
    let loads = &loads_arr[..nloads];
    let min_v = loads.iter().map(|p| p.vaddr).min().unwrap_or(0);

    // 1. Копирование PT_LOAD-сегментов (+ обнуление bss-хвостов).
    for p in loads {
        let dest = (base + p.vaddr - min_v) as *mut u8;
        if p.offset as usize + p.filesz as usize > elf.len() {
            return Err(ElfError::BadSegment);
        }
        // SAFETY: p.offset..p.offset+p.filesz — в пределах `elf`
        // (проверено выше); dest — в пределах резерва (контракт вызывающего).
        unsafe {
            core::ptr::copy_nonoverlapping(
                elf.get_unchecked(p.offset as usize..(p.offset + p.filesz) as usize)
                    .as_ptr(),
                dest,
                p.filesz as usize,
            );
            if p.memsz > p.filesz {
                // memsz-хвост (bss) обнуляем.
                dest.add(p.filesz as usize)
                    .write_bytes(0, (p.memsz - p.filesz) as usize);
            }
        }
    }

    // 2. Динамические релокации (только R_X86_64_RELATIVE).
    if let Some(d) = dyninfo {
        if d.rela_size > 0 {
            for i in 0..(d.rela_size as usize / 24) {
                let roff = d.rela_off as usize + i * 24;
                let r_offset = read_at::<u64>(elf, roff).ok_or(ElfError::BadSegment)?;
                let r_info = read_at::<u64>(elf, roff + 8).ok_or(ElfError::BadSegment)?;
                let r_addend = read_at::<u64>(elf, roff + 16).ok_or(ElfError::BadSegment)?;
                // Значение RELATIVE = base + addend - min_v (addend — vaddr цели
                // в связочном пространстве образа; цель загружается в base + (vaddr - min_v)).
                apply_relative(
                    r_info,
                    (base + r_offset - min_v) as *mut u64,
                    base + r_addend - min_v,
                )?;
            }
        }
        if d.rel_size > 0 {
            for i in 0..(d.rel_size as usize / 16) {
                let roff = d.rel_off as usize + i * 16;
                let r_offset = read_at::<u64>(elf, roff).ok_or(ElfError::BadSegment)?;
                let r_info = read_at::<u64>(elf, roff + 8).ok_or(ElfError::BadSegment)?;
                // Для REL (без addend) допустима только RELATIVE (addend = 0 → base - min_v).
                apply_relative(r_info, (base + r_offset - min_v) as *mut u64, base - min_v)?;
            }
        }
    }

    Ok(base + eh.entry - min_v)
}

/// Применение одной релокации; любой тип, кроме RELATIVE — ошибка.
fn apply_relative(r_info: u64, loc: *mut u64, value: u64) -> Result<(), ElfError> {
    let r_type = (r_info & 0xFFFF_FFFF) as u32;
    if r_type != R_RELATIVE {
        return Err(ElfError::BadRelocation(r_type));
    }
    // SAFETY: loc — адрес в пределах загруженного образа (релокационная
    // таблица ссылается на смещения, сдвинутые на base - min_v).
    unsafe { loc.write(value) };
    Ok(())
}

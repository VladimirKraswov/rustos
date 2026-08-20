//! Нормализация UEFI memory map в BootInfo (модуль загрузчика).
//!
//! UEFI передаёт карту в формате `EFI_MEMORY_DESCRIPTOR` (виртуальные
//! адреса + атрибуты). Ядру нужны только физический адрес, размер и тип —
//! см. `rustos_abi::memmap`. Здесь карта нормализуется, соседние регионы
//! одного типа сливаются, чтобы гарантированно уместиться в
//! `MEMMAP_MAX_REGIONS`.

use rustos_abi::{BootInfo, MemRegion, MemRegionKind, MEMMAP_MAX_REGIONS};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};

/// Перевод UEFI-типа региона в тип ABI.
fn kind_of(ty: MemoryType) -> u32 {
    match ty {
        MemoryType::CONVENTIONAL => MemRegionKind::Usable as u32,
        MemoryType::ACPI_RECLAIM => MemRegionKind::AcpiReclaim as u32,
        MemoryType::ACPI_NON_VOLATILE => MemRegionKind::AcpiNvs as u32,
        MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => MemRegionKind::Mmio as u32,
        MemoryType::RUNTIME_SERVICES_CODE | MemoryType::RUNTIME_SERVICES_DATA => {
            MemRegionKind::RuntimeServices as u32
        }
        _ => MemRegionKind::Reserved as u32,
    }
}

/// Нормализация карты: массив регионов + количество.
///
/// Соседние (по физическому адресу) регионы одного типа сливаются.
/// Если после слияния регионов больше `MEMMAP_MAX_REGIONS`, лишние
/// (с конца) отбрасываются — такое количество для OVMF недостижимо.
pub fn normalize_map(map: &MemoryMapOwned) -> ([MemRegion; MEMMAP_MAX_REGIONS], u32) {
    let mut out: [MemRegion; MEMMAP_MAX_REGIONS] = [MemRegion::ZERO; MEMMAP_MAX_REGIONS];
    let mut n = 0usize;

    for d in map.entries() {
        let phys = d.phys_start;
        let size = d.page_count * 4096;
        if size == 0 {
            continue;
        }
        let kind = kind_of(d.ty);
        // Слияние с последним, если он соседний и того же типа.
        if n > 0 && out[n - 1].kind == kind && out[n - 1].phys_start + out[n - 1].size == phys {
            out[n - 1].size += size;
            continue;
        }
        if n >= MEMMAP_MAX_REGIONS {
            break;
        }
        out[n] = MemRegion {
            kind,
            _pad: 0,
            phys_start: phys,
            size,
        };
        n += 1;
    }
    (out, n as u32)
}

/// Запись нормализованной карты в BootInfo, лежащий в памяти.
///
/// # Safety
///
/// `info` — указатель на BootInfo в доступной памяти (резерв загрузчика).
pub unsafe fn write_memmap(
    info: *mut BootInfo,
    regions: &[MemRegion; MEMMAP_MAX_REGIONS],
    count: u32,
) {
    let count = count as usize;
    // SAFETY: info — валидный указатель (контракт вызывающего); memmap —
    // массив MEMMAP_MAX_REGIONS, копируем первые `count` (в пределах);
    // `copy_nonoverlapping` + `addr_of_mut!` — без промежуточного заимствования
    // (autoref `&mut` на deref сырого указателя запрещён).
    unsafe {
        (*info).memmap_count = count as u32;
        let dst = core::ptr::addr_of_mut!((*info).memmap).cast::<MemRegion>();
        core::ptr::copy_nonoverlapping(regions.as_ptr(), dst, count);
        for i in count..MEMMAP_MAX_REGIONS {
            (*info).memmap[i] = MemRegion::ZERO;
        }
    }
}

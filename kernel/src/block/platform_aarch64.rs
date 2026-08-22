//! Выбор block transport на AArch64.
//!
//! Обычный `run-arm` публикует system volume как virtio-mmio, а UTM создаёт
//! нативные drives как virtio-pci. Верхний block/VFS ABI от этого не зависит:
//! выбранный при загрузке transport остаётся владельцем устройства до reboot.

use core::sync::atomic::{AtomicU8, Ordering};

use super::{virtio_mmio, virtio_pci, BlockError, BlockInfo};

const TRANSPORT_NONE: u8 = 0;
const TRANSPORT_MMIO: u8 = 1;
const TRANSPORT_PCI: u8 = 2;

static SELECTED: AtomicU8 = AtomicU8::new(TRANSPORT_NONE);

pub fn initialize() -> Result<BlockInfo, BlockError> {
    if let Ok(info) = virtio_mmio::initialize() {
        SELECTED.store(TRANSPORT_MMIO, Ordering::Release);
        return Ok(info);
    }
    let info = virtio_pci::initialize()?;
    SELECTED.store(TRANSPORT_PCI, Ordering::Release);
    Ok(info)
}

pub fn info() -> Result<BlockInfo, BlockError> {
    route(virtio_mmio::info, virtio_pci::info)
}

pub fn read_block(block: u64, output: &mut [u8; 4096]) -> Result<(), BlockError> {
    match SELECTED.load(Ordering::Acquire) {
        TRANSPORT_MMIO => virtio_mmio::read_block(block, output),
        TRANSPORT_PCI => virtio_pci::read_block(block, output),
        _ => Err(BlockError::Unsupported),
    }
}

pub fn write_block(block: u64, input: &[u8; 4096]) -> Result<(), BlockError> {
    match SELECTED.load(Ordering::Acquire) {
        TRANSPORT_MMIO => virtio_mmio::write_block(block, input),
        TRANSPORT_PCI => virtio_pci::write_block(block, input),
        _ => Err(BlockError::Unsupported),
    }
}

pub fn flush() -> Result<(), BlockError> {
    route(virtio_mmio::flush, virtio_pci::flush)
}

fn route<T>(
    mmio: impl FnOnce() -> Result<T, BlockError>,
    pci: impl FnOnce() -> Result<T, BlockError>,
) -> Result<T, BlockError> {
    match SELECTED.load(Ordering::Acquire) {
        TRANSPORT_MMIO => mmio(),
        TRANSPORT_PCI => pci(),
        _ => Err(BlockError::Unsupported),
    }
}

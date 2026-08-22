//! Минимальное обнаружение modern virtio-pci display function.
//!
//! Пока PC platform использует один PCI segment и bus 0, как QEMU q35.
//! Код не закрепляет slot/function: просматриваются все функции bus 0. В будущем
//! enumerator PCI bridges/IOMMU станет отдельным сервисом и передаст драйверу
//! уже проверенные BAR capabilities.

use crate::arch;

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_GPU_MODERN: u16 = 0x1050;
const PCI_STATUS_CAPABILITIES: u16 = 1 << 4;
const PCI_CAP_VENDOR: u8 = 0x09;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

#[derive(Clone, Copy, Debug)]
pub struct VirtioPciRegions {
    pub common: u64,
    pub common_length: u32,
    pub notify: u64,
    pub notify_length: u32,
    pub notify_multiplier: u32,
    pub isr: u64,
    pub device: u64,
    pub device_length: u32,
}

#[derive(Clone, Copy)]
struct Function {
    bus: u8,
    slot: u8,
    function: u8,
}

impl Function {
    fn read_u32(self, offset: u8) -> u32 {
        let address = 0x8000_0000u32
            | (u32::from(self.bus) << 16)
            | (u32::from(self.slot) << 11)
            | (u32::from(self.function) << 8)
            | u32::from(offset & 0xfc);
        unsafe {
            arch::outl(0xcf8, address);
            arch::inl(0xcfc)
        }
    }

    fn write_u32(self, offset: u8, value: u32) {
        let address = 0x8000_0000u32
            | (u32::from(self.bus) << 16)
            | (u32::from(self.slot) << 11)
            | u32::from(offset & 0xfc);
        unsafe {
            arch::outl(0xcf8, address);
            arch::outl(0xcfc, value);
        }
    }

    fn read_u16(self, offset: u8) -> u16 {
        let shift = u32::from(offset & 2) * 8;
        (self.read_u32(offset) >> shift) as u16
    }

    fn read_u8(self, offset: u8) -> u8 {
        let shift = u32::from(offset & 3) * 8;
        (self.read_u32(offset) >> shift) as u8
    }

    fn bar(self, index: u8) -> Option<u64> {
        if index >= 6 {
            return None;
        }
        let offset = 0x10 + index * 4;
        let low = self.read_u32(offset);
        if low == 0 || low == u32::MAX || low & 1 != 0 {
            return None;
        }
        let kind = (low >> 1) & 3;
        let mut address = u64::from(low & !0x0f);
        if kind == 2 {
            if index == 5 {
                return None;
            }
            address |= u64::from(self.read_u32(offset + 4)) << 32;
        } else if kind != 0 {
            return None;
        }
        (address != 0).then_some(address)
    }
}

pub fn discover_virtio_gpu() -> Option<VirtioPciRegions> {
    for slot in 0..32 {
        let first = Function {
            bus: 0,
            slot,
            function: 0,
        };
        if first.read_u16(0) == u16::MAX {
            continue;
        }
        let function_count = if first.read_u8(0x0e) & 0x80 != 0 {
            8
        } else {
            1
        };
        for function_index in 0..function_count {
            let function = Function {
                bus: 0,
                slot,
                function: function_index,
            };
            let id = function.read_u32(0);
            if id as u16 != VIRTIO_VENDOR || (id >> 16) as u16 != VIRTIO_GPU_MODERN {
                continue;
            }
            // Memory space + bus mastering; interrupts пока не используются,
            // completion controlq читается polling'ом.
            let command_status = function.read_u32(0x04);
            // Верхние 16 бит PCI status содержат write-one-to-clear flags: их
            // нельзя бездумно записывать обратно вместе с command.
            function.write_u32(0x04, (command_status & 0xffff) | 0x0000_0006);
            if let Some(regions) = parse_capabilities(function) {
                return Some(regions);
            }
        }
    }
    None
}

fn parse_capabilities(function: Function) -> Option<VirtioPciRegions> {
    if function.read_u16(0x06) & PCI_STATUS_CAPABILITIES == 0 {
        return None;
    }
    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = 0;
    let mut isr = None;
    let mut device = None;
    let mut cursor = function.read_u8(0x34) & !3;
    for _ in 0..48 {
        if !(0x40..=0xf0).contains(&cursor) || cursor & 3 != 0 {
            break;
        }
        let id = function.read_u8(cursor);
        let next = function.read_u8(cursor + 1) & !3;
        let length = function.read_u8(cursor + 2);
        if u16::from(cursor) + u16::from(length) > 256 {
            break;
        }
        if id == PCI_CAP_VENDOR && length >= 16 {
            let config_type = function.read_u8(cursor + 3);
            let bar_index = function.read_u8(cursor + 4);
            let offset = function.read_u32(cursor + 8);
            let region_length = function.read_u32(cursor + 12);
            if let Some(bar) = function.bar(bar_index) {
                if let Some(address) = bar.checked_add(u64::from(offset)) {
                    match config_type {
                        VIRTIO_PCI_CAP_COMMON_CFG if region_length >= 56 => {
                            common = Some((address, region_length));
                        }
                        VIRTIO_PCI_CAP_NOTIFY_CFG if length >= 20 && region_length >= 2 => {
                            notify = Some((address, region_length));
                            notify_multiplier = function.read_u32(cursor + 16);
                        }
                        VIRTIO_PCI_CAP_ISR_CFG if region_length >= 1 => isr = Some(address),
                        VIRTIO_PCI_CAP_DEVICE_CFG if region_length >= 16 => {
                            device = Some((address, region_length));
                        }
                        _ => {}
                    }
                }
            }
        }
        if next == 0 || next == cursor {
            break;
        }
        cursor = next;
    }
    let (common, common_length) = common?;
    let (notify, notify_length) = notify?;
    let (device, device_length) = device?;
    Some(VirtioPciRegions {
        common,
        common_length,
        notify,
        notify_length,
        notify_multiplier,
        isr: isr?,
        device,
        device_length,
    })
}

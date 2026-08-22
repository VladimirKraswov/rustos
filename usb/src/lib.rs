//! Переносимая часть USB host stack RustOS.
//!
//! Этот crate не знает о MMIO, PCI, DMA и xHCI. Он целиком проверяет входные
//! дескрипторы устройства и декодирует фиксированные HID reports. Такое
//! разделение важно для микроядра: позднее тот же код без изменений переедет
//! в изолированный `usbd`, а kernel сохранит только IRQ/IOMMU capabilities.

#![no_std]

/// USB class Human Interface Device.
pub const CLASS_HID: u8 = 3;
/// Boot Interface Subclass.
pub const SUBCLASS_BOOT: u8 = 1;
/// Boot keyboard interface protocol.
pub const PROTOCOL_KEYBOARD: u8 = 1;
/// Boot mouse interface protocol.
pub const PROTOCOL_MOUSE: u8 = 2;

/// Поддерживаемый HID interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HidKind {
    Keyboard,
    RelativePointer,
    /// Абсолютный USB tablet, который UTM/QEMU использует для capture-free
    /// указателя. Его шесть байт описаны стандартным HID report descriptor.
    AbsolutePointer,
}

/// Данные, необходимые host-controller driver для настройки interrupt IN pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HidInterface {
    pub kind: HidKind,
    pub configuration_value: u8,
    pub interface_number: u8,
    pub endpoint_address: u8,
    pub interval: u8,
    pub max_packet_size: u16,
}

/// Ошибка недоверенного потока USB descriptors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorError {
    Truncated,
    InvalidLength,
    InvalidConfiguration,
    Unsupported,
}

/// Проверяет device descriptor и возвращает `bMaxPacketSize0`.
pub fn endpoint_zero_packet_size(descriptor: &[u8]) -> Result<u16, DescriptorError> {
    if descriptor.len() < 18 {
        return Err(DescriptorError::Truncated);
    }
    if descriptor[0] < 18 || descriptor[1] != 1 {
        return Err(DescriptorError::InvalidLength);
    }
    match descriptor[7] {
        8 | 16 | 32 | 64 => Ok(u16::from(descriptor[7])),
        // SuperSpeed кодирует 512 байт как exponent 9.
        9 if u16::from_le_bytes([descriptor[2], descriptor[3]]) >= 0x0300 => Ok(512),
        _ => Err(DescriptorError::Unsupported),
    }
}

/// Находит первый поддерживаемый HID interface с interrupt IN endpoint.
///
/// Все `bLength` и `wTotalLength` проверяются до чтения полей. Неизвестные
/// descriptors пропускаются: HID descriptor между interface и endpoint —
/// нормальная часть конфигурации, а не ошибка парсера.
pub fn find_hid_interface(bytes: &[u8]) -> Result<HidInterface, DescriptorError> {
    if bytes.len() < 9 {
        return Err(DescriptorError::Truncated);
    }
    if bytes[0] < 9 || bytes[1] != 2 {
        return Err(DescriptorError::InvalidConfiguration);
    }
    let total = usize::from(u16::from_le_bytes([bytes[2], bytes[3]]));
    if total < 9 || total > bytes.len() {
        return Err(DescriptorError::Truncated);
    }
    let configuration_value = bytes[5];
    if configuration_value == 0 {
        return Err(DescriptorError::InvalidConfiguration);
    }

    let mut interface = None;
    let mut offset = 0usize;
    while offset < total {
        if offset + 2 > total {
            return Err(DescriptorError::Truncated);
        }
        let length = usize::from(bytes[offset]);
        if length < 2 || offset.checked_add(length).is_none_or(|end| end > total) {
            return Err(DescriptorError::InvalidLength);
        }
        match bytes[offset + 1] {
            4 if length >= 9 => {
                let class = bytes[offset + 5];
                let subclass = bytes[offset + 6];
                let protocol = bytes[offset + 7];
                interface = if class != CLASS_HID {
                    None
                } else if subclass == SUBCLASS_BOOT {
                    match protocol {
                        PROTOCOL_KEYBOARD => Some((HidKind::Keyboard, bytes[offset + 2], true)),
                        PROTOCOL_MOUSE => Some((HidKind::RelativePointer, bytes[offset + 2], true)),
                        _ => None,
                    }
                } else if subclass == 0 && protocol == 0 {
                    // UTM предоставляет `usb-tablet` как report-protocol HID
                    // без Boot subclass. На этом этапе принимаем только его
                    // компактный interrupt report; произвольные HID layouts
                    // появятся вместе с report-descriptor interpreter в usbd.
                    Some((HidKind::AbsolutePointer, bytes[offset + 2], false))
                } else {
                    None
                };
            }
            0x21 if length >= 9 => {
                if let Some((HidKind::AbsolutePointer, _, layout_supported)) = interface.as_mut() {
                    let descriptor_kind = bytes[offset + 6];
                    let descriptor_length =
                        u16::from_le_bytes([bytes[offset + 7], bytes[offset + 8]]);
                    // Точная длина является частью стабильного QEMU
                    // usb-tablet layout. Неизвестный report-protocol HID
                    // нельзя ошибочно декодировать как координаты и кнопки.
                    *layout_supported = descriptor_kind == 0x22 && descriptor_length == 74;
                }
            }
            5 if length >= 7 => {
                let address = bytes[offset + 2];
                let attributes = bytes[offset + 3] & 0x03;
                if let Some((kind, interface_number, layout_supported)) = interface {
                    if layout_supported && address & 0x80 != 0 && attributes == 0x03 {
                        let max_packet_size =
                            u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]]) & 0x07ff;
                        let minimum_packet_size = match kind {
                            HidKind::Keyboard => 8,
                            HidKind::RelativePointer => 3,
                            HidKind::AbsolutePointer => 6,
                        };
                        if max_packet_size < minimum_packet_size || max_packet_size > 1024 {
                            return Err(DescriptorError::Unsupported);
                        }
                        return Ok(HidInterface {
                            kind,
                            configuration_value,
                            interface_number,
                            endpoint_address: address,
                            interval: bytes[offset + 6].max(1),
                            max_packet_size,
                        });
                    }
                }
            }
            _ => {}
        }
        offset += length;
    }
    Err(DescriptorError::Unsupported)
}

/// Снимок стандартного восьмибайтного Boot Keyboard report.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyboardReport {
    pub modifiers: u8,
    pub usages: [u8; 6],
}

impl KeyboardReport {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 8 || bytes[2..8].contains(&1) {
            return None;
        }
        let mut usages = [0u8; 6];
        usages.copy_from_slice(&bytes[2..8]);
        Some(Self {
            modifiers: bytes[0],
            usages,
        })
    }

    /// Вызывает callback только для новых нажатий. Ноль и дубликаты отчёта
    /// исключаются; release хранится в следующем snapshot, но не порождает
    /// текстовый ввод сам по себе.
    pub fn newly_pressed(self, previous: Self, mut emit: impl FnMut(u8)) {
        for (index, usage) in self.usages.into_iter().enumerate() {
            if usage == 0
                || self.usages[..index].contains(&usage)
                || previous.usages.contains(&usage)
            {
                continue;
            }
            emit(usage);
        }
    }

    pub const fn shift(self) -> bool {
        self.modifiers & ((1 << 1) | (1 << 5)) != 0
    }
}

/// Boot Mouse report. Wheel/extra buttons являются совместимым расширением:
/// базовое устройство может прислать только первые три байта.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MouseReport {
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    pub wheel: i16,
}

/// Шестибайтный report абсолютного USB HID Tablet в QEMU/UTM.
///
/// Координаты задаются в диапазоне `0..=32767` независимо от размера экрана.
/// Преобразование в пиксели выполняет window server: только он знает текущий
/// видеорежим и не должен получать host-координаты через ABI драйвера.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AbsolutePointerReport {
    pub buttons: u8,
    pub x: u16,
    pub y: u16,
    pub wheel: i16,
}

impl AbsolutePointerReport {
    pub const MAXIMUM_COORDINATE: u16 = 0x7fff;

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 6 {
            return None;
        }
        let x = u16::from_le_bytes([bytes[1], bytes[2]]);
        let y = u16::from_le_bytes([bytes[3], bytes[4]]);
        if x > Self::MAXIMUM_COORDINATE || y > Self::MAXIMUM_COORDINATE {
            return None;
        }
        Some(Self {
            buttons: bytes[0] & 0x1f,
            x,
            y,
            wheel: i16::from(bytes[5] as i8),
        })
    }
}

impl MouseReport {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 3 {
            return None;
        }
        Some(Self {
            buttons: bytes[0] & 0x1f,
            dx: i16::from(bytes[1] as i8),
            dy: i16::from(bytes[2] as i8),
            wheel: bytes.get(3).map_or(0, |value| i16::from(*value as i8)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYBOARD_CONFIGURATION: [u8; 34] = [
        9, 2, 34, 0, 1, 1, 0, 0xa0, 50, 9, 4, 0, 0, 1, 3, 1, 1, 0, 9, 0x21, 0x11, 0x01, 0, 1, 0x22,
        63, 0, 7, 5, 0x81, 3, 8, 0, 10,
    ];
    const TABLET_CONFIGURATION: [u8; 34] = [
        9, 2, 34, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 0, 0, 0, 9, 0x21, 0x11, 0x01, 0, 1, 0x22,
        74, 0, 7, 5, 0x81, 3, 8, 0, 10,
    ];

    #[test]
    fn parses_boot_keyboard_around_hid_descriptor() {
        assert_eq!(
            find_hid_interface(&KEYBOARD_CONFIGURATION),
            Ok(HidInterface {
                kind: HidKind::Keyboard,
                configuration_value: 1,
                interface_number: 0,
                endpoint_address: 0x81,
                interval: 10,
                max_packet_size: 8,
            })
        );
    }

    #[test]
    fn malformed_descriptor_never_walks_past_total_length() {
        let mut malformed = KEYBOARD_CONFIGURATION;
        malformed[18] = 40;
        assert_eq!(
            find_hid_interface(&malformed),
            Err(DescriptorError::InvalidLength)
        );
        malformed = KEYBOARD_CONFIGURATION;
        malformed[2] = 0xff;
        malformed[3] = 0xff;
        assert_eq!(
            find_hid_interface(&malformed),
            Err(DescriptorError::Truncated)
        );
    }

    #[test]
    fn keyboard_emits_only_edges_and_rejects_rollover() {
        let previous = KeyboardReport::decode(&[0, 0, 4, 0, 0, 0, 0, 0]).unwrap();
        let current = KeyboardReport::decode(&[2, 0, 4, 5, 5, 0, 0, 0]).unwrap();
        let mut pressed = [0u8; 6];
        let mut count = 0;
        current.newly_pressed(previous, |usage| {
            pressed[count] = usage;
            count += 1;
        });
        assert_eq!(&pressed[..count], &[5]);
        assert!(current.shift());
        assert!(KeyboardReport::decode(&[0, 0, 1, 1, 1, 1, 1, 1]).is_none());
    }

    #[test]
    fn mouse_accepts_three_or_four_byte_reports() {
        assert_eq!(
            MouseReport::decode(&[5, 0xfe, 3]),
            Some(MouseReport {
                buttons: 5,
                dx: -2,
                dy: 3,
                wheel: 0,
            })
        );
        assert_eq!(MouseReport::decode(&[0, 0, 0, 0xff]).unwrap().wheel, -1);
        assert!(MouseReport::decode(&[0, 0]).is_none());
    }

    #[test]
    fn parses_and_decodes_qemu_absolute_tablet() {
        assert_eq!(
            find_hid_interface(&TABLET_CONFIGURATION),
            Ok(HidInterface {
                kind: HidKind::AbsolutePointer,
                configuration_value: 1,
                interface_number: 0,
                endpoint_address: 0x81,
                interval: 10,
                max_packet_size: 8,
            })
        );
        assert_eq!(
            AbsolutePointerReport::decode(&[5, 0x34, 0x12, 0xff, 0x7f, 0xff]),
            Some(AbsolutePointerReport {
                buttons: 5,
                x: 0x1234,
                y: 0x7fff,
                wheel: -1,
            })
        );
        assert!(AbsolutePointerReport::decode(&[0, 0, 0, 0, 0]).is_none());
        assert!(AbsolutePointerReport::decode(&[0, 0, 0x80, 0, 0, 0]).is_none());

        let mut unknown_layout = TABLET_CONFIGURATION;
        unknown_layout[25] = 73;
        assert_eq!(
            find_hid_interface(&unknown_layout),
            Err(DescriptorError::Unsupported)
        );
    }
}

//! Bounded EDID 1.x parser для выбора monitor-native режима.
//!
//! Нам не нужен универсальный userspace parser на bootstrap-этапе: драйвер
//! извлекает физический размер, detailed timings и standard timings, строго
//! проверяя checksum. Неизвестные extensions безопасно игнорируются.

pub const MAX_EDID_MODES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdidMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihertz: u32,
    pub preferred: bool,
}

impl EdidMode {
    const ZERO: Self = Self {
        width: 0,
        height: 0,
        refresh_millihertz: 0,
        preferred: false,
    };
}

pub struct EdidInfo {
    pub width_mm: u16,
    pub height_mm: u16,
    pub modes: [EdidMode; MAX_EDID_MODES],
    pub mode_count: usize,
}

impl EdidInfo {
    fn empty() -> Self {
        Self {
            width_mm: 0,
            height_mm: 0,
            modes: [EdidMode::ZERO; MAX_EDID_MODES],
            mode_count: 0,
        }
    }

    fn add(&mut self, mode: EdidMode) {
        if mode.width < 640 || mode.height < 480 || mode.width > 7680 || mode.height > 4320 {
            return;
        }
        if let Some(existing) = self.modes[..self.mode_count]
            .iter_mut()
            .find(|entry| entry.width == mode.width && entry.height == mode.height)
        {
            if mode.preferred || existing.refresh_millihertz == 0 {
                *existing = mode;
            }
            return;
        }
        if self.mode_count < MAX_EDID_MODES {
            self.modes[self.mode_count] = mode;
            self.mode_count += 1;
        }
    }
}

pub fn parse(bytes: &[u8]) -> Option<EdidInfo> {
    if bytes.len() < 128
        || bytes[..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]
        || bytes[..128]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
            != 0
    {
        return None;
    }
    let mut info = EdidInfo::empty();
    info.width_mm = u16::from(bytes[21]) * 10;
    info.height_mm = u16::from(bytes[22]) * 10;

    // Первый detailed timing является preferred timing для EDID 1.3+.
    for index in 0..4 {
        let offset = 54 + index * 18;
        if let Some(mut mode) = detailed_timing(&bytes[offset..offset + 18]) {
            mode.preferred = index == 0;
            info.add(mode);
        }
    }

    for timing in bytes[38..54].as_chunks::<2>().0 {
        if *timing == [0x01, 0x01] {
            continue;
        }
        let width = (u32::from(timing[0]) + 31) * 8;
        let height = match timing[1] >> 6 {
            0 => width * 10 / 16,
            1 => width * 3 / 4,
            2 => width * 4 / 5,
            _ => width * 9 / 16,
        };
        info.add(EdidMode {
            width,
            height,
            refresh_millihertz: (60 + u32::from(timing[1] & 0x3f)) * 1000,
            preferred: false,
        });
    }
    Some(info)
}

fn detailed_timing(bytes: &[u8]) -> Option<EdidMode> {
    if bytes.len() < 18 {
        return None;
    }
    let pixel_clock_hz = u64::from(u16::from_le_bytes([bytes[0], bytes[1]])) * 10_000;
    if pixel_clock_hz == 0 {
        return None;
    }
    let horizontal_active = u32::from(bytes[2]) | (u32::from(bytes[4] & 0xf0) << 4);
    let horizontal_blank = u32::from(bytes[3]) | (u32::from(bytes[4] & 0x0f) << 8);
    let vertical_active = u32::from(bytes[5]) | (u32::from(bytes[7] & 0xf0) << 4);
    let vertical_blank = u32::from(bytes[6]) | (u32::from(bytes[7] & 0x0f) << 8);
    let total = u64::from(horizontal_active.checked_add(horizontal_blank)?)
        .checked_mul(u64::from(vertical_active.checked_add(vertical_blank)?))?;
    let refresh_millihertz = pixel_clock_hz.checked_mul(1000)?.checked_div(total)? as u32;
    Some(EdidMode {
        width: horizontal_active,
        height: vertical_active,
        refresh_millihertz,
        preferred: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_checksum() {
        let mut bytes = [0u8; 128];
        bytes[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        assert!(parse(&bytes).is_none());
    }

    #[test]
    fn decodes_detailed_widescreen_timing() {
        // 1280x720: только поля, которые использует parser. Checksum
        // вычисляется после заполнения base block.
        let mut bytes = [0u8; 128];
        bytes[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        bytes[18] = 1;
        bytes[19] = 4;
        bytes[54..72].copy_from_slice(&[
            0x01, 0x1d, 0x00, 0x72, 0x51, 0xd0, 0x1e, 0x20, 0x6e, 0x28, 0x55, 0x00, 0, 0, 0, 0, 0,
            0,
        ]);
        bytes[127] = 0u8.wrapping_sub(
            bytes[..127]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        );
        let info = parse(&bytes).expect("valid EDID");
        assert_eq!((info.modes[0].width, info.modes[0].height), (1280, 720));
        assert!(info.modes[0].preferred);
    }
}

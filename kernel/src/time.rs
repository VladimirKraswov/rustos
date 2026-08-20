//! Календарные часы поверх архитектурного монотонного таймера.
//!
//! Планировщик и таймауты всегда используют monotonic clock: перевод
//! календарных часов не может сдвинуть deadline назад. Этот модуль отвечает
//! только за человекочитаемые дату и время. На PC источником служит CMOS RTC;
//! на платформах без подключённого RTC UI честно показывает uptime.

#[cfg(target_arch = "x86_64")]
use crate::arch;

const CLOCK_POLL_MS: u64 = 1_000;

/// Проверенное календарное значение. Часовой пояс намеренно не спрятан в
/// структуру: CMOS/QEMU сейчас трактуется как UTC, а timezone service будет
/// отдельной пользовательской policy поверх этого источника.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalendarTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl CalendarTime {
    fn same_visible_minute(self, other: Self) -> bool {
        self.year == other.year
            && self.month == other.month
            && self.day == other.day
            && self.hour == other.hour
            && self.minute == other.minute
    }

    #[cfg(target_arch = "x86_64")]
    fn valid(self) -> bool {
        if self.year < 1970
            || self.month == 0
            || self.month > 12
            || self.day == 0
            || self.hour > 23
            || self.minute > 59
            || self.second > 59
        {
            return false;
        }
        self.day <= days_in_month(self.year, self.month)
    }
}

/// Небольшая строка без heap. В неё попадают только ASCII-цифры и системные
/// подписи, поэтому `as_str` не может получить невалидный UTF-8.
#[derive(Clone, Copy)]
struct FixedText {
    bytes: [u8; 16],
    len: usize,
}

impl FixedText {
    const fn new() -> Self {
        Self {
            bytes: [0; 16],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, byte: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes.iter().copied() {
            self.push(byte);
        }
    }

    fn push_two_digits(&mut self, value: u8) {
        self.push(b'0' + value / 10);
        self.push(b'0' + value % 10);
    }

    fn push_four_digits(&mut self, value: u16) {
        self.push(b'0' + ((value / 1_000) % 10) as u8);
        self.push(b'0' + ((value / 100) % 10) as u8);
        self.push(b'0' + ((value / 10) % 10) as u8);
        self.push(b'0' + (value % 10) as u8);
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

/// Кэш wall clock для UI. Аппаратный RTC читается не чаще раза в секунду,
/// а перерисовка требуется только при смене видимой минуты или даты.
pub struct SystemClock {
    calendar: Option<CalendarTime>,
    next_poll_ms: u64,
    last_uptime_minute: u64,
    time_text: FixedText,
    date_text: FixedText,
}

impl SystemClock {
    pub fn new(now_ms: u64) -> Self {
        let mut result = Self {
            calendar: None,
            next_poll_ms: now_ms.saturating_add(CLOCK_POLL_MS),
            last_uptime_minute: now_ms / 60_000,
            time_text: FixedText::new(),
            date_text: FixedText::new(),
        };
        result.calendar = read_wall_clock();
        result.format(now_ms);
        result
    }

    /// Обновляет кэш и сообщает, изменились ли символы на taskbar.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if now_ms < self.next_poll_ms {
            return false;
        }
        self.next_poll_ms = now_ms.saturating_add(CLOCK_POLL_MS);
        let previous = self.calendar;
        self.calendar = read_wall_clock();
        let uptime_minute = now_ms / 60_000;
        let changed = match (previous, self.calendar) {
            (Some(before), Some(after)) => !before.same_visible_minute(after),
            (None, None) => uptime_minute != self.last_uptime_minute,
            _ => true,
        };
        self.last_uptime_minute = uptime_minute;
        if changed {
            self.format(now_ms);
        }
        changed
    }

    pub fn time_text(&self) -> &str {
        self.time_text.as_str()
    }

    pub fn date_text(&self) -> &str {
        self.date_text.as_str()
    }

    pub const fn source_name(&self) -> &'static str {
        if self.calendar.is_some() {
            "cmos-rtc"
        } else {
            "monotonic-uptime"
        }
    }

    fn format(&mut self, now_ms: u64) {
        self.time_text.clear();
        self.date_text.clear();
        if let Some(value) = self.calendar {
            self.time_text.push_two_digits(value.hour);
            self.time_text.push(b':');
            self.time_text.push_two_digits(value.minute);
            self.date_text.push_two_digits(value.day);
            self.date_text.push(b'.');
            self.date_text.push_two_digits(value.month);
            self.date_text.push(b'.');
            self.date_text.push_four_digits(value.year);
        } else {
            let total_minutes = now_ms / 60_000;
            let hours = ((total_minutes / 60) % 100) as u8;
            let minutes = (total_minutes % 60) as u8;
            self.time_text.push_bytes(b"UP ");
            self.time_text.push_two_digits(hours);
            self.time_text.push(b':');
            self.time_text.push_two_digits(minutes);
            self.date_text.push_bytes(b"RTC NOT FOUND");
        }
    }
}

#[cfg(target_arch = "x86_64")]
const fn leap_year(year: u16) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

#[cfg(target_arch = "x86_64")]
const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Eq, PartialEq)]
struct RawRtc {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
    status_b: u8,
}

#[cfg(target_arch = "x86_64")]
fn read_wall_clock() -> Option<CalendarTime> {
    // Два одинаковых snapshot исключают смесь значений по разные стороны
    // секундного rollover. Число попыток ограничено: неисправный RTC не может
    // навсегда остановить compositor.
    let mut before = read_raw_rtc()?;
    for _ in 0..4 {
        let after = read_raw_rtc()?;
        if before == after {
            return decode_rtc(after);
        }
        before = after;
    }
    None
}

#[cfg(not(target_arch = "x86_64"))]
fn read_wall_clock() -> Option<CalendarTime> {
    // ARM-платы предоставляют RTC через ACPI/Device Tree и MMIO. До выбора
    // конкретного устройства нельзя угадывать адрес регистра.
    None
}

#[cfg(target_arch = "x86_64")]
fn read_raw_rtc() -> Option<RawRtc> {
    let mut ready = false;
    for _ in 0..10_000 {
        if read_cmos(0x0a) & 0x80 == 0 {
            ready = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !ready {
        return None;
    }
    Some(RawRtc {
        second: read_cmos(0x00),
        minute: read_cmos(0x02),
        hour: read_cmos(0x04),
        day: read_cmos(0x07),
        month: read_cmos(0x08),
        year: read_cmos(0x09),
        century: read_cmos(0x32),
        status_b: read_cmos(0x0b),
    })
}

#[cfg(target_arch = "x86_64")]
fn read_cmos(register: u8) -> u8 {
    // Bit 7 оставляем нулевым: после доступа NMI снова разрешены. Ни один
    // другой kernel subsystem CMOS index port сейчас не использует.
    unsafe {
        arch::outb(0x70, register & 0x7f);
        arch::inb(0x71)
    }
}

#[cfg(target_arch = "x86_64")]
fn decode_rtc(raw: RawRtc) -> Option<CalendarTime> {
    let binary = raw.status_b & 0x04 != 0;
    let hour_24 = raw.status_b & 0x02 != 0;
    let decode = |value: u8| {
        if binary {
            value
        } else {
            (value & 0x0f).saturating_add((value >> 4).saturating_mul(10))
        }
    };
    let pm = raw.hour & 0x80 != 0;
    let encoded_hour = raw.hour & 0x7f;
    let mut hour = decode(encoded_hour);
    if !hour_24 {
        hour %= 12;
        if pm {
            hour = hour.saturating_add(12);
        }
    }
    let short_year = decode(raw.year);
    let decoded_century = decode(raw.century);
    let century = if (19..=99).contains(&decoded_century) {
        u16::from(decoded_century)
    } else {
        20
    };
    let result = CalendarTime {
        year: century
            .saturating_mul(100)
            .saturating_add(u16::from(short_year)),
        month: decode(raw.month),
        day: decode(raw.day),
        hour,
        minute: decode(raw.minute),
        second: decode(raw.second),
    };
    result.valid().then_some(result)
}

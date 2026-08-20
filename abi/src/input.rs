//! Стабильные типы настройки указателя и курсора.
//!
//! Аппаратный драйвер применяет те поля, которые умеет устройство, а
//! оставшиеся (чувствительность, ускорение и распознавание жестов) реализует
//! input service. Поэтому одно приложение настроек работает и с PS/2, и с
//! будущими USB HID/virtio-input драйверами.

/// Версия ABI настроек мыши.
pub const MOUSE_SETTINGS_VERSION: u16 = 1;

/// Желаемый вид системного курсора.
///
/// Приложение задаёт только семантику. Конкретное изображение, hotspot и
/// анимацию выбирает активный cursor pack оконного сервера.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PointerCursor {
    /// Обычная стрелка.
    #[default]
    Arrow = 0,
    /// Ввод или выделение текста.
    Text = 1,
    /// Ссылка или другая явно нажимаемая область.
    Link = 2,
    /// Объект можно схватить.
    Grab = 3,
    /// Объект уже перетаскивается.
    Grabbing = 4,
    /// Приложение занято; курсор анимируется оконным сервером.
    Busy = 5,
    /// Точное указание точки.
    Crosshair = 6,
    /// Действие запрещено.
    NotAllowed = 7,
    /// Изменение ширины.
    ResizeHorizontal = 8,
    /// Изменение высоты.
    ResizeVertical = 9,
    /// Диагональное изменение размера: северо-запад — юго-восток.
    ResizeNwSe = 10,
    /// Диагональное изменение размера: северо-восток — юго-запад.
    ResizeNeSw = 11,
}

/// Настройки мыши, передаваемые input service целиком одной версированной
/// структурой.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MouseSettings {
    /// Версия структуры, сейчас [`MOUSE_SETTINGS_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах для безопасного расширения ABI.
    pub size: u16,
    /// Частота отчётов устройства. Для PS/2: 10/20/40/60/80/100/200 Гц.
    pub sample_rate_hz: u16,
    /// Аппаратное разрешение PS/2: 0..3 (1, 2, 4 или 8 counts/mm).
    pub resolution_level: u8,
    /// Зарезервировано для left-handed/scroll flags следующей версии.
    pub flags: u8,
    /// Линейная чувствительность в процентах, 100 = без изменения.
    pub sensitivity_percent: u16,
    /// Дополнительное ускорение быстрого движения в процентах.
    pub acceleration_percent: u16,
    /// Максимальный интервал между двумя кликами.
    pub double_click_ms: u16,
    /// Защита контакта кнопки от повторного одиночного клика.
    pub click_debounce_ms: u16,
    /// Смещение до начала drag вместо клика.
    pub drag_threshold_px: u16,
    /// Зарезервировано; должно быть равно нулю.
    pub reserved: u16,
}

impl MouseSettings {
    /// Безопасный профиль для desktop: достаточно отзывчивый, но без
    /// случайных двойных кликов и drag на дрожащей руке.
    pub const DEFAULT: Self = Self {
        version: MOUSE_SETTINGS_VERSION,
        size: core::mem::size_of::<Self>() as u16,
        sample_rate_hz: 100,
        resolution_level: 2,
        flags: 0,
        sensitivity_percent: 100,
        // Нулевой default сохраняет 1:1 движение и предсказуемые тесты.
        // Пользователь включает acceleration явно под своё устройство.
        acceleration_percent: 0,
        double_click_ms: 450,
        click_debounce_ms: 25,
        drag_threshold_px: 5,
        reserved: 0,
    };

    /// Ограничивает поля диапазонами, которые input service гарантированно
    /// может обработать без переполнения и неоднозначной семантики.
    pub const fn sanitized(mut self) -> Self {
        self.version = MOUSE_SETTINGS_VERSION;
        self.size = core::mem::size_of::<Self>() as u16;
        self.sample_rate_hz = nearest_ps2_rate(self.sample_rate_hz);
        self.resolution_level = if self.resolution_level > 3 {
            3
        } else {
            self.resolution_level
        };
        self.sensitivity_percent = clamp_u16(self.sensitivity_percent, 25, 400);
        self.acceleration_percent = clamp_u16(self.acceleration_percent, 0, 300);
        self.double_click_ms = clamp_u16(self.double_click_ms, 100, 1_200);
        self.click_debounce_ms = clamp_u16(self.click_debounce_ms, 0, 250);
        self.drag_threshold_px = clamp_u16(self.drag_threshold_px, 1, 32);
        self.reserved = 0;
        self
    }
}

impl Default for MouseSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Возможности активного драйвера мыши.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MouseCapabilities {
    /// Можно ли менять аппаратную частоту отчётов.
    pub configurable_sample_rate: u8,
    /// Можно ли менять аппаратное разрешение сенсора.
    pub configurable_resolution: u8,
    /// Есть ли колесо прокрутки.
    pub wheel: u8,
    /// Есть ли две дополнительные боковые кнопки.
    pub extra_buttons: u8,
    /// Минимальная поддерживаемая частота отчётов.
    pub minimum_rate_hz: u16,
    /// Максимальная поддерживаемая частота отчётов.
    pub maximum_rate_hz: u16,
    /// Число уровней аппаратного разрешения.
    pub resolution_levels: u8,
    /// Зарезервировано и равно нулю.
    pub reserved: [u8; 7],
}

/// Ближайшая стандартная частота PS/2, с предпочтением более высокой при
/// одинаковом расстоянии.
pub const fn nearest_ps2_rate(requested: u16) -> u16 {
    const RATES: [u16; 7] = [10, 20, 40, 60, 80, 100, 200];
    let mut best = RATES[0];
    let mut best_distance = requested.abs_diff(best);
    let mut index = 1;
    while index < RATES.len() {
        let candidate = RATES[index];
        let distance = requested.abs_diff(candidate);
        if distance <= best_distance {
            best = candidate;
            best_distance = distance;
        }
        index += 1;
    }
    best
}

const fn clamp_u16(value: u16, minimum: u16, maximum: u16) -> u16 {
    if value < minimum {
        minimum
    } else if value > maximum {
        maximum
    } else {
        value
    }
}

const _: () = assert!(core::mem::size_of::<MouseSettings>() == 20);
const _: () = assert!(core::mem::size_of::<MouseCapabilities>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_is_stable_and_bounded() {
        let settings = MouseSettings {
            sample_rate_hz: 150,
            resolution_level: 9,
            sensitivity_percent: 500,
            acceleration_percent: 999,
            double_click_ms: 20,
            click_debounce_ms: 999,
            drag_threshold_px: 0,
            ..MouseSettings::DEFAULT
        }
        .sanitized();
        assert_eq!(settings.sample_rate_hz, 200);
        assert_eq!(settings.resolution_level, 3);
        assert_eq!(settings.sensitivity_percent, 400);
        assert_eq!(settings.acceleration_percent, 300);
        assert_eq!(settings.double_click_ms, 100);
        assert_eq!(settings.click_debounce_ms, 250);
        assert_eq!(settings.drag_threshold_px, 1);
    }
}

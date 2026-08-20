//! Версионированный ABI между приложением и оконным сервером.
//!
//! Приложение отправляет [`WindowCommand`], а window server возвращает
//! упорядоченные [`WindowEvent`]. Пакеты не содержат указателей и могут
//! без преобразования передаваться через capability IPC/shared-memory queue.

/// Текущая версия оконного ABI.
pub const WINDOW_ABI_VERSION: u16 = 1;

/// Stable handle окна внутри одной GUI-сессии.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowId(pub u64);

impl WindowId {
    /// Создаёт ID. Нулевое значение зарезервировано.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Геометрия окна в логических пикселях desktop.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowRect {
    /// Горизонтальная позиция левого края.
    pub x: i32,
    /// Вертикальная позиция верхнего края.
    pub y: i32,
    /// Ширина; window server применяет minimum/maximum constraints.
    pub width: u32,
    /// Высота; window server применяет minimum/maximum constraints.
    pub height: u32,
}

impl WindowRect {
    /// Создаёт прямоугольник окна.
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Набор флагов поведения и системного оформления окна.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowStyle(pub u32);

impl WindowStyle {
    /// Нет рамки, title bar, кнопок и пользовательского resize.
    pub const FRAMELESS: Self = Self(0);
    /// Отображать рамку.
    pub const BORDER: Self = Self(1 << 0);
    /// Отображать title bar.
    pub const TITLE_BAR: Self = Self(1 << 1);
    /// Разрешить перемещение мышью и API-командой.
    pub const MOVABLE: Self = Self(1 << 2);
    /// Разрешить изменение размера мышью и API-командой.
    pub const RESIZABLE: Self = Self(1 << 3);
    /// Показать кнопку сворачивания.
    pub const BUTTON_MINIMIZE: Self = Self(1 << 4);
    /// Показать кнопку maximize/restore.
    pub const BUTTON_MAXIMIZE: Self = Self(1 << 5);
    /// Показать кнопку закрытия.
    pub const BUTTON_CLOSE: Self = Self(1 << 6);
    /// Обычное окно: рамка, title bar, resize/move и три кнопки.
    pub const STANDARD: Self = Self(
        Self::BORDER.0
            | Self::TITLE_BAR.0
            | Self::MOVABLE.0
            | Self::RESIZABLE.0
            | Self::BUTTON_MINIMIZE.0
            | Self::BUTTON_MAXIMIZE.0
            | Self::BUTTON_CLOSE.0,
    );

    /// Проверяет, что все указанные флаги установлены.
    pub const fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }

    /// Объединяет два набора флагов.
    pub const fn union(self, flags: Self) -> Self {
        Self(self.0 | flags.0)
    }

    /// Возвращает style без указанных флагов. Удобно для настройки окна на
    /// основе [`Self::STANDARD`], например без maximize-кнопки или рамки.
    pub const fn without(self, flags: Self) -> Self {
        Self(self.0 & !flags.0)
    }
}

/// Параметры создания окна. Запрос не содержит указателей: title и surface
/// передаются отдельными capability-сообщениями, а оконный сервер возвращает
/// назначенный [`WindowId`] в первом событии `SHOWN`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowCreateRequest {
    /// Должно равняться [`WINDOW_ABI_VERSION`].
    pub version: u16,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved: u16,
    /// Будущие create options; в ABI v1 должно быть равно нулю.
    pub options: u32,
    /// Желаемая geometry; сервер ограничит её рабочей областью.
    pub rect: WindowRect,
    /// Минимальная ширина окна.
    pub minimum_width: u32,
    /// Минимальная высота окна.
    pub minimum_height: u32,
    /// Decoration/behaviour flags.
    pub style: WindowStyle,
    /// Зарезервировано для ABI v2; отправитель заполняет нулём.
    pub reserved_tail: u32,
}

impl WindowCreateRequest {
    /// Создаёт request с полностью явными geometry и style.
    pub const fn new(
        rect: WindowRect,
        minimum_width: u32,
        minimum_height: u32,
        style: WindowStyle,
    ) -> Self {
        Self {
            version: WINDOW_ABI_VERSION,
            reserved: 0,
            options: 0,
            rect,
            minimum_width,
            minimum_height,
            style,
            reserved_tail: 0,
        }
    }

    /// Обычное окно с рамкой, title bar и тремя системными кнопками.
    pub const fn standard(rect: WindowRect, minimum_width: u32, minimum_height: u32) -> Self {
        Self::new(rect, minimum_width, minimum_height, WindowStyle::STANDARD)
    }
}

/// Коды команд оконному серверу.
pub mod command {
    /// Переместить окно; используются `rect.x/y`.
    pub const MOVE: u16 = 1;
    /// Изменить позицию и размер по `rect`.
    pub const RESIZE: u16 = 2;
    /// Свернуть окно.
    pub const MINIMIZE: u16 = 3;
    /// Развернуть окно на рабочую область.
    pub const MAXIMIZE: u16 = 4;
    /// Восстановить normal/maximized state до сворачивания.
    pub const RESTORE: u16 = 5;
    /// Закрыть окно после обработки `CLOSE_REQUESTED`.
    pub const CLOSE: u16 = 6;
    /// Повторно показать закрытое окно.
    pub const SHOW: u16 = 7;
    /// Заменить [`super::WindowStyle`]; биты передаются в `value`.
    pub const SET_STYLE: u16 = 8;
}

/// Версионированная команда оконному серверу.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowCommand {
    /// Должно равняться [`WINDOW_ABI_VERSION`].
    pub version: u16,
    /// Один из кодов модуля [`command`].
    pub kind: u16,
    /// Зарезервировано; отправитель заполняет нулями.
    pub flags: u32,
    /// Целевое окно.
    pub window: WindowId,
    /// Геометрия для `MOVE`/`RESIZE`, иначе нули.
    pub rect: WindowRect,
    /// Дополнительное значение; `SET_STYLE` передаёт здесь style bits.
    pub value: u64,
}

impl WindowCommand {
    /// Создаёт команду без payload.
    pub const fn simple(window: WindowId, kind: u16) -> Self {
        Self {
            version: WINDOW_ABI_VERSION,
            kind,
            flags: 0,
            window,
            rect: WindowRect::new(0, 0, 0, 0),
            value: 0,
        }
    }

    /// Создаёт команду перемещения.
    pub const fn move_to(window: WindowId, x: i32, y: i32) -> Self {
        let mut command = Self::simple(window, command::MOVE);
        command.rect.x = x;
        command.rect.y = y;
        command
    }

    /// Создаёт команду изменения geometry.
    pub const fn resize(window: WindowId, rect: WindowRect) -> Self {
        let mut command = Self::simple(window, command::RESIZE);
        command.rect = rect;
        command
    }

    /// Создаёт команду изменения оформления.
    pub const fn set_style(window: WindowId, style: WindowStyle) -> Self {
        let mut command = Self::simple(window, command::SET_STYLE);
        command.value = style.0 as u64;
        command
    }

    /// Создаёт команду сворачивания.
    pub const fn minimize(window: WindowId) -> Self {
        Self::simple(window, command::MINIMIZE)
    }

    /// Создаёт команду разворачивания на рабочую область.
    pub const fn maximize(window: WindowId) -> Self {
        Self::simple(window, command::MAXIMIZE)
    }

    /// Создаёт команду восстановления прежнего состояния/размера.
    pub const fn restore(window: WindowId) -> Self {
        Self::simple(window, command::RESTORE)
    }

    /// Подтверждает закрытие после `CLOSE_REQUESTED`.
    pub const fn close(window: WindowId) -> Self {
        Self::simple(window, command::CLOSE)
    }

    /// Повторно показывает закрытое окно.
    pub const fn show(window: WindowId) -> Self {
        Self::simple(window, command::SHOW)
    }
}

/// Коды состояния окна.
pub mod state {
    /// Обычная geometry.
    pub const NORMAL: u16 = 1;
    /// Окно свёрнуто и не рисуется.
    pub const MINIMIZED: u16 = 2;
    /// Окно занимает всю рабочую область.
    pub const MAXIMIZED: u16 = 3;
    /// Окно закрыто; surface можно освободить.
    pub const CLOSED: u16 = 4;
}

/// Коды событий от оконного сервера к приложению.
pub mod event {
    /// Окно показано или создано.
    pub const SHOWN: u16 = 1;
    /// Изменилась позиция.
    pub const MOVED: u16 = 2;
    /// Изменился размер (и, возможно, позиция левого/верхнего края).
    pub const RESIZED: u16 = 3;
    /// Окно свёрнуто.
    pub const MINIMIZED: u16 = 4;
    /// Окно развёрнуто на рабочую область.
    pub const MAXIMIZED: u16 = 5;
    /// Окно восстановлено из minimized/maximized state.
    pub const RESTORED: u16 = 6;
    /// Пользователь нажал close; приложение может сохраниться и ответить `CLOSE`.
    pub const CLOSE_REQUESTED: u16 = 7;
    /// Окно закрыто.
    pub const CLOSED: u16 = 8;
    /// Изменились style/decorations/buttons.
    pub const STYLE_CHANGED: u16 = 9;
}

/// Событие окна с итоговым snapshot state.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowEvent {
    /// Должно равняться [`WINDOW_ABI_VERSION`].
    pub version: u16,
    /// Один из кодов модуля [`event`].
    pub kind: u16,
    /// Один из кодов модуля [`state`].
    pub state: u16,
    /// Зарезервировано; receiver игнорирует.
    pub reserved: u16,
    /// Окно-источник.
    pub window: WindowId,
    /// Геометрия после события.
    pub rect: WindowRect,
    /// Монотонный номер события для обнаружения пропусков.
    pub serial: u64,
}

const _: () = assert!(core::mem::size_of::<WindowCommand>() == 40);
const _: () = assert!(core::mem::size_of::<WindowEvent>() == 40);
const _: () = assert!(core::mem::size_of::<WindowCreateRequest>() == 40);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builders_keep_version_and_payload_stable() {
        let id = WindowId::new(7);
        let resize = WindowCommand::resize(id, WindowRect::new(10, 20, 800, 600));
        assert_eq!(resize.version, WINDOW_ABI_VERSION);
        assert_eq!(resize.kind, command::RESIZE);
        assert_eq!(resize.rect.width, 800);

        let style = WindowStyle::BORDER.union(WindowStyle::BUTTON_CLOSE);
        assert!(style.contains(WindowStyle::BORDER));
        assert!(!style.contains(WindowStyle::BUTTON_MINIMIZE));
        assert_eq!(WindowCommand::set_style(id, style).value, style.0 as u64);
        assert_eq!(
            WindowStyle::STANDARD.without(WindowStyle::BUTTON_MAXIMIZE),
            WindowStyle(WindowStyle::STANDARD.0 & !WindowStyle::BUTTON_MAXIMIZE.0)
        );
        assert_eq!(WindowCommand::close(id).kind, command::CLOSE);
        let create = WindowCreateRequest::standard(WindowRect::new(20, 30, 640, 480), 320, 200);
        assert_eq!(create.version, WINDOW_ABI_VERSION);
        assert_eq!(create.options, 0);
        assert_eq!(create.style, WindowStyle::STANDARD);
    }
}

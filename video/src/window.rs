//! Независимая от renderer'а модель окна и bounded очередь событий.
//!
//! Это policy-слой между wire ABI и compositor'ом. Он проверяет ID,
//! версию, style permissions и geometry constraints до изменения сцены.

use rustos_abi::window::{
    command, event, state, WindowCommand, WindowCreateRequest, WindowEvent, WindowId, WindowRect,
    WindowStyle, WINDOW_ABI_VERSION,
};

const KNOWN_STYLE: u32 = WindowStyle::STANDARD.0;

/// Ошибка проверки window command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowError {
    /// Версия command packet не поддерживается.
    UnsupportedVersion,
    /// Команда адресована другому окну.
    WrongWindow,
    /// Код команды или payload невалиден.
    InvalidCommand,
    /// Style окна запрещает операцию.
    NotAllowed,
    /// Операция не имеет смысла в текущем state.
    InvalidState,
}

/// Стороны рамки, за которые пользователь изменяет размер.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResizeEdges(u8);

impl ResizeEdges {
    /// Курсор не на рамке.
    pub const NONE: Self = Self(0);
    /// Левая сторона.
    pub const LEFT: Self = Self(1 << 0);
    /// Правая сторона.
    pub const RIGHT: Self = Self(1 << 1);
    /// Верхняя сторона.
    pub const TOP: Self = Self(1 << 2);
    /// Нижняя сторона.
    pub const BOTTOM: Self = Self(1 << 3);

    /// Объединяет стороны для corner resize.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Проверяет сторону.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Возвращает true вне resize border.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Хранимое window-server'ом состояние одного окна.
pub struct ManagedWindow {
    id: WindowId,
    rect: WindowRect,
    restore_rect: WindowRect,
    style: WindowStyle,
    state: u16,
    resume_state: u16,
    minimum_width: u32,
    minimum_height: u32,
    serial: u64,
}

impl ManagedWindow {
    /// Проверяет create request, применяет рабочую область и возвращает окно
    /// вместе с первым `SHOWN` event. Назначение уникального ID остаётся за
    /// оконным сервером/capability namespace.
    pub fn create(
        id: WindowId,
        request: WindowCreateRequest,
        work_area: WindowRect,
    ) -> Result<(Self, WindowEvent), WindowError> {
        if request.version != WINDOW_ABI_VERSION {
            return Err(WindowError::UnsupportedVersion);
        }
        if id.0 == 0
            || request.reserved != 0
            || request.options != 0
            || request.reserved_tail != 0
            || !style_is_valid(request.style)
        {
            return Err(WindowError::InvalidCommand);
        }
        let rect = constrain_rect(
            request.rect,
            work_area,
            request.minimum_width,
            request.minimum_height,
        );
        let mut window = Self::new(
            id,
            rect,
            request.style,
            request.minimum_width,
            request.minimum_height,
        );
        let shown = window.make_event(event::SHOWN);
        Ok((window, shown))
    }

    /// Создаёт normal window с constraints на content/decorations.
    pub const fn new(
        id: WindowId,
        rect: WindowRect,
        style: WindowStyle,
        minimum_width: u32,
        minimum_height: u32,
    ) -> Self {
        Self {
            id,
            rect,
            restore_rect: rect,
            style,
            state: state::NORMAL,
            resume_state: state::NORMAL,
            minimum_width,
            minimum_height,
            serial: 0,
        }
    }

    /// ID окна.
    pub const fn id(&self) -> WindowId {
        self.id
    }

    /// Текущая geometry.
    pub const fn rect(&self) -> WindowRect {
        self.rect
    }

    /// Geometry, которая будет восстановлена после maximize.
    pub const fn restore_rect(&self) -> WindowRect {
        self.restore_rect
    }

    /// Активные decoration/behaviour flags.
    pub const fn style(&self) -> WindowStyle {
        self.style
    }

    /// Числовой state из `rustos_abi::window::state`.
    pub const fn state(&self) -> u16 {
        self.state
    }

    /// Окно свёрнуто.
    pub const fn is_minimized(&self) -> bool {
        self.state == state::MINIMIZED
    }

    /// Окно развёрнуто.
    pub const fn is_maximized(&self) -> bool {
        self.state == state::MAXIMIZED
    }

    /// Окно закрыто.
    pub const fn is_closed(&self) -> bool {
        self.state == state::CLOSED
    }

    /// Можно ли рисовать окно.
    pub const fn is_visible(&self) -> bool {
        self.state != state::MINIMIZED && self.state != state::CLOSED
    }

    /// Применяет команду и возвращает событие с фактической geometry.
    pub fn apply(
        &mut self,
        command_packet: WindowCommand,
        work_area: WindowRect,
    ) -> Result<WindowEvent, WindowError> {
        if command_packet.version != WINDOW_ABI_VERSION {
            return Err(WindowError::UnsupportedVersion);
        }
        if command_packet.window != self.id {
            return Err(WindowError::WrongWindow);
        }
        match command_packet.kind {
            command::MOVE => {
                if self.state != state::NORMAL {
                    return Err(WindowError::InvalidState);
                }
                if !self.style.contains(WindowStyle::MOVABLE) {
                    return Err(WindowError::NotAllowed);
                }
                self.rect.x = clamp_move_x(command_packet.rect.x, self.rect.width, work_area);
                self.rect.y = clamp_move_y(command_packet.rect.y, self.rect.height, work_area);
                self.restore_rect = self.rect;
                Ok(self.make_event(event::MOVED))
            }
            command::RESIZE => {
                if self.state != state::NORMAL {
                    return Err(WindowError::InvalidState);
                }
                if !self.style.contains(WindowStyle::RESIZABLE) {
                    return Err(WindowError::NotAllowed);
                }
                self.rect = constrain_rect(
                    command_packet.rect,
                    work_area,
                    self.minimum_width,
                    self.minimum_height,
                );
                self.restore_rect = self.rect;
                Ok(self.make_event(event::RESIZED))
            }
            command::MINIMIZE => {
                if self.state == state::MINIMIZED || self.state == state::CLOSED {
                    return Err(WindowError::InvalidState);
                }
                self.resume_state = self.state;
                self.state = state::MINIMIZED;
                Ok(self.make_event(event::MINIMIZED))
            }
            command::MAXIMIZE => {
                if self.state == state::CLOSED {
                    return Err(WindowError::InvalidState);
                }
                if self.state == state::NORMAL {
                    self.restore_rect = self.rect;
                }
                self.state = state::MAXIMIZED;
                self.resume_state = state::MAXIMIZED;
                self.rect = work_area;
                Ok(self.make_event(event::MAXIMIZED))
            }
            command::RESTORE => {
                match self.state {
                    state::MAXIMIZED => {
                        self.state = state::NORMAL;
                        self.resume_state = state::NORMAL;
                        self.rect = constrain_rect(
                            self.restore_rect,
                            work_area,
                            self.minimum_width,
                            self.minimum_height,
                        );
                    }
                    state::MINIMIZED => {
                        self.state = self.resume_state;
                        if self.state == state::MAXIMIZED {
                            self.rect = work_area;
                        } else {
                            self.state = state::NORMAL;
                            self.rect = constrain_rect(
                                self.restore_rect,
                                work_area,
                                self.minimum_width,
                                self.minimum_height,
                            );
                        }
                    }
                    _ => return Err(WindowError::InvalidState),
                }
                Ok(self.make_event(event::RESTORED))
            }
            command::CLOSE => {
                if self.state == state::CLOSED {
                    return Err(WindowError::InvalidState);
                }
                self.state = state::CLOSED;
                Ok(self.make_event(event::CLOSED))
            }
            command::SHOW => {
                if self.state != state::CLOSED {
                    return Err(WindowError::InvalidState);
                }
                self.state = state::NORMAL;
                self.resume_state = state::NORMAL;
                self.rect = constrain_rect(
                    self.restore_rect,
                    work_area,
                    self.minimum_width,
                    self.minimum_height,
                );
                Ok(self.make_event(event::SHOWN))
            }
            command::SET_STYLE => {
                if command_packet.value > u64::from(u32::MAX)
                    || !style_is_valid(WindowStyle(command_packet.value as u32))
                {
                    return Err(WindowError::InvalidCommand);
                }
                self.style = WindowStyle(command_packet.value as u32);
                Ok(self.make_event(event::STYLE_CHANGED))
            }
            _ => Err(WindowError::InvalidCommand),
        }
    }

    /// Создаёт `CLOSE_REQUESTED`, не закрывая окно. Так клиент может
    /// показать диалог сохранения, а затем ответить `CLOSE`.
    pub fn request_close(&mut self) -> Result<WindowEvent, WindowError> {
        if self.state == state::CLOSED {
            return Err(WindowError::InvalidState);
        }
        Ok(self.make_event(event::CLOSE_REQUESTED))
    }

    /// Пересчитывает geometry после смены display mode/work area.
    /// Это внутренняя window-server operation, поэтому она работает и для
    /// minimized/closed окна, не делая их видимыми.
    pub fn reflow(&mut self, work_area: WindowRect) -> WindowEvent {
        self.restore_rect = constrain_rect(
            self.restore_rect,
            work_area,
            self.minimum_width,
            self.minimum_height,
        );
        if self.state == state::MAXIMIZED
            || self.state == state::MINIMIZED && self.resume_state == state::MAXIMIZED
        {
            self.rect = work_area;
        } else if self.state != state::CLOSED {
            self.rect = self.restore_rect;
        }
        self.make_event(event::RESIZED)
    }

    fn make_event(&mut self, kind: u16) -> WindowEvent {
        self.serial = self.serial.wrapping_add(1);
        WindowEvent {
            version: WINDOW_ABI_VERSION,
            kind,
            state: self.state,
            reserved: 0,
            window: self.id,
            rect: self.rect,
            serial: self.serial,
        }
    }
}

const fn style_is_valid(style: WindowStyle) -> bool {
    style.0 & !KNOWN_STYLE == 0
}

/// Определяет resize edges для точки вокруг рамки.
pub fn hit_test_resize(rect: WindowRect, x: i32, y: i32, thickness: u32) -> ResizeEdges {
    let thickness = thickness.max(1) as i32;
    let right = rect.x.saturating_add(rect.width as i32);
    let bottom = rect.y.saturating_add(rect.height as i32);
    if x < rect.x - thickness
        || x >= right + thickness
        || y < rect.y - thickness
        || y >= bottom + thickness
    {
        return ResizeEdges::NONE;
    }
    let mut edges = ResizeEdges::NONE;
    if x < rect.x + thickness {
        edges = edges.union(ResizeEdges::LEFT);
    } else if x >= right - thickness {
        edges = edges.union(ResizeEdges::RIGHT);
    }
    if y < rect.y + thickness {
        edges = edges.union(ResizeEdges::TOP);
    } else if y >= bottom - thickness {
        edges = edges.union(ResizeEdges::BOTTOM);
    }
    edges
}

/// Рассчитывает requested geometry при перетаскивании стороны или угла.
///
/// Функция сохраняет противоположную сторону рамки и minimum size. Она не
/// привязана к экрану: итоговые ограничения рабочей области применяет
/// [`ManagedWindow::apply`]. Благодаря этому один алгоритм используют и
/// desktop, и будущий ring-3 window server.
pub fn resize_from_edges(
    start: WindowRect,
    edges: ResizeEdges,
    delta_x: i32,
    delta_y: i32,
    minimum_width: u32,
    minimum_height: u32,
) -> WindowRect {
    let mut left = i64::from(start.x);
    let mut top = i64::from(start.y);
    let mut right = left.saturating_add(i64::from(start.width));
    let mut bottom = top.saturating_add(i64::from(start.height));
    let minimum_width = i64::from(minimum_width.max(1));
    let minimum_height = i64::from(minimum_height.max(1));

    if edges.contains(ResizeEdges::LEFT) {
        left = left
            .saturating_add(i64::from(delta_x))
            .min(right.saturating_sub(minimum_width));
    } else if edges.contains(ResizeEdges::RIGHT) {
        right = right
            .saturating_add(i64::from(delta_x))
            .max(left.saturating_add(minimum_width));
    }
    if edges.contains(ResizeEdges::TOP) {
        top = top
            .saturating_add(i64::from(delta_y))
            .min(bottom.saturating_sub(minimum_height));
    } else if edges.contains(ResizeEdges::BOTTOM) {
        bottom = bottom
            .saturating_add(i64::from(delta_y))
            .max(top.saturating_add(minimum_height));
    }

    WindowRect::new(
        clamp_i64_to_i32(left),
        clamp_i64_to_i32(top),
        right.saturating_sub(left).clamp(1, i64::from(u32::MAX)) as u32,
        bottom.saturating_sub(top).clamp(1, i64::from(u32::MAX)) as u32,
    )
}

const fn clamp_i64_to_i32(value: i64) -> i32 {
    if value < i32::MIN as i64 {
        i32::MIN
    } else if value > i32::MAX as i64 {
        i32::MAX
    } else {
        value as i32
    }
}

/// Очередь оконных событий без heap allocation.
pub struct WindowEventQueue<const CAPACITY: usize> {
    entries: [Option<WindowEvent>; CAPACITY],
    read: usize,
    len: usize,
}

impl<const CAPACITY: usize> WindowEventQueue<CAPACITY> {
    /// Создаёт пустую FIFO-очередь.
    pub const fn new() -> Self {
        Self {
            entries: [None; CAPACITY],
            read: 0,
            len: 0,
        }
    }

    /// Добавляет событие; false означает backpressure, а не потерю события.
    pub fn push(&mut self, event: WindowEvent) -> bool {
        if CAPACITY == 0 || self.len == CAPACITY {
            return false;
        }
        let write = (self.read + self.len) % CAPACITY;
        self.entries[write] = Some(event);
        self.len += 1;
        true
    }

    /// Извлекает самое старое событие.
    pub fn pop(&mut self) -> Option<WindowEvent> {
        if self.len == 0 || CAPACITY == 0 {
            return None;
        }
        let event = self.entries[self.read].take();
        self.read = (self.read + 1) % CAPACITY;
        self.len -= 1;
        event
    }

    /// Число ожидающих событий.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Пуста ли очередь.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const CAPACITY: usize> Default for WindowEventQueue<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

fn constrain_rect(
    requested: WindowRect,
    work: WindowRect,
    minimum_width: u32,
    minimum_height: u32,
) -> WindowRect {
    let min_width = minimum_width.min(work.width).max(1);
    let min_height = minimum_height.min(work.height).max(1);
    let width = requested.width.clamp(min_width, work.width.max(1));
    let height = requested.height.clamp(min_height, work.height.max(1));
    let max_x = work
        .x
        .saturating_add(work.width.saturating_sub(width) as i32);
    let max_y = work
        .y
        .saturating_add(work.height.saturating_sub(height) as i32);
    WindowRect::new(
        requested.x.clamp(work.x, max_x),
        requested.y.clamp(work.y, max_y),
        width,
        height,
    )
}

fn clamp_move_x(requested: i32, width: u32, work: WindowRect) -> i32 {
    let visible = width.min(120) as i32;
    let minimum = work.x.saturating_sub(width as i32).saturating_add(visible);
    let maximum = work
        .x
        .saturating_add(work.width as i32)
        .saturating_sub(visible);
    requested.clamp(minimum, maximum)
}

fn clamp_move_y(requested: i32, height: u32, work: WindowRect) -> i32 {
    let visible = height.min(32) as i32;
    let maximum = work
        .y
        .saturating_add(work.height as i32)
        .saturating_sub(visible);
    requested.clamp(work.y, maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: WindowId = WindowId::new(1);
    const WORK: WindowRect = WindowRect::new(0, 0, 1280, 754);

    fn window() -> ManagedWindow {
        ManagedWindow::new(
            ID,
            WindowRect::new(100, 50, 800, 600),
            WindowStyle::STANDARD,
            320,
            200,
        )
    }

    #[test]
    fn maximize_minimize_restore_preserves_normal_geometry() {
        let mut window = window();
        window
            .apply(WindowCommand::simple(ID, command::MAXIMIZE), WORK)
            .unwrap();
        assert_eq!(window.rect(), WORK);
        window
            .apply(WindowCommand::simple(ID, command::MINIMIZE), WORK)
            .unwrap();
        let restored = window
            .apply(WindowCommand::simple(ID, command::RESTORE), WORK)
            .unwrap();
        assert_eq!(restored.state, state::MAXIMIZED);
        window
            .apply(WindowCommand::simple(ID, command::RESTORE), WORK)
            .unwrap();
        assert_eq!(window.rect(), WindowRect::new(100, 50, 800, 600));
    }

    #[test]
    fn create_validates_packet_and_emits_assigned_id() {
        let request = WindowCreateRequest::standard(WindowRect::new(-30, -20, 8_000, 10), 320, 200);
        let (window, shown) = ManagedWindow::create(ID, request, WORK).unwrap();
        assert_eq!(window.rect(), WindowRect::new(0, 0, 1280, 200));
        assert_eq!(shown.kind, event::SHOWN);
        assert_eq!(shown.window, ID);

        let mut invalid = request;
        invalid.options = 1;
        assert!(matches!(
            ManagedWindow::create(ID, invalid, WORK),
            Err(WindowError::InvalidCommand)
        ));
    }

    #[test]
    fn style_controls_commands_and_unknown_bits_are_rejected() {
        let mut window = window();
        window
            .apply(WindowCommand::set_style(ID, WindowStyle::BORDER), WORK)
            .unwrap();
        assert_eq!(
            window.apply(WindowCommand::move_to(ID, 20, 20), WORK),
            Err(WindowError::NotAllowed)
        );
        let mut unknown = WindowCommand::simple(ID, command::SET_STYLE);
        unknown.value = 1 << 31;
        assert_eq!(
            window.apply(unknown, WORK),
            Err(WindowError::InvalidCommand)
        );
    }

    #[test]
    fn close_request_is_confirmed_before_close_and_window_can_be_shown_again() {
        let mut window = window();
        let request = window.request_close().unwrap();
        assert_eq!(request.kind, event::CLOSE_REQUESTED);
        assert!(!window.is_closed());

        let closed = window.apply(WindowCommand::close(ID), WORK).unwrap();
        assert_eq!(closed.kind, event::CLOSED);
        assert!(window.is_closed());

        let shown = window.apply(WindowCommand::show(ID), WORK).unwrap();
        assert_eq!(shown.kind, event::SHOWN);
        assert!(window.is_visible());
        assert!(shown.serial > request.serial);
    }

    #[test]
    fn resize_is_bounded_and_emits_actual_geometry() {
        let mut window = window();
        let event = window
            .apply(
                WindowCommand::resize(ID, WindowRect::new(-100, -20, 20, 9_000)),
                WORK,
            )
            .unwrap();
        assert_eq!(event.kind, event::RESIZED);
        assert_eq!(event.rect, WindowRect::new(0, 0, 320, 754));
    }

    #[test]
    fn event_queue_is_fifo_and_reports_backpressure() {
        let mut window = window();
        let moved = window
            .apply(WindowCommand::move_to(ID, 110, 60), WORK)
            .unwrap();
        let resized = window
            .apply(
                WindowCommand::resize(ID, WindowRect::new(110, 60, 900, 640)),
                WORK,
            )
            .unwrap();
        let mut queue = WindowEventQueue::<2>::new();
        assert!(queue.push(moved));
        assert!(queue.push(resized));
        assert!(!queue.push(resized));
        assert_eq!(queue.pop().unwrap().kind, event::MOVED);
        assert_eq!(queue.pop().unwrap().kind, event::RESIZED);
        assert!(queue.is_empty());
    }

    #[test]
    fn corners_are_reported_as_two_edges() {
        let rect = WindowRect::new(100, 100, 500, 300);
        let edges = hit_test_resize(rect, 101, 101, 6);
        assert!(edges.contains(ResizeEdges::LEFT));
        assert!(edges.contains(ResizeEdges::TOP));
    }

    #[test]
    fn edge_resize_keeps_opposite_corner_and_minimum_size() {
        let start = WindowRect::new(100, 80, 800, 600);
        assert_eq!(
            resize_from_edges(
                start,
                ResizeEdges::LEFT.union(ResizeEdges::TOP),
                50,
                30,
                320,
                200,
            ),
            WindowRect::new(150, 110, 750, 570)
        );
        assert_eq!(
            resize_from_edges(start, ResizeEdges::RIGHT, -2_000, 0, 320, 200),
            WindowRect::new(100, 80, 320, 600)
        );
    }
}

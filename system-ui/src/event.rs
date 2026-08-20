//! Единый ввод, hit testing, pointer capture и capture/bubble propagation.

use crate::{CommandId, ComponentKind, NodeId, NodeState, Tree};

/// Тип pointer-события.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerKind {
    /// Перемещение.
    Move = 1,
    /// Основная кнопка нажата.
    Down = 2,
    /// Основная кнопка отпущена.
    Up = 3,
    /// Колесо/trackpad scroll.
    Scroll = 4,
}

/// Нормализованный pointer event в логических координатах surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerEvent {
    /// Вид.
    pub kind: PointerKind,
    /// X.
    pub x: i32,
    /// Y.
    pub y: i32,
    /// Горизонтальная прокрутка.
    pub scroll_x: i16,
    /// Вертикальная прокрутка.
    pub scroll_y: i16,
    /// Modifier flags.
    pub modifiers: u16,
}

impl PointerEvent {
    /// Простое pointer event без scroll/modifiers.
    pub const fn at(kind: PointerKind, x: i32, y: i32) -> Self {
        Self {
            kind,
            x,
            y,
            scroll_x: 0,
            scroll_y: 0,
            modifiers: 0,
        }
    }
}

/// Независимые от архитектуры клавиши навигации UI.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    /// Tab.
    Tab = 1,
    /// Enter.
    Enter = 2,
    /// Space.
    Space = 3,
    /// Escape.
    Escape = 4,
    /// Стрелка влево.
    Left = 5,
    /// Стрелка вправо.
    Right = 6,
    /// Стрелка вверх.
    Up = 7,
    /// Стрелка вниз.
    Down = 8,
    /// Unicode scalar value.
    Character(char) = 256,
}

/// Keyboard event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    /// Клавиша.
    pub key: Key,
    /// `true` для key-down, `false` для key-up.
    pub pressed: bool,
    /// Modifier flags.
    pub modifiers: u16,
    /// Обратный Tab traversal.
    pub shift: bool,
}

/// Все источники ввода сходятся в одну очередь runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    /// Pointer.
    Pointer(PointerEvent),
    /// Keyboard.
    Key(KeyEvent),
}

/// Стадия распространения события.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventPhase {
    /// От root к target: родитель может перехватить gesture/shortcut.
    Capture = 1,
    /// Сам target.
    Target = 2,
    /// От target к root: обычные component handlers.
    Bubble = 3,
}

/// Один шаг маршрута event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoutedEvent {
    /// Получатель.
    pub node: NodeId,
    /// Стадия.
    pub phase: EventPhase,
}

impl RoutedEvent {
    const EMPTY: Self = Self {
        node: NodeId::NONE,
        phase: EventPhase::Target,
    };
}

/// Bounded маршрут capture/target/bubble, пригодный для inspector/replay.
pub struct EventRoute<const C: usize> {
    entries: [RoutedEvent; C],
    len: usize,
}

impl<const C: usize> EventRoute<C> {
    /// Пустой маршрут.
    pub const fn new() -> Self {
        Self {
            entries: [RoutedEvent::EMPTY; C],
            len: 0,
        }
    }

    /// Шаги маршрута.
    pub fn as_slice(&self) -> &[RoutedEvent] {
        &self.entries[..self.len]
    }

    fn push(&mut self, event: RoutedEvent) {
        if self.len < C {
            self.entries[self.len] = event;
            self.len += 1;
        }
    }
}

impl<const C: usize> Default for EventRoute<C> {
    fn default() -> Self {
        Self::new()
    }
}

/// Результат dispatch, который приложение преобразует в `UiEvent` ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchResult {
    /// Фактический target.
    pub target: NodeId,
    /// Активированная команда или ноль.
    pub command: CommandId,
    /// Нужен новый frame.
    pub changed: bool,
    /// Событие обработано UI.
    pub consumed: bool,
}

impl DispatchResult {
    pub(crate) const NONE: Self = Self {
        target: NodeId::NONE,
        command: CommandId(0),
        changed: false,
        consumed: false,
    };
}

/// Состояние input dispatcher одной UI-сессии.
pub(crate) struct InputState {
    pub hovered: NodeId,
    pub pressed: NodeId,
    pub captured: NodeId,
    pub focused: NodeId,
}

impl InputState {
    pub const fn new() -> Self {
        Self {
            hovered: NodeId::NONE,
            pressed: NodeId::NONE,
            captured: NodeId::NONE,
            focused: NodeId::NONE,
        }
    }
}

pub(crate) fn dispatch<const N: usize, F>(
    tree: &mut Tree<N>,
    state: &mut InputState,
    input: InputEvent,
    mut damage: F,
) -> DispatchResult
where
    F: FnMut(crate::Rect),
{
    match input {
        InputEvent::Pointer(event) => pointer(tree, state, event, &mut damage),
        InputEvent::Key(event) => key(tree, state, event, &mut damage),
    }
}

/// Строит явный capture/target/bubble route.
pub fn route<const N: usize, const C: usize>(tree: &Tree<N>, target: NodeId) -> EventRoute<C> {
    let mut ancestors = [NodeId::NONE; N];
    let mut len = 0usize;
    let mut current = target;
    while let Some(node) = tree.get(current) {
        if len == N {
            break;
        }
        ancestors[len] = current;
        len += 1;
        current = node.parent;
        if current.is_none() {
            break;
        }
    }
    let mut result = EventRoute::new();
    for index in (1..len).rev() {
        result.push(RoutedEvent {
            node: ancestors[index],
            phase: EventPhase::Capture,
        });
    }
    if len != 0 {
        result.push(RoutedEvent {
            node: target,
            phase: EventPhase::Target,
        });
    }
    for node in ancestors.iter().take(len).skip(1) {
        result.push(RoutedEvent {
            node: *node,
            phase: EventPhase::Bubble,
        });
    }
    result
}

fn pointer<const N: usize, F>(
    tree: &mut Tree<N>,
    state: &mut InputState,
    event: PointerEvent,
    damage: &mut F,
) -> DispatchResult
where
    F: FnMut(crate::Rect),
{
    let hit = if state.captured.is_none() {
        hit_test(tree, event.x, event.y)
    } else {
        state.captured
    };
    let mut changed = false;
    if state.hovered != hit {
        changed |= update_flag(tree, state.hovered, NodeState::HOVERED, false, damage);
        state.hovered = hit;
        changed |= update_flag(tree, hit, NodeState::HOVERED, true, damage);
    }

    match event.kind {
        PointerKind::Down if !hit.is_none() => {
            state.pressed = hit;
            state.captured = hit;
            changed |= update_flag(tree, hit, NodeState::PRESSED, true, damage);
            changed |= set_focus(tree, state, hit, damage);
            DispatchResult {
                target: hit,
                command: CommandId(0),
                changed,
                consumed: true,
            }
        }
        PointerKind::Up if !state.pressed.is_none() => {
            let pressed = state.pressed;
            changed |= update_flag(tree, pressed, NodeState::PRESSED, false, damage);
            state.pressed = NodeId::NONE;
            state.captured = NodeId::NONE;
            let activate = tree.get(pressed).is_some_and(|node| {
                node.rect.contains(event.x, event.y) && !node.state.contains(NodeState::DISABLED)
            });
            let command = if activate {
                toggle_if_needed(tree, pressed, damage);
                tree.get(pressed).map_or(CommandId(0), |node| node.command)
            } else {
                CommandId(0)
            };
            DispatchResult {
                target: pressed,
                command,
                changed,
                consumed: true,
            }
        }
        _ => DispatchResult {
            target: hit,
            command: CommandId(0),
            changed,
            consumed: !hit.is_none(),
        },
    }
}

fn key<const N: usize, F>(
    tree: &mut Tree<N>,
    state: &mut InputState,
    event: KeyEvent,
    damage: &mut F,
) -> DispatchResult
where
    F: FnMut(crate::Rect),
{
    if !event.pressed {
        return DispatchResult::NONE;
    }
    if event.key == Key::Tab {
        let next = next_focus(tree, state.focused, event.shift);
        let changed = set_focus(tree, state, next, damage);
        return DispatchResult {
            target: next,
            command: CommandId(0),
            changed,
            consumed: true,
        };
    }
    if matches!(event.key, Key::Enter | Key::Space) && !state.focused.is_none() {
        let focused = state.focused;
        let enabled = tree
            .get(focused)
            .is_some_and(|node| !node.state.contains(NodeState::DISABLED));
        if enabled {
            toggle_if_needed(tree, focused, damage);
            return DispatchResult {
                target: focused,
                command: tree.get(focused).map_or(CommandId(0), |node| node.command),
                changed: true,
                consumed: true,
            };
        }
    }
    DispatchResult {
        target: state.focused,
        ..DispatchResult::NONE
    }
}

fn hit_test<const N: usize>(tree: &Tree<N>, x: i32, y: i32) -> NodeId {
    let mut result = NodeId::NONE;
    for id in tree.ids() {
        let node = tree.get(id).expect("iterator yields live nodes");
        if node.kind.focusable()
            && node.rect.contains(x, y)
            && !node.state.contains(NodeState::DISABLED)
        {
            result = id;
        }
    }
    result
}

fn next_focus<const N: usize>(tree: &Tree<N>, current: NodeId, reverse: bool) -> NodeId {
    let current_key = tree
        .get(current)
        .map(|node| (node.tab_index, current.index()));
    let mut best = NodeId::NONE;
    let mut best_key = if reverse {
        (i16::MIN, 0usize)
    } else {
        (i16::MAX, usize::MAX)
    };
    let mut wrap = NodeId::NONE;
    let mut wrap_key = if reverse {
        (i16::MIN, 0usize)
    } else {
        (i16::MAX, usize::MAX)
    };
    for id in tree.ids() {
        let node = tree.get(id).expect("iterator yields live nodes");
        if node.tab_index < 0 || node.state.contains(NodeState::DISABLED) {
            continue;
        }
        let key = (node.tab_index, id.index());
        if reverse {
            if current_key.is_some_and(|current| key < current) && key >= best_key {
                best = id;
                best_key = key;
            }
            if key >= wrap_key {
                wrap = id;
                wrap_key = key;
            }
        } else {
            if current_key.is_none_or(|current| key > current) && key <= best_key {
                best = id;
                best_key = key;
            }
            if key <= wrap_key {
                wrap = id;
                wrap_key = key;
            }
        }
    }
    if best.is_none() {
        wrap
    } else {
        best
    }
}

fn set_focus<const N: usize, F>(
    tree: &mut Tree<N>,
    state: &mut InputState,
    target: NodeId,
    damage: &mut F,
) -> bool
where
    F: FnMut(crate::Rect),
{
    if state.focused == target {
        return false;
    }
    let mut changed = update_flag(tree, state.focused, NodeState::FOCUSED, false, damage);
    state.focused = target;
    changed |= update_flag(tree, target, NodeState::FOCUSED, true, damage);
    changed
}

fn toggle_if_needed<const N: usize, F>(tree: &mut Tree<N>, id: NodeId, damage: &mut F)
where
    F: FnMut(crate::Rect),
{
    let Some(node) = tree.get(id).copied() else {
        return;
    };
    if !matches!(
        node.kind,
        ComponentKind::CheckBox | ComponentKind::Switch | ComponentKind::RadioButton
    ) {
        return;
    }
    let mut state = node.state;
    if state.contains(NodeState::CHECKED) {
        state.remove(NodeState::CHECKED);
    } else {
        state.insert(NodeState::CHECKED);
    }
    if let Ok(rect) = tree.set_state(id, state) {
        damage(rect);
    }
}

fn update_flag<const N: usize, F>(
    tree: &mut Tree<N>,
    id: NodeId,
    flag: NodeState,
    enabled: bool,
    damage: &mut F,
) -> bool
where
    F: FnMut(crate::Rect),
{
    let Some(node) = tree.get(id).copied() else {
        return false;
    };
    let already = node.state.contains(flag);
    if already == enabled {
        return false;
    }
    let mut state = node.state;
    if enabled {
        state.insert(flag);
    } else {
        state.remove(flag);
    }
    if let Ok(rect) = tree.set_state(id, state) {
        damage(rect);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ComponentKind, NodeSpec};

    #[test]
    fn route_is_capture_target_bubble() {
        let mut tree = Tree::<4>::new();
        let panel = tree
            .create(tree.root(), NodeSpec::new(ComponentKind::Panel))
            .unwrap();
        let button = tree
            .create(panel, NodeSpec::new(ComponentKind::Button))
            .unwrap();
        let route = route::<_, 8>(&tree, button);
        assert_eq!(
            route.as_slice()[0],
            RoutedEvent {
                node: tree.root(),
                phase: EventPhase::Capture
            }
        );
        assert_eq!(
            route.as_slice()[1],
            RoutedEvent {
                node: panel,
                phase: EventPhase::Capture
            }
        );
        assert_eq!(
            route.as_slice()[2],
            RoutedEvent {
                node: button,
                phase: EventPhase::Target
            }
        );
        assert_eq!(
            route.as_slice()[4],
            RoutedEvent {
                node: tree.root(),
                phase: EventPhase::Bubble
            }
        );
    }
}

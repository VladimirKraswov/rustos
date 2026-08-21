//! Единый ввод, hit testing, pointer capture и capture/bubble propagation.

use crate::{
    CommandId, ComponentKind, NodeId, NodeState, OverscrollPolicy, ScrollAxis, ScrollBarPolicy,
    ScrollController, ScrollDelta, ScrollUnit, ScrollbarGeometry, Tree,
};

/// Стабильные modifier bits для pointer/keyboard input.
pub mod modifiers {
    /// Shift.
    pub const SHIFT: u16 = 1 << 0;
    /// Control/Command-equivalent системных edit/navigation commands.
    pub const CONTROL: u16 = 1 << 1;
    /// Alt/Option.
    pub const ALT: u16 = 1 << 2;
    /// System/Super key.
    pub const SYSTEM: u16 = 1 << 3;
}

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
    /// Pixel/line/page delta; никакой зависимости от аппаратного шага 120.
    pub scroll_unit: ScrollUnit,
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
            scroll_unit: ScrollUnit::Pixel,
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
    /// На одну viewport-страницу вверх.
    PageUp = 9,
    /// На одну viewport-страницу вниз.
    PageDown = 10,
    /// К началу строки/коллекции.
    Home = 11,
    /// К концу строки/коллекции.
    End = 12,
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
    scroll_drag: ScrollDrag,
}

#[derive(Clone, Copy)]
struct ScrollDrag {
    node: NodeId,
    view: NodeId,
    axis: ScrollAxis,
    grab_offset: i32,
}

impl ScrollDrag {
    const NONE: Self = Self {
        node: NodeId::NONE,
        view: NodeId::NONE,
        axis: ScrollAxis::Vertical,
        grab_offset: 0,
    };
}

impl InputState {
    pub const fn new() -> Self {
        Self {
            hovered: NodeId::NONE,
            pressed: NodeId::NONE,
            captured: NodeId::NONE,
            focused: NodeId::NONE,
            scroll_drag: ScrollDrag::NONE,
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
    if event.kind == PointerKind::Scroll {
        return scroll_pointer(tree, event, damage);
    }

    if !state.scroll_drag.node.is_none() {
        return drag_scrollbar(tree, state, event, damage);
    }

    if event.kind == PointerKind::Down {
        if let Some((node, view, axis, geometry)) = hit_scrollbar(tree, event.x, event.y) {
            let coordinate = axis_coordinate(axis, event.x, event.y);
            let thumb_start = axis_coordinate(axis, geometry.thumb.x, geometry.thumb.y);
            if geometry.thumb.contains(event.x, event.y) {
                state.scroll_drag = ScrollDrag {
                    node,
                    view,
                    axis,
                    grab_offset: coordinate.saturating_sub(thumb_start),
                };
                state.captured = node;
            } else {
                let direction = if coordinate < thumb_start { -1 } else { 1 };
                let changed = page_scroll(tree, node, axis, direction, damage);
                if view != node && changed {
                    if let Some(bar) = tree.get(view) {
                        damage(bar.rect);
                    }
                }
                return DispatchResult {
                    target: node,
                    command: CommandId(0),
                    changed,
                    consumed: true,
                };
            }
            let changed = set_focus(tree, state, view, false, damage);
            return DispatchResult {
                target: node,
                command: CommandId(0),
                changed,
                consumed: true,
            };
        }
    }

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
            let list_changed = select_list_at_pointer(tree, hit, event, damage);
            state.pressed = hit;
            state.captured = hit;
            changed |= update_flag(tree, hit, NodeState::PRESSED, true, damage);
            changed |= set_focus(tree, state, hit, false, damage);
            changed |= list_changed;
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

fn scroll_pointer<const N: usize, F>(
    tree: &mut Tree<N>,
    event: PointerEvent,
    damage: &mut F,
) -> DispatchResult
where
    F: FnMut(crate::Rect),
{
    let mut hit = hit_test_any(tree, event.x, event.y);
    if hit.is_none() {
        return DispatchResult::NONE;
    }
    if let Some(node) = tree.get(hit) {
        if node.kind == ComponentKind::ScrollBar && !node.scroll_target.is_none() {
            hit = node.scroll_target;
        }
    }
    let delta = ScrollDelta {
        x: i32::from(event.scroll_x),
        y: i32::from(event.scroll_y),
        unit: event.scroll_unit,
    };
    let (x_changed, x_consumed, x_target) = scroll_axis_chain(
        tree,
        hit,
        ScrollAxis::Horizontal,
        delta.x,
        delta.unit,
        damage,
    );
    let (y_changed, y_consumed, y_target) =
        scroll_axis_chain(tree, hit, ScrollAxis::Vertical, delta.y, delta.unit, damage);
    DispatchResult {
        target: if !y_target.is_none() {
            y_target
        } else {
            x_target
        },
        command: CommandId(0),
        changed: x_changed || y_changed,
        consumed: x_consumed || y_consumed,
    }
}

fn scroll_axis_chain<const N: usize, F>(
    tree: &mut Tree<N>,
    start: NodeId,
    axis: ScrollAxis,
    delta: i32,
    mut unit: ScrollUnit,
    damage: &mut F,
) -> (bool, bool, NodeId)
where
    F: FnMut(crate::Rect),
{
    if delta == 0 {
        return (false, false, NodeId::NONE);
    }
    let mut remainder = i64::from(delta);
    let mut node_id = start;
    let mut changed = false;
    let mut consumed = false;
    let mut target = NodeId::NONE;
    while let Some(snapshot) = tree.get(node_id).copied() {
        let policy = match axis {
            ScrollAxis::Horizontal => snapshot.scroll.config.horizontal,
            ScrollAxis::Vertical => snapshot.scroll.config.vertical,
        };
        if policy != ScrollBarPolicy::Hidden {
            let (unused, node_changed, contain, rect) = {
                let node = tree
                    .get_mut_internal(node_id)
                    .expect("snapshot came from the same tree");
                let config = node.scroll.config;
                let model = node.scroll.model_mut(axis);
                let before = *model;
                let unused = ScrollController::apply(
                    model,
                    remainder.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                    unit,
                    config.line_extent,
                    config.behavior,
                );
                let node_changed = *model != before;
                if node_changed {
                    node.dirty.insert(crate::DirtyFlags::LAYOUT);
                    node.dirty.insert(crate::DirtyFlags::PAINT);
                }
                (
                    unused,
                    node_changed,
                    config.overscroll == OverscrollPolicy::Contain,
                    node.rect,
                )
            };
            if node_changed {
                changed = true;
                consumed = true;
                target = node_id;
                damage(rect);
                damage_bound_scrollbars(tree, node_id, damage);
            }
            remainder = unused;
            unit = ScrollUnit::Pixel;
            if contain {
                consumed = true;
                remainder = 0;
            }
            if remainder == 0 {
                break;
            }
        }
        node_id = snapshot.parent;
    }
    (changed, consumed, target)
}

fn drag_scrollbar<const N: usize, F>(
    tree: &mut Tree<N>,
    state: &mut InputState,
    event: PointerEvent,
    damage: &mut F,
) -> DispatchResult
where
    F: FnMut(crate::Rect),
{
    let drag = state.scroll_drag;
    if event.kind == PointerKind::Up {
        state.scroll_drag = ScrollDrag::NONE;
        state.captured = NodeId::NONE;
        return DispatchResult {
            target: drag.node,
            command: CommandId(0),
            changed: false,
            consumed: true,
        };
    }
    if event.kind != PointerKind::Move {
        return DispatchResult {
            target: drag.node,
            command: CommandId(0),
            changed: false,
            consumed: true,
        };
    }
    let Some(node) = tree.get(drag.node).copied() else {
        state.scroll_drag = ScrollDrag::NONE;
        state.captured = NodeId::NONE;
        return DispatchResult::NONE;
    };
    let model = node.scroll.model(drag.axis);
    let view_rect = tree.get(drag.view).map_or(node.rect, |view| view.rect);
    let geometry = if drag.view == drag.node {
        scrollbar_geometry(view_rect, model, drag.axis)
    } else {
        ScrollbarGeometry::with_visibility(
            view_rect,
            model,
            drag.axis,
            match drag.axis {
                ScrollAxis::Horizontal => view_rect.height,
                ScrollAxis::Vertical => view_rect.width,
            },
            24,
            true,
        )
    };
    let position = axis_coordinate(drag.axis, event.x, event.y).saturating_sub(drag.grab_offset);
    let offset = geometry.offset_for_thumb(position, model);
    let changed = {
        let node = tree
            .get_mut_internal(drag.node)
            .expect("node remained live during synchronous dispatch");
        let changed = node.scroll.model_mut(drag.axis).scroll_to(offset);
        if changed {
            node.dirty.insert(crate::DirtyFlags::LAYOUT);
            node.dirty.insert(crate::DirtyFlags::PAINT);
        }
        changed
    };
    if changed {
        damage(node.rect);
        if drag.view != drag.node {
            damage(view_rect);
        }
        damage_bound_scrollbars(tree, drag.node, damage);
    }
    DispatchResult {
        target: drag.node,
        command: CommandId(0),
        changed,
        consumed: true,
    }
}

fn page_scroll<const N: usize, F>(
    tree: &mut Tree<N>,
    id: NodeId,
    axis: ScrollAxis,
    direction: i32,
    damage: &mut F,
) -> bool
where
    F: FnMut(crate::Rect),
{
    let Some(node) = tree.get_mut_internal(id) else {
        return false;
    };
    let page = node.scroll.model(axis).page_size().max(1) as i64 * i64::from(direction);
    let changed = node.scroll.model_mut(axis).scroll_by(page) != page;
    if changed {
        node.dirty.insert(crate::DirtyFlags::LAYOUT);
        node.dirty.insert(crate::DirtyFlags::PAINT);
        damage(node.rect);
    }
    changed
}

fn hit_scrollbar<const N: usize>(
    tree: &Tree<N>,
    x: i32,
    y: i32,
) -> Option<(NodeId, NodeId, ScrollAxis, ScrollbarGeometry)> {
    let mut result = None;
    for id in tree.ids() {
        let node = tree.get(id).expect("iterator yields live nodes");
        if node.state.contains(NodeState::HIDDEN) || !node.rect.contains(x, y) {
            continue;
        }
        if node.kind == ComponentKind::ScrollBar {
            let Some(target) = tree.get(node.scroll_target) else {
                continue;
            };
            let model = target.scroll.model(node.scroll_axis);
            let geometry = ScrollbarGeometry::with_visibility(
                node.rect,
                model,
                node.scroll_axis,
                match node.scroll_axis {
                    ScrollAxis::Horizontal => node.rect.height,
                    ScrollAxis::Vertical => node.rect.width,
                },
                24,
                true,
            );
            if geometry.track.contains(x, y) {
                result = Some((node.scroll_target, id, node.scroll_axis, geometry));
            }
            continue;
        }
        for axis in [ScrollAxis::Horizontal, ScrollAxis::Vertical] {
            let policy = match axis {
                ScrollAxis::Horizontal => node.scroll.config.horizontal,
                ScrollAxis::Vertical => node.scroll.config.vertical,
            };
            if policy == ScrollBarPolicy::Hidden {
                continue;
            }
            let model = node.scroll.model(axis);
            let geometry = ScrollbarGeometry::with_visibility(
                node.rect,
                model,
                axis,
                crate::DEFAULT_SCROLLBAR_THICKNESS,
                24,
                model.can_scroll() || policy == ScrollBarPolicy::Always,
            );
            if geometry.visible && geometry.track.contains(x, y) {
                result = Some((id, id, axis, geometry));
            }
        }
    }
    result
}

fn scrollbar_geometry(
    rect: crate::Rect,
    model: crate::ScrollModel,
    axis: ScrollAxis,
) -> ScrollbarGeometry {
    ScrollbarGeometry::overlay(rect, model, axis, crate::DEFAULT_SCROLLBAR_THICKNESS, 24)
}

fn damage_bound_scrollbars<const N: usize, F>(tree: &Tree<N>, target: NodeId, damage: &mut F)
where
    F: FnMut(crate::Rect),
{
    for id in tree.ids() {
        let node = tree.get(id).expect("iterator yields live nodes");
        if node.kind == ComponentKind::ScrollBar && node.scroll_target == target {
            damage(node.rect);
        }
    }
}

const fn axis_coordinate(axis: ScrollAxis, x: i32, y: i32) -> i32 {
    match axis {
        ScrollAxis::Horizontal => x,
        ScrollAxis::Vertical => y,
    }
}

fn select_list_at_pointer<const N: usize, F>(
    tree: &mut Tree<N>,
    hit: NodeId,
    event: PointerEvent,
    damage: &mut F,
) -> bool
where
    F: FnMut(crate::Rect),
{
    let mut current = hit;
    while let Some(snapshot) = tree.get(current).copied() {
        if collection_kind(snapshot.kind) && snapshot.list.is_configured() {
            let local_y = event.y.saturating_sub(snapshot.rect.y).max(0) as u32;
            let shift = event.modifiers & modifiers::SHIFT != 0;
            let control = event.modifiers & modifiers::CONTROL != 0;
            let changed = tree.get_mut_internal(current).is_some_and(|node| {
                node.list
                    .select_at(local_y, node.scroll.vertical, shift, control)
            });
            if changed {
                if let Some(node) = tree.get_mut_internal(current) {
                    node.dirty.insert(crate::DirtyFlags::PAINT);
                    node.dirty.insert(crate::DirtyFlags::SEMANTICS);
                    damage(node.rect);
                }
            }
            return changed;
        }
        current = snapshot.parent;
    }
    false
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
        let changed = set_focus(tree, state, next, true, damage);
        return DispatchResult {
            target: next,
            command: CommandId(0),
            changed,
            consumed: true,
        };
    }
    if !state.focused.is_none() {
        let focused = state.focused;
        let shift = event.shift || event.modifiers & modifiers::SHIFT != 0;
        let control = event.modifiers & modifiers::CONTROL != 0;
        let list_changed = {
            let Some(node) = tree.get_mut_internal(focused) else {
                return DispatchResult::NONE;
            };
            if collection_kind(node.kind) && node.list.is_configured() {
                let changed =
                    node.list
                        .navigate(event.key, shift, control, &mut node.scroll.vertical);
                if changed {
                    node.dirty.insert(crate::DirtyFlags::LAYOUT);
                    node.dirty.insert(crate::DirtyFlags::PAINT);
                    node.dirty.insert(crate::DirtyFlags::SEMANTICS);
                }
                changed
            } else {
                false
            }
        };
        if list_changed {
            if let Some(node) = tree.get(focused) {
                damage(node.rect);
            }
            return DispatchResult {
                target: focused,
                command: CommandId(0),
                changed: true,
                consumed: true,
            };
        }
        if matches!(
            event.key,
            Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End
        ) && scroll_key_ancestor(tree, focused, event.key, damage)
        {
            return DispatchResult {
                target: focused,
                command: CommandId(0),
                changed: true,
                consumed: true,
            };
        }
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

fn scroll_key_ancestor<const N: usize, F>(
    tree: &mut Tree<N>,
    start: NodeId,
    key: Key,
    damage: &mut F,
) -> bool
where
    F: FnMut(crate::Rect),
{
    let mut current = start;
    while let Some(snapshot) = tree.get(current).copied() {
        if snapshot.scroll.config.vertical != ScrollBarPolicy::Hidden
            && snapshot.scroll.vertical.can_scroll()
        {
            let changed = {
                let node = tree
                    .get_mut_internal(current)
                    .expect("snapshot came from the same tree");
                let model = &mut node.scroll.vertical;
                let before = model.offset();
                match key {
                    Key::Home => {
                        let _ = model.scroll_to(model.minimum());
                    }
                    Key::End => {
                        let _ = model.scroll_to(model.maximum());
                    }
                    Key::Up => {
                        let _ = model.scroll_by(-i64::from(node.scroll.config.line_extent));
                    }
                    Key::Down => {
                        let _ = model.scroll_by(i64::from(node.scroll.config.line_extent));
                    }
                    Key::PageUp => {
                        let _ = model.scroll_by(-i64::from(model.page_size().max(1)));
                    }
                    Key::PageDown => {
                        let _ = model.scroll_by(i64::from(model.page_size().max(1)));
                    }
                    _ => {}
                }
                let changed = model.offset() != before;
                if changed {
                    node.dirty.insert(crate::DirtyFlags::LAYOUT);
                    node.dirty.insert(crate::DirtyFlags::PAINT);
                }
                changed
            };
            if changed {
                damage(snapshot.rect);
                damage_bound_scrollbars(tree, current, damage);
            }
            return changed;
        }
        current = snapshot.parent;
    }
    false
}

const fn collection_kind(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::ListView
            | ComponentKind::TreeView
            | ComponentKind::TableView
            | ComponentKind::GridView
    )
}

fn hit_test<const N: usize>(tree: &Tree<N>, x: i32, y: i32) -> NodeId {
    let mut result = NodeId::NONE;
    for id in tree.ids() {
        let node = tree.get(id).expect("iterator yields live nodes");
        if node.kind.focusable()
            && node.rect.contains(x, y)
            && tree.paint_clip(id).contains(x, y)
            && !node.state.contains(NodeState::DISABLED)
            && !node.state.contains(NodeState::HIDDEN)
        {
            result = id;
        }
    }
    result
}

fn hit_test_any<const N: usize>(tree: &Tree<N>, x: i32, y: i32) -> NodeId {
    let mut result = NodeId::NONE;
    for id in tree.ids() {
        let node = tree.get(id).expect("iterator yields live nodes");
        if node.rect.contains(x, y)
            && tree.paint_clip(id).contains(x, y)
            && !node.state.contains(NodeState::HIDDEN)
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
        if node.tab_index < 0
            || node.state.contains(NodeState::DISABLED)
            || node.state.contains(NodeState::HIDDEN)
        {
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
    focus_visible: bool,
    damage: &mut F,
) -> bool
where
    F: FnMut(crate::Rect),
{
    if state.focused == target {
        return update_flag(
            tree,
            target,
            NodeState::FOCUS_VISIBLE,
            focus_visible,
            damage,
        );
    }
    let old = state.focused;
    let mut changed = update_flag(tree, old, NodeState::FOCUSED, false, damage);
    changed |= update_flag(tree, old, NodeState::FOCUS_VISIBLE, false, damage);
    state.focused = target;
    changed |= update_flag(tree, target, NodeState::FOCUSED, true, damage);
    changed |= update_flag(
        tree,
        target,
        NodeState::FOCUS_VISIBLE,
        focus_visible,
        damage,
    );
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

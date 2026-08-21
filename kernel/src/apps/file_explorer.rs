//! Системный Проводник RustOS.
//!
//! Это bootstrap-клиент общего VFS facade: приложение не знает устройство
//! диска и не разбирает файловую систему. Когда desktop-клиенты окончательно
//! перейдут в ring 3, те же операции (`list`, `rename`, `copy_tree`) станут
//! запросами `vfs.dll` к изолированному `vfsd`, а component tree не изменится.
//! Все коллекции bounded — один повреждённый каталог не может заставить
//! оконный сервер бесконтрольно выделять память.

use core::{fmt, fmt::Write, str};

use crate::{
    apps::draw_system_ui_text,
    fs::{BootstrapFs, FileKind, FsError, PATH_CAPACITY},
    graphics::{Color, Framebuffer, Rect},
    input::Key as InputKey,
    serial,
};
use rustos_abi::{bootinfo::BootInitramfs, input::MouseSettings};
use rustos_system_assets::{icon_for_path, IconKind, IconPack};
use rustos_system_ui::{
    build_file_browser, style_class, Align, CommandId, ComponentKind, Content, Edges,
    FileBrowserItem, FileBrowserSpec, FileBrowserTreeItem, FileBrowserView, FontSpec, FrameResult,
    InputEvent, LayoutSpec, Length, NodeId, NodeSpec, NodeState, PointerEvent, PointerKind,
    RenderBackend, ResourceId, Runtime, SemanticRole, Theme,
};
use rustos_video::DamageRegion;

const MAX_ENTRIES: usize = 32;
const NAME_CAPACITY: usize = 64;
const STATUS_CAPACITY: usize = 160;

const COMMAND_BACK: CommandId = CommandId(1);
const COMMAND_UP: CommandId = CommandId(2);
const COMMAND_NEW_FOLDER: CommandId = CommandId(3);
const COMMAND_COPY: CommandId = CommandId(4);
const COMMAND_CUT: CommandId = CommandId(5);
const COMMAND_PASTE: CommandId = CommandId(6);
const COMMAND_RENAME: CommandId = CommandId(7);
const COMMAND_DELETE: CommandId = CommandId(8);
const COMMAND_VIEW_GRID: CommandId = CommandId(9);
const COMMAND_VIEW_LIST: CommandId = CommandId(10);
const COMMAND_VIEW_DETAILS: CommandId = CommandId(11);
const COMMAND_HOME: CommandId = CommandId(20);
const COMMAND_ROOT: CommandId = CommandId(21);
const COMMAND_BOOT: CommandId = CommandId(22);
const COMMAND_SYSTEM: CommandId = CommandId(23);
const COMMAND_SOURCE: CommandId = CommandId(24);
const COMMAND_SYSTEM_BIN: CommandId = CommandId(25);
const COMMAND_SYSTEM_LIB: CommandId = CommandId(26);
const COMMAND_ENTRY_BASE: u32 = 1_000;

const COMMAND_POPUP_CREATE: CommandId = CommandId(100);
const COMMAND_POPUP_COPY: CommandId = CommandId(101);
const COMMAND_POPUP_CUT: CommandId = CommandId(102);
const COMMAND_POPUP_PASTE: CommandId = CommandId(103);
const COMMAND_POPUP_RENAME: CommandId = CommandId(104);
const COMMAND_POPUP_DELETE: CommandId = CommandId(105);
const COMMAND_CREATE_FOLDER: CommandId = CommandId(110);
const COMMAND_CREATE_TEXT: CommandId = CommandId(111);

const TEXT_PATH: ResourceId = ResourceId(1);
const TEXT_BACK: ResourceId = ResourceId(2);
const TEXT_UP: ResourceId = ResourceId(3);
const TEXT_NEW_FOLDER: ResourceId = ResourceId(4);
const TEXT_COPY: ResourceId = ResourceId(5);
const TEXT_CUT: ResourceId = ResourceId(6);
const TEXT_PASTE: ResourceId = ResourceId(7);
const TEXT_RENAME: ResourceId = ResourceId(8);
const TEXT_DELETE: ResourceId = ResourceId(9);
const TEXT_GRID: ResourceId = ResourceId(10);
const TEXT_LIST: ResourceId = ResourceId(11);
const TEXT_DETAILS: ResourceId = ResourceId(12);
const TEXT_HOME: ResourceId = ResourceId(13);
const TEXT_ROOT: ResourceId = ResourceId(14);
const TEXT_BOOT: ResourceId = ResourceId(15);
const TEXT_SYSTEM: ResourceId = ResourceId(16);
const TEXT_SOURCE: ResourceId = ResourceId(17);
const TEXT_NAME_HEADER: ResourceId = ResourceId(18);
const TEXT_STATUS: ResourceId = ResourceId(19);
const TEXT_EMPTY: ResourceId = ResourceId(22);
const TEXT_POPUP_CREATE: ResourceId = ResourceId(23);
const TEXT_CREATE_FOLDER: ResourceId = ResourceId(24);
const TEXT_CREATE_TEXT: ResourceId = ResourceId(25);
const TEXT_LOCATION: ResourceId = ResourceId(27);
const TEXT_QUICK_ACCESS: ResourceId = ResourceId(28);
const TEXT_MENU_FILE: ResourceId = ResourceId(29);
const TEXT_MENU_EDIT: ResourceId = ResourceId(30);
const TEXT_MENU_VIEW: ResourceId = ResourceId(31);
const TEXT_MENU_FAVORITES: ResourceId = ResourceId(32);
const TEXT_MENU_HELP: ResourceId = ResourceId(33);
const TEXT_SEARCH: ResourceId = ResourceId(34);
const TEXT_ADDRESS: ResourceId = ResourceId(35);
const TEXT_KIND_HEADER: ResourceId = ResourceId(37);
const TEXT_SIZE_HEADER: ResourceId = ResourceId(38);
const TEXT_FS_STATUS: ResourceId = ResourceId(39);
const TEXT_SYSTEM_BIN: ResourceId = ResourceId(40);
const TEXT_SYSTEM_LIB: ResourceId = ResourceId(41);

const IMAGE_HOME: ResourceId = ResourceId(50);
const IMAGE_ROOT: ResourceId = ResourceId(51);
const IMAGE_BOOT: ResourceId = ResourceId(52);
const IMAGE_SYSTEM: ResourceId = ResourceId(53);
const IMAGE_SOURCE: ResourceId = ResourceId(54);
const IMAGE_FOLDER: ResourceId = ResourceId(55);
const IMAGE_TEXT: ResourceId = ResourceId(56);
const IMAGE_TRASH: ResourceId = ResourceId(57);
const IMAGE_GRID: ResourceId = ResourceId(58);
const IMAGE_BACK: ResourceId = ResourceId(59);
const IMAGE_FORWARD: ResourceId = ResourceId(60);
const IMAGE_SEARCH: ResourceId = ResourceId(61);
const TEXT_ENTRY_BASE: u32 = 200;
const IMAGE_ENTRY_BASE: u32 = 300;
const TEXT_ENTRY_KIND_BASE: u32 = 400;
const TEXT_ENTRY_SIZE_BASE: u32 = 500;

type ExplorerRuntime = Runtime<320, 1_024, 24>;
type PopupRuntime = Runtime<20, 72, 8>;
/// Основное дерево и два popup вместе могут вернуть не более 40 независимых
/// dirty-прямоугольников. Этот бюджет передаётся compositor'у без heap.
pub type ExplorerFrame = DamageRegion<40>;

fn append_frame<const D: usize>(damage: &mut ExplorerFrame, frame: FrameResult<D>) {
    for rect in frame.damage().iter().copied() {
        damage.add(rect);
    }
}

/// Три стандартных представления каталога. Файловая модель у них одна и та
/// же; переключается только component composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExplorerView {
    Grid,
    List,
    Details,
}

#[derive(Clone, Copy)]
struct FixedText<const N: usize> {
    bytes: [u8; N],
    len: u8,
}

impl<const N: usize> FixedText<N> {
    const EMPTY: Self = Self {
        bytes: [0; N],
        len: 0,
    };

    fn set(&mut self, value: &str) {
        self.bytes.fill(0);
        let len = value.len().min(N.saturating_sub(1));
        // Все внутренние строки берутся из UTF-8 VFS. Обрезаем только на
        // границе scalar value, иначе provider не смог бы вернуть `&str`.
        let mut boundary = len;
        while boundary != 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        self.bytes[..boundary].copy_from_slice(&value.as_bytes()[..boundary]);
        self.len = boundary.min(u8::MAX as usize) as u8;
    }

    fn push_ascii(&mut self, byte: u8) -> bool {
        let len = self.len as usize;
        if !byte.is_ascii_graphic() && byte != b' ' || len + 1 >= N {
            return false;
        }
        self.bytes[len] = byte;
        self.len += 1;
        true
    }

    fn pop_char(&mut self) -> bool {
        let mut len = self.len as usize;
        if len == 0 {
            return false;
        }
        len -= 1;
        while len != 0 && self.bytes[len] & 0b1100_0000 == 0b1000_0000 {
            len -= 1;
        }
        self.bytes[len..self.len as usize].fill(0);
        self.len = len as u8;
        true
    }

    fn as_str(&self) -> &str {
        str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

impl<const N: usize> fmt::Write for FixedText<N> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let old_len = self.len as usize;
        let end = old_len.checked_add(value.len()).ok_or(fmt::Error)?;
        if end >= N || end > u8::MAX as usize {
            return Err(fmt::Error);
        }
        self.bytes[old_len..end].copy_from_slice(value.as_bytes());
        self.len = end as u8;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ExplorerPath(FixedText<PATH_CAPACITY>);

impl ExplorerPath {
    fn new(value: &str) -> Result<Self, FsError> {
        if value.is_empty() || value.len() >= PATH_CAPACITY {
            return Err(FsError::InvalidPath);
        }
        let mut text = FixedText::EMPTY;
        text.set(value);
        if text.as_str().len() != value.len() {
            return Err(FsError::InvalidPath);
        }
        Ok(Self(text))
    }

    fn join(self, name: &str) -> Result<Self, FsError> {
        if name.is_empty() || name.contains('/') {
            return Err(FsError::InvalidPath);
        }
        let mut value = FixedText::EMPTY;
        value
            .write_str(self.as_str())
            .map_err(|_| FsError::InvalidPath)?;
        if self.as_str() != "/" {
            value.write_char('/').map_err(|_| FsError::InvalidPath)?;
        }
        value.write_str(name).map_err(|_| FsError::InvalidPath)?;
        Ok(Self(value))
    }

    fn parent(self) -> Self {
        let value = self.as_str();
        if value == "/" {
            return self;
        }
        let slash = value.rfind('/').unwrap_or(0);
        Self::new(if slash == 0 { "/" } else { &value[..slash] }).unwrap_or(self)
    }

    fn basename(&self) -> &str {
        self.as_str()
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("root")
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Copy)]
struct ExplorerEntry {
    name: FixedText<NAME_CAPACITY>,
    grid_label: FixedText<NAME_CAPACITY>,
    kind_label: FixedText<NAME_CAPACITY>,
    size_label: FixedText<NAME_CAPACITY>,
    kind: FileKind,
    size: usize,
    read_only: bool,
}

impl ExplorerEntry {
    const EMPTY: Self = Self {
        name: FixedText::EMPTY,
        grid_label: FixedText::EMPTY,
        kind_label: FixedText::EMPTY,
        size_label: FixedText::EMPTY,
        kind: FileKind::File,
        size: 0,
        read_only: false,
    };
}

#[derive(Clone, Copy)]
struct Clipboard {
    source: ExplorerPath,
    cut: bool,
    valid: bool,
}

impl Clipboard {
    fn empty() -> Self {
        Self {
            source: ExplorerPath::new("/").expect("root path is valid"),
            cut: false,
            valid: false,
        }
    }
}

/// Независимое состояние одного окна. Закрытие окна уничтожает этот объект;
/// новый Проводник не наследует selection, history или незавершённый rename.
pub struct FileExplorer {
    runtime: ExplorerRuntime,
    popup: PopupRuntime,
    create_popup: PopupRuntime,
    fs: BootstrapFs,
    viewport: Rect,
    current: ExplorerPath,
    back: ExplorerPath,
    has_back: bool,
    entries: [ExplorerEntry; MAX_ENTRIES],
    entry_nodes: [NodeId; MAX_ENTRIES],
    entry_len: usize,
    selected: Option<usize>,
    view: ExplorerView,
    status: FixedText<STATUS_CAPACITY>,
    rename: FixedText<NAME_CAPACITY>,
    renaming: Option<usize>,
    clipboard: Clipboard,
    popup_open: bool,
    create_popup_open: bool,
    popup_rect: Rect,
    create_popup_rect: Rect,
    last_clicked: Option<usize>,
    last_click_ms: u64,
    ui_scale_milli: u16,
}

impl FileExplorer {
    /// Создаёт крупную UI-сессию непосредственно в кадрах приложения.
    ///
    /// # Safety
    /// `destination` должен быть выровнен, доступен для записи и указывать на
    /// неинициализированное хранилище размером `FileExplorer`.
    pub unsafe fn initialize_in_place(
        destination: *mut Self,
        viewport: Rect,
        initramfs: BootInitramfs,
        ui_scale_milli: u16,
    ) {
        let theme = explorer_theme(ui_scale_milli);
        // SAFETY: контракт метода предоставляет storage всего FileExplorer;
        // каждый field инициализируется ровно один раз до получения &mut Self.
        unsafe {
            ExplorerRuntime::initialize_in_place(
                core::ptr::addr_of_mut!((*destination).runtime),
                viewport,
                theme,
            );
            PopupRuntime::initialize_in_place(
                core::ptr::addr_of_mut!((*destination).popup),
                Rect::EMPTY,
                theme,
            );
            PopupRuntime::initialize_in_place(
                core::ptr::addr_of_mut!((*destination).create_popup),
                Rect::EMPTY,
                theme,
            );
            core::ptr::addr_of_mut!((*destination).fs).write(BootstrapFs::new(initramfs));
            core::ptr::addr_of_mut!((*destination).viewport).write(viewport);
            core::ptr::addr_of_mut!((*destination).current)
                .write(ExplorerPath::new("/").expect("root path is valid"));
            core::ptr::addr_of_mut!((*destination).back)
                .write(ExplorerPath::new("/").expect("root path is valid"));
            core::ptr::addr_of_mut!((*destination).has_back).write(false);
            core::ptr::addr_of_mut!((*destination).entries)
                .write([ExplorerEntry::EMPTY; MAX_ENTRIES]);
            core::ptr::addr_of_mut!((*destination).entry_nodes).write([NodeId::NONE; MAX_ENTRIES]);
            core::ptr::addr_of_mut!((*destination).entry_len).write(0);
            core::ptr::addr_of_mut!((*destination).selected).write(None);
            core::ptr::addr_of_mut!((*destination).view).write(ExplorerView::Grid);
            core::ptr::addr_of_mut!((*destination).status).write(FixedText::EMPTY);
            core::ptr::addr_of_mut!((*destination).rename).write(FixedText::EMPTY);
            core::ptr::addr_of_mut!((*destination).renaming).write(None);
            core::ptr::addr_of_mut!((*destination).clipboard).write(Clipboard::empty());
            core::ptr::addr_of_mut!((*destination).popup_open).write(false);
            core::ptr::addr_of_mut!((*destination).create_popup_open).write(false);
            core::ptr::addr_of_mut!((*destination).popup_rect).write(Rect::EMPTY);
            core::ptr::addr_of_mut!((*destination).create_popup_rect).write(Rect::EMPTY);
            core::ptr::addr_of_mut!((*destination).last_clicked).write(None);
            core::ptr::addr_of_mut!((*destination).last_click_ms).write(0);
            core::ptr::addr_of_mut!((*destination).ui_scale_milli).write(ui_scale_milli);
        }
        // SAFETY: все поля полностью инициализированы выше.
        let result = unsafe { &mut *destination };
        result.refresh();
        log_operation("READY", result.current.as_str());
    }

    pub fn resize(&mut self, viewport: Rect) {
        if self.viewport == viewport {
            return;
        }
        self.viewport = viewport;
        self.close_popups();
        self.rebuild_main();
    }

    pub fn set_scale(&mut self, ui_scale_milli: u16) {
        if self.ui_scale_milli == ui_scale_milli {
            return;
        }
        self.ui_scale_milli = ui_scale_milli;
        self.rebuild_main();
    }

    pub fn pointer(
        &mut self,
        kind: PointerKind,
        x: i32,
        y: i32,
        now_ms: u64,
        settings: MouseSettings,
    ) -> bool {
        if self.create_popup_open {
            if self.create_popup_rect.contains(x, y) {
                let result = self
                    .create_popup
                    .dispatch(InputEvent::Pointer(PointerEvent::at(kind, x, y)));
                if result.command != CommandId(0) {
                    self.execute(result.command, now_ms, x, y);
                }
                return result.changed || result.command != CommandId(0);
            }
            if kind == PointerKind::Down {
                self.close_popups();
                self.rebuild_main();
                return true;
            }
        }
        if self.popup_open {
            if self.popup_rect.contains(x, y) {
                let result = self
                    .popup
                    .dispatch(InputEvent::Pointer(PointerEvent::at(kind, x, y)));
                if result.command != CommandId(0) {
                    self.execute(result.command, now_ms, x, y);
                }
                return result.changed || result.command != CommandId(0);
            }
            if kind == PointerKind::Down {
                self.close_popups();
                self.rebuild_main();
                return true;
            }
        }

        let result = self
            .runtime
            .dispatch(InputEvent::Pointer(PointerEvent::at(kind, x, y)));
        if result.command.0 >= COMMAND_ENTRY_BASE {
            let index = (result.command.0 - COMMAND_ENTRY_BASE) as usize;
            self.activate_entry(index, now_ms, x, y, settings.double_click_ms);
        } else if result.command != CommandId(0) {
            self.execute(result.command, now_ms, x, y);
        }
        // `consumed` означает только маршрутизацию события. Обычный Move над
        // тем же самым control consumed=true, но визуально ничего не меняет.
        // Считать его repaint'ом означало перерисовывать окно на каждый пакет
        // мыши — именно это раньше создавало тяжёлые hover-тормоза.
        result.changed || result.command != CommandId(0)
    }

    /// Вторичная кнопка приходит отдельно от primary pointer dispatcher.
    /// Selection определяется по rect-ам component tree — параллельной ручной
    /// таблицы hit-test у Проводника нет.
    pub fn open_context_menu(&mut self, x: i32, y: i32) {
        for index in 0..self.entry_len {
            let node = self.entry_nodes[index];
            if self
                .runtime
                .tree()
                .get(node)
                .is_some_and(|node| node.rect.contains(x, y))
            {
                self.selected = Some(index);
                break;
            }
        }
        self.renaming = None;
        self.popup_rect = popup_rect(x, y, 224, 282, self.viewport);
        self.popup_open = true;
        self.create_popup_open = false;
        self.rebuild_main();
        self.rebuild_popup();
    }

    pub fn key(&mut self, key: InputKey, now_ms: u64, settings: MouseSettings) -> bool {
        if self.renaming.is_some() {
            return match key {
                InputKey::Enter => {
                    self.commit_rename();
                    true
                }
                InputKey::Escape => {
                    self.renaming = None;
                    self.set_status("Переименование отменено");
                    self.rebuild_main();
                    true
                }
                InputKey::Backspace => {
                    if self.rename.pop_char() {
                        self.rebuild_main();
                    }
                    true
                }
                InputKey::Character(byte) if byte != b'/' && byte != b'\\' => {
                    if self.rename.push_ascii(byte) {
                        self.rebuild_main();
                    }
                    true
                }
                InputKey::Tab
                | InputKey::Left
                | InputKey::Right
                | InputKey::Up
                | InputKey::Down
                | InputKey::PageUp
                | InputKey::PageDown
                | InputKey::Home
                | InputKey::End
                | InputKey::Character(_) => true,
            };
        }
        if (self.popup_open || self.create_popup_open) && matches!(key, InputKey::Escape) {
            self.close_popups();
            self.rebuild_main();
            return true;
        }
        let ui_key = match key {
            InputKey::Tab => rustos_system_ui::Key::Tab,
            InputKey::Enter => rustos_system_ui::Key::Enter,
            InputKey::Escape => rustos_system_ui::Key::Escape,
            InputKey::Left => rustos_system_ui::Key::Left,
            InputKey::Right => rustos_system_ui::Key::Right,
            InputKey::Up => rustos_system_ui::Key::Up,
            InputKey::Down => rustos_system_ui::Key::Down,
            InputKey::PageUp => rustos_system_ui::Key::PageUp,
            InputKey::PageDown => rustos_system_ui::Key::PageDown,
            InputKey::Home => rustos_system_ui::Key::Home,
            InputKey::End => rustos_system_ui::Key::End,
            InputKey::Character(b' ') => rustos_system_ui::Key::Space,
            InputKey::Character(byte) if byte.is_ascii() => {
                rustos_system_ui::Key::Character(char::from(byte))
            }
            InputKey::Backspace | InputKey::Character(_) => return false,
        };
        let result = self
            .runtime
            .dispatch(InputEvent::Key(rustos_system_ui::KeyEvent {
                key: ui_key,
                pressed: true,
                modifiers: 0,
                shift: false,
            }));
        if result.command.0 >= COMMAND_ENTRY_BASE {
            self.activate_entry(
                (result.command.0 - COMMAND_ENTRY_BASE) as usize,
                now_ms,
                0,
                0,
                settings.double_click_ms,
            );
        } else if result.command != CommandId(0) {
            self.execute(result.command, now_ms, 0, 0);
        }
        result.changed || result.command != CommandId(0)
    }

    /// Wheel не меняет focus окна: событие получает scrollable-компонент под
    /// указателем, как в обычных desktop-системах.
    pub fn scroll(&mut self, x: i32, y: i32, wheel_x: i16, wheel_y: i16) -> bool {
        let mut pointer = PointerEvent::at(PointerKind::Scroll, x, y);
        pointer.scroll_x = wheel_x;
        pointer.scroll_y = wheel_y;
        pointer.scroll_unit = rustos_system_ui::ScrollUnit::Line;
        self.runtime.dispatch(InputEvent::Pointer(pointer)).changed
    }

    pub fn draw(
        &mut self,
        framebuffer: &mut Framebuffer,
        icons: IconPack,
        full: bool,
    ) -> ExplorerFrame {
        if full {
            self.runtime.invalidate_all();
            if self.popup_open {
                self.popup.invalidate_all();
            }
            if self.create_popup_open {
                self.create_popup.invalidate_all();
            }
        }
        let resources = ExplorerResources {
            current: &self.current,
            entries: &self.entries,
            status: &self.status,
            rename: &self.rename,
            renaming: self.renaming,
            view: self.view,
        };
        let mut backend = ExplorerBackend {
            framebuffer,
            resources: &resources,
            icons,
        };
        let mut damage = ExplorerFrame::new(self.viewport);
        append_frame(
            &mut damage,
            self.runtime
                .render(&mut backend)
                .unwrap_or_else(|_| FrameResult::empty()),
        );
        if self.popup_open {
            append_frame(
                &mut damage,
                self.popup
                    .render(&mut backend)
                    .unwrap_or_else(|_| FrameResult::empty()),
            );
        }
        if self.create_popup_open {
            append_frame(
                &mut damage,
                self.create_popup
                    .render(&mut backend)
                    .unwrap_or_else(|_| FrameResult::empty()),
            );
        }
        damage
    }

    fn execute(&mut self, command: CommandId, _now_ms: u64, _x: i32, _y: i32) {
        match command {
            COMMAND_BACK if self.has_back => {
                core::mem::swap(&mut self.back, &mut self.current);
                self.selected = None;
                self.refresh();
            }
            COMMAND_UP => self.navigate(self.current.parent()),
            COMMAND_HOME => self.navigate_path("/home/user"),
            COMMAND_ROOT => self.navigate_path("/"),
            COMMAND_BOOT => self.navigate_path("/boot"),
            COMMAND_SYSTEM => self.navigate_path("/system"),
            COMMAND_SOURCE => self.navigate_path("/src"),
            COMMAND_SYSTEM_BIN => self.navigate_path("/system/bin"),
            COMMAND_SYSTEM_LIB => self.navigate_path("/system/lib"),
            COMMAND_NEW_FOLDER | COMMAND_CREATE_FOLDER => self.create_folder(),
            COMMAND_CREATE_TEXT => self.create_text_file(),
            COMMAND_COPY | COMMAND_POPUP_COPY => self.copy_selection(false),
            COMMAND_CUT | COMMAND_POPUP_CUT => self.copy_selection(true),
            COMMAND_PASTE | COMMAND_POPUP_PASTE => self.paste(),
            COMMAND_RENAME | COMMAND_POPUP_RENAME => self.begin_rename(),
            COMMAND_DELETE | COMMAND_POPUP_DELETE => self.delete_selection(),
            COMMAND_VIEW_GRID => self.set_view(ExplorerView::Grid),
            COMMAND_VIEW_LIST => self.set_view(ExplorerView::List),
            COMMAND_VIEW_DETAILS => self.set_view(ExplorerView::Details),
            COMMAND_POPUP_CREATE => {
                self.create_popup_open = true;
                self.create_popup_rect = popup_rect(
                    self.popup_rect.right().saturating_add(4),
                    self.popup_rect.y,
                    216,
                    108,
                    self.viewport,
                );
                self.rebuild_create_popup();
                return;
            }
            _ => return,
        }
        self.close_popups();
    }

    fn activate_entry(&mut self, index: usize, now_ms: u64, x: i32, y: i32, double_click_ms: u16) {
        if index >= self.entry_len {
            return;
        }
        let same = self.last_clicked == Some(index)
            && now_ms.saturating_sub(self.last_click_ms) <= u64::from(double_click_ms);
        self.selected = Some(index);
        self.renaming = None;
        if same {
            if self.click_is_on_name(index, x, y) {
                self.begin_rename();
            } else if self.entries[index].kind == FileKind::Directory {
                if let Ok(path) = self.current.join(self.entries[index].name.as_str()) {
                    self.navigate(path);
                }
            } else {
                self.set_status("Файл выбран · используйте контекстное меню для действий");
                self.rebuild_main();
            }
            self.last_clicked = None;
            return;
        }
        self.last_clicked = Some(index);
        self.last_click_ms = now_ms;
        self.set_default_status();
        self.rebuild_main();
    }

    fn click_is_on_name(&self, index: usize, x: i32, y: i32) -> bool {
        let Some(rect) = self
            .runtime
            .tree()
            .get(self.entry_nodes[index])
            .map(|node| node.rect)
        else {
            return false;
        };
        match self.view {
            ExplorerView::Grid => y >= rect.bottom().saturating_sub(32),
            ExplorerView::List | ExplorerView::Details => x >= rect.x.saturating_add(42),
        }
    }

    fn navigate_path(&mut self, path: &str) {
        match ExplorerPath::new(path) {
            Ok(path) => self.navigate(path),
            Err(error) => self.set_error("Не удалось открыть путь", error),
        }
    }

    fn navigate(&mut self, path: ExplorerPath) {
        match self.fs.stat(path.as_str()) {
            Ok(stat) if stat.kind == FileKind::Directory => {
                self.back = self.current;
                self.has_back = self.back.as_str() != path.as_str();
                self.current = path;
                self.selected = None;
                self.renaming = None;
                self.refresh();
                log_operation("OPEN", self.current.as_str());
            }
            Ok(_) => self.set_status("Выбранный объект не является папкой"),
            Err(error) => self.set_error("Не удалось открыть папку", error),
        }
    }

    fn set_view(&mut self, view: ExplorerView) {
        self.view = view;
        self.rebuild_main();
    }

    fn create_folder(&mut self) {
        let Some(path) = self.unique_child("Новая папка") else {
            return;
        };
        match self.fs.make_dir(path.as_str()) {
            Ok(()) => {
                log_operation("MKDIR", path.as_str());
                self.refresh();
                self.select_named(path.basename());
                self.set_status("Папка создана");
                self.rebuild_main();
            }
            Err(error) => self.set_error("Не удалось создать папку", error),
        }
    }

    fn create_text_file(&mut self) {
        let Some(path) = self.unique_child("Новый файл.txt") else {
            return;
        };
        match self.fs.touch(path.as_str()) {
            Ok(()) => {
                log_operation("TOUCH", path.as_str());
                self.refresh();
                self.select_named(path.basename());
                self.set_status("Текстовый файл создан");
                self.rebuild_main();
            }
            Err(error) => self.set_error("Не удалось создать файл", error),
        }
    }

    fn unique_child(&mut self, base: &str) -> Option<ExplorerPath> {
        for number in 1..=99u8 {
            let mut name = FixedText::<NAME_CAPACITY>::EMPTY;
            if number == 1 {
                name.set(base);
            } else if let Some((stem, extension)) = base.rsplit_once('.') {
                let _ = write!(name, "{} {}.{}", stem, number, extension);
            } else {
                let _ = write!(name, "{} {}", base, number);
            }
            let Ok(path) = self.current.join(name.as_str()) else {
                self.set_status("Слишком длинное имя");
                return None;
            };
            if matches!(self.fs.stat(path.as_str()), Err(FsError::NotFound)) {
                return Some(path);
            }
        }
        self.set_status("Не удалось подобрать свободное имя");
        None
    }

    fn copy_selection(&mut self, cut: bool) {
        let Some(index) = self.selected else {
            self.set_status("Сначала выберите файл или папку");
            self.rebuild_main();
            return;
        };
        let Ok(source) = self.current.join(self.entries[index].name.as_str()) else {
            return;
        };
        if cut && self.entries[index].read_only {
            self.set_status("Объект доступен только для чтения; его можно скопировать");
            self.rebuild_main();
            return;
        }
        self.clipboard = Clipboard {
            source,
            cut,
            valid: true,
        };
        log_operation(if cut { "CUT" } else { "COPY" }, source.as_str());
        self.set_status(if cut {
            "Объект вырезан · выберите папку и нажмите «Вставить»"
        } else {
            "Объект скопирован · выберите папку и нажмите «Вставить»"
        });
        self.rebuild_main();
    }

    fn paste(&mut self) {
        if !self.clipboard.valid {
            self.set_status("Буфер обмена пуст");
            self.rebuild_main();
            return;
        }
        let source = self.clipboard.source;
        let mut target = match self.current.join(source.basename()) {
            Ok(path) => path,
            Err(error) => {
                self.set_error("Слишком длинный путь назначения", error);
                return;
            }
        };
        if self.fs.stat(target.as_str()).is_ok() {
            let mut copy_name = FixedText::<NAME_CAPACITY>::EMPTY;
            let _ = write!(copy_name, "Копия {}", source.basename());
            let Some(unique) = self.unique_child(copy_name.as_str()) else {
                return;
            };
            target = unique;
        }
        let was_cut = self.clipboard.cut;
        let result = if was_cut {
            self.fs.rename(source.as_str(), target.as_str())
        } else {
            self.fs.copy_tree(source.as_str(), target.as_str())
        };
        match result {
            Ok(()) => {
                log_operation(if was_cut { "MOVE" } else { "PASTE" }, target.as_str());
                if was_cut {
                    self.clipboard.valid = false;
                }
                self.refresh();
                self.select_named(target.basename());
                self.set_status(if was_cut {
                    "Объект перемещён"
                } else {
                    "Копия создана"
                });
                self.rebuild_main();
            }
            Err(error) => self.set_error("Не удалось вставить объект", error),
        }
    }

    fn delete_selection(&mut self) {
        let Some(index) = self.selected else {
            self.set_status("Сначала выберите объект для удаления");
            self.rebuild_main();
            return;
        };
        let Ok(path) = self.current.join(self.entries[index].name.as_str()) else {
            return;
        };
        match self.fs.remove_tree(path.as_str()) {
            Ok(()) => {
                log_operation("REMOVE", path.as_str());
                self.selected = None;
                self.refresh();
                self.set_status("Объект удалён");
                self.rebuild_main();
            }
            Err(error) => self.set_error("Не удалось удалить объект", error),
        }
    }

    fn begin_rename(&mut self) {
        let Some(index) = self.selected else {
            self.set_status("Сначала выберите объект");
            self.rebuild_main();
            return;
        };
        if self.entries[index].read_only {
            self.set_status("Объект доступен только для чтения");
            self.rebuild_main();
            return;
        }
        self.rename = self.entries[index].name;
        self.renaming = Some(index);
        if let Ok(path) = self.current.join(self.entries[index].name.as_str()) {
            log_operation("RENAME-BEGIN", path.as_str());
        }
        self.set_status("Введите новое имя · Enter — сохранить · Esc — отменить");
        self.rebuild_main();
    }

    fn commit_rename(&mut self) {
        let Some(index) = self.renaming.take() else {
            return;
        };
        let rename = self.rename;
        let name = rename.as_str().trim();
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            self.renaming = Some(index);
            self.set_status("Имя не должно быть пустым или содержать / и \\");
            self.rebuild_main();
            return;
        }
        let Ok(source) = self.current.join(self.entries[index].name.as_str()) else {
            return;
        };
        let Ok(target) = self.current.join(name) else {
            self.set_status("Новое имя слишком длинное");
            self.rebuild_main();
            return;
        };
        log_operation("RENAME-REQUEST", target.as_str());
        match self.fs.rename(source.as_str(), target.as_str()) {
            Ok(()) => {
                log_operation("RENAME", target.as_str());
                self.refresh();
                self.select_named(name);
                self.set_status("Объект переименован");
                self.rebuild_main();
            }
            Err(error) => {
                self.renaming = Some(index);
                self.set_error("Не удалось переименовать объект", error);
            }
        }
    }

    fn refresh(&mut self) {
        self.entries.fill(ExplorerEntry::EMPTY);
        self.entry_len = 0;
        match self.fs.list(self.current.as_str()) {
            Ok(listing) => {
                for source in listing.entries().iter().take(MAX_ENTRIES) {
                    let mut entry = ExplorerEntry::EMPTY;
                    entry.name.set(source.name());
                    entry.kind = source.kind();
                    entry.read_only = source.is_read_only();
                    if let Ok(path) = self.current.join(source.name()) {
                        entry.size = self.fs.stat(path.as_str()).map_or(0, |stat| stat.size);
                    }
                    entry.kind_label.set(if entry.kind == FileKind::Directory {
                        "Папка"
                    } else {
                        "Файл"
                    });
                    if entry.kind == FileKind::Directory {
                        entry.size_label.set("—");
                    } else {
                        let _ = write!(entry.size_label, "{} Б", entry.size);
                    }
                    entry.grid_label = compact_label(entry.name.as_str(), 17);
                    self.entries[self.entry_len] = entry;
                    self.entry_len += 1;
                }
                self.sort_entries();
                self.set_default_status();
            }
            Err(error) => self.set_error("Не удалось прочитать каталог", error),
        }
        self.rebuild_main();
    }

    fn sort_entries(&mut self) {
        for right in 1..self.entry_len {
            let mut current = right;
            while current != 0 && entry_after(&self.entries[current - 1], &self.entries[current]) {
                self.entries.swap(current - 1, current);
                current -= 1;
            }
        }
    }

    fn select_named(&mut self, name: &str) {
        self.selected = self.entries[..self.entry_len]
            .iter()
            .position(|entry| entry.name.as_str() == name);
    }

    fn set_default_status(&mut self) {
        self.status = FixedText::EMPTY;
        let _ = write!(self.status, "{} элементов", self.entry_len);
        if let Some(index) = self.selected {
            let entry = self.entries[index];
            let _ = write!(
                self.status,
                " · {} · {}",
                entry.name.as_str(),
                if entry.read_only {
                    "только чтение"
                } else {
                    "доступен"
                }
            );
        }
    }

    fn set_status(&mut self, message: &str) {
        self.status.set(message);
    }

    fn set_error(&mut self, operation: &str, error: FsError) {
        serial::put_str("[explorer] error=");
        serial::put_str(error.message());
        serial::put_str(" operation=");
        serial::put_str(operation);
        serial::put_str("\n");
        self.status = FixedText::EMPTY;
        let _ = write!(self.status, "{}: {}", operation, error.message());
        self.rebuild_main();
    }

    fn close_popups(&mut self) {
        self.popup_open = false;
        self.create_popup_open = false;
    }

    fn rebuild_main(&mut self) {
        let theme = explorer_theme(self.ui_scale_milli);
        self.runtime.rebuild(self.viewport, theme);
        self.entry_nodes.fill(NodeId::NONE);
        let path = self.current.as_str();
        let state = MainTreeState {
            viewport_width: self.viewport.width,
            view: self.view,
            entry_len: self.entry_len,
            selected: self.selected,
            renaming: self.renaming,
            has_back: self.has_back,
            can_up: self.current.as_str() != "/",
            read_only: self.current.as_str().starts_with("/boot"),
            can_paste: self.clipboard.valid,
            selected_location: if path == "/home/user" {
                COMMAND_HOME
            } else if path == "/" {
                COMMAND_ROOT
            } else if path.starts_with("/boot") {
                COMMAND_BOOT
            } else if path.starts_with("/system/bin") {
                COMMAND_SYSTEM_BIN
            } else if path.starts_with("/system/lib") {
                COMMAND_SYSTEM_LIB
            } else if path.starts_with("/system") {
                COMMAND_SYSTEM
            } else if path.starts_with("/src") {
                COMMAND_SOURCE
            } else {
                CommandId(0)
            },
        };
        build_main_tree(&mut self.runtime, state, &mut self.entry_nodes);
    }

    fn rebuild_popup(&mut self) {
        self.popup
            .rebuild(self.popup_rect, explorer_theme(self.ui_scale_milli));
        build_popup_tree(
            &mut self.popup,
            self.selected.is_some(),
            self.selected
                .is_some_and(|index| self.entries[index].read_only),
            self.clipboard.valid,
            self.current.as_str().starts_with("/boot"),
        );
    }

    fn rebuild_create_popup(&mut self) {
        self.create_popup
            .rebuild(self.create_popup_rect, explorer_theme(self.ui_scale_milli));
        build_create_popup_tree(&mut self.create_popup);
    }
}

#[derive(Clone, Copy)]
struct MainTreeState {
    viewport_width: u32,
    view: ExplorerView,
    entry_len: usize,
    selected: Option<usize>,
    renaming: Option<usize>,
    has_back: bool,
    can_up: bool,
    read_only: bool,
    can_paste: bool,
    selected_location: CommandId,
}

fn build_main_tree(
    runtime: &mut ExplorerRuntime,
    state: MainTreeState,
    entry_nodes: &mut [NodeId; MAX_ENTRIES],
) {
    let MainTreeState {
        viewport_width,
        view,
        entry_len,
        selected,
        renaming,
        has_back,
        can_up,
        read_only,
        can_paste,
        selected_location,
    } = state;
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let page_node = match ui.column(
        root,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            gap: 1,
            ..LayoutSpec::default()
        },
    ) {
        Ok(node) => node,
        Err(_) => return,
    };

    // Верхнее меню остаётся обычным Toolbar: когда появятся popup-пункты,
    // приложение добавит команды, не меняя layout и hit-test.
    let menu = match ui.toolbar(
        page_node,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(30),
            padding: Edges::symmetric(8, 3),
            gap: 2,
            align: Align::Center,
            ..LayoutSpec::default()
        },
    ) {
        Ok(node) => node,
        Err(_) => return,
    };
    for (label, width) in [
        (TEXT_MENU_FILE, 82),
        (TEXT_MENU_EDIT, 96),
        (TEXT_MENU_VIEW, 82),
        (TEXT_MENU_FAVORITES, 132),
        (TEXT_MENU_HELP, 92),
    ] {
        add_menu_label(&mut ui, menu, label, width);
    }

    let toolbar = match ui.toolbar(
        page_node,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(56),
            padding: Edges::symmetric(10, 7),
            gap: 7,
            align: Align::Center,
            ..LayoutSpec::default()
        },
    ) {
        Ok(node) => node,
        Err(_) => return,
    };
    add_command_button(
        &mut ui,
        toolbar,
        TEXT_BACK,
        IMAGE_BACK,
        COMMAND_BACK,
        100,
        !has_back,
    );
    add_command_button(
        &mut ui,
        toolbar,
        TEXT_UP,
        IMAGE_FOLDER,
        COMMAND_UP,
        96,
        !can_up,
    );
    add_command_button(
        &mut ui,
        toolbar,
        TEXT_SEARCH,
        IMAGE_SEARCH,
        CommandId(0),
        96,
        false,
    );
    add_command_button(
        &mut ui,
        toolbar,
        TEXT_NEW_FOLDER,
        IMAGE_FOLDER,
        COMMAND_NEW_FOLDER,
        160,
        read_only,
    );
    add_command_button(
        &mut ui,
        toolbar,
        TEXT_COPY,
        IMAGE_TEXT,
        COMMAND_COPY,
        126,
        selected.is_none(),
    );
    add_command_button(
        &mut ui,
        toolbar,
        TEXT_PASTE,
        IMAGE_TEXT,
        COMMAND_PASTE,
        118,
        !can_paste || read_only,
    );
    add_command_button(
        &mut ui,
        toolbar,
        TEXT_DELETE,
        IMAGE_TRASH,
        COMMAND_DELETE,
        110,
        selected.is_none() || read_only,
    );
    for (label, command, active) in [
        (TEXT_GRID, COMMAND_VIEW_GRID, view == ExplorerView::Grid),
        (TEXT_LIST, COMMAND_VIEW_LIST, view == ExplorerView::List),
        (
            TEXT_DETAILS,
            COMMAND_VIEW_DETAILS,
            view == ExplorerView::Details,
        ),
    ] {
        add_view_button(&mut ui, toolbar, label, command, active);
    }

    let address = match ui.row(
        page_node,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(44),
            padding: Edges::symmetric(10, 5),
            gap: 8,
            align: Align::Center,
            ..LayoutSpec::default()
        },
    ) {
        Ok(node) => node,
        Err(_) => return,
    };
    let _ = ui.text(
        address,
        TEXT_ADDRESS,
        LayoutSpec {
            width: Length::Px(54),
            height: Length::Fill(1),
            ..LayoutSpec::default()
        },
    );
    let mut path = NodeSpec::new(ComponentKind::TextField);
    path.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(34),
        min_width: 180,
        ..LayoutSpec::default()
    };
    path.content = Content::Text(TEXT_PATH);
    path.accessible_name = TEXT_LOCATION;
    path.role = SemanticRole::TextField;
    path.state = NodeState::READ_ONLY;
    path.tab_index = -1;
    let _ = ui.component(address, path);

    let tree_items = [
        tree_item(TEXT_HOME, IMAGE_HOME, COMMAND_HOME, 0, selected_location),
        tree_item(TEXT_ROOT, IMAGE_ROOT, COMMAND_ROOT, 0, selected_location),
        tree_item(TEXT_BOOT, IMAGE_BOOT, COMMAND_BOOT, 1, selected_location),
        tree_item(
            TEXT_SYSTEM,
            IMAGE_SYSTEM,
            COMMAND_SYSTEM,
            1,
            selected_location,
        ),
        tree_item(
            TEXT_SYSTEM_BIN,
            IMAGE_FOLDER,
            COMMAND_SYSTEM_BIN,
            2,
            selected_location,
        ),
        tree_item(
            TEXT_SYSTEM_LIB,
            IMAGE_FOLDER,
            COMMAND_SYSTEM_LIB,
            2,
            selected_location,
        ),
        tree_item(
            TEXT_SOURCE,
            IMAGE_SOURCE,
            COMMAND_SOURCE,
            1,
            selected_location,
        ),
    ];
    let empty_item = FileBrowserItem {
        label: ResourceId(0),
        kind_label: ResourceId(0),
        size_label: ResourceId(0),
        icon: ResourceId(0),
        command: CommandId(0),
        selected: false,
        editing: false,
    };
    let mut items = [empty_item; MAX_ENTRIES];
    for (index, item) in items.iter_mut().enumerate().take(entry_len) {
        *item = FileBrowserItem {
            label: ResourceId(TEXT_ENTRY_BASE + index as u32),
            kind_label: ResourceId(TEXT_ENTRY_KIND_BASE + index as u32),
            size_label: ResourceId(TEXT_ENTRY_SIZE_BASE + index as u32),
            icon: ResourceId(IMAGE_ENTRY_BASE + index as u32),
            command: CommandId(COMMAND_ENTRY_BASE + index as u32),
            selected: selected == Some(index),
            editing: renaming == Some(index),
        };
    }
    let browser_view = match view {
        ExplorerView::Grid => FileBrowserView::Grid,
        ExplorerView::List => FileBrowserView::List,
        ExplorerView::Details => FileBrowserView::Details,
    };
    if build_file_browser(
        &mut ui,
        page_node,
        FileBrowserSpec {
            layout: LayoutSpec {
                width: Length::Fill(1),
                height: Length::Fill(1),
                gap: 1,
                ..LayoutSpec::default()
            },
            navigation_width: 228,
            navigation_heading: TEXT_QUICK_ACCESS,
            empty_text: TEXT_EMPTY,
            name_heading: TEXT_NAME_HEADER,
            kind_heading: TEXT_KIND_HEADER,
            size_heading: TEXT_SIZE_HEADER,
            view: browser_view,
            grid_columns: grid_columns(viewport_width) as u8,
        },
        &tree_items,
        &items[..entry_len],
        entry_nodes,
    )
    .is_err()
    {
        return;
    }

    if let Ok(status) = ui.status_bar(
        page_node,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Px(34),
            padding: Edges::symmetric(10, 4),
            gap: 8,
            align: Align::Center,
            ..LayoutSpec::default()
        },
    ) {
        let _ = ui.text(
            status,
            TEXT_STATUS,
            LayoutSpec {
                width: Length::Fill(1),
                height: Length::Fill(1),
                ..LayoutSpec::default()
            },
        );
        let _ = ui.text(
            status,
            TEXT_FS_STATUS,
            LayoutSpec {
                width: Length::Px(220),
                height: Length::Fill(1),
                align: Align::End,
                ..LayoutSpec::default()
            },
        );
    }
}

fn tree_item(
    label: ResourceId,
    icon: ResourceId,
    command: CommandId,
    depth: u8,
    selected: CommandId,
) -> FileBrowserTreeItem {
    FileBrowserTreeItem {
        label,
        icon,
        command,
        depth,
        selected: command == selected,
        disabled: false,
    }
}

fn add_menu_label<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    label: ResourceId,
    width: u16,
) {
    let mut button = NodeSpec::new(ComponentKind::Button);
    button.layout = LayoutSpec {
        width: Length::Px(width),
        height: Length::Px(24),
        ..LayoutSpec::default()
    };
    button.content = Content::Text(label);
    button.accessible_name = label;
    button.role = SemanticRole::Button;
    button.style = style_class::GHOST;
    let _ = ui.component(parent, button);
}

fn add_command_button<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    label: ResourceId,
    image: ResourceId,
    command: CommandId,
    width: u16,
    disabled: bool,
) {
    let mut button = NodeSpec::new(ComponentKind::Button);
    button.layout = LayoutSpec {
        width: Length::Px(width),
        height: Length::Px(40),
        ..LayoutSpec::default()
    };
    button.command = command;
    button.accessible_name = label;
    button.role = SemanticRole::Button;
    if disabled {
        button.state.insert(NodeState::DISABLED);
    }
    let Ok(button) = ui.component(parent, button) else {
        return;
    };
    let Ok(row) = ui.row(
        button,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            padding: Edges::symmetric(8, 6),
            gap: 6,
            align: Align::Center,
            ..LayoutSpec::default()
        },
    ) else {
        return;
    };
    let _ = ui.image(
        row,
        image,
        ResourceId(0),
        LayoutSpec {
            width: Length::Px(25),
            height: Length::Px(25),
            ..LayoutSpec::default()
        },
    );
    let _ = ui.text(
        row,
        label,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            ..LayoutSpec::default()
        },
    );
}

fn add_view_button<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    label: ResourceId,
    command: CommandId,
    active: bool,
) {
    let mut button = NodeSpec::new(ComponentKind::Button);
    button.layout = LayoutSpec {
        width: Length::Px(40),
        height: Length::Px(40),
        ..LayoutSpec::default()
    };
    button.content = Content::Text(label);
    button.accessible_name = label;
    button.command = command;
    button.role = SemanticRole::Button;
    if active {
        button.state.insert(NodeState::SELECTED);
    }
    let _ = ui.component(parent, button);
}

fn build_popup_tree(
    runtime: &mut PopupRuntime,
    has_selection: bool,
    selected_read_only: bool,
    has_clipboard: bool,
    current_read_only: bool,
) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let surface = ui.menu(root, LayoutSpec::fill()).unwrap_or(NodeId::NONE);
    if surface.is_none() {
        return;
    }
    let mut column = NodeSpec::new(ComponentKind::Column);
    column.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        padding: Edges::all(7),
        gap: 5,
        ..LayoutSpec::default()
    };
    let Ok(column) = ui.component(surface, column) else {
        return;
    };
    for (label, command, disabled) in [
        (TEXT_POPUP_CREATE, COMMAND_POPUP_CREATE, current_read_only),
        (TEXT_COPY, COMMAND_POPUP_COPY, !has_selection),
        (
            TEXT_CUT,
            COMMAND_POPUP_CUT,
            !has_selection || selected_read_only,
        ),
        (
            TEXT_PASTE,
            COMMAND_POPUP_PASTE,
            !has_clipboard || current_read_only,
        ),
        (
            TEXT_RENAME,
            COMMAND_POPUP_RENAME,
            !has_selection || selected_read_only,
        ),
        (
            TEXT_DELETE,
            COMMAND_POPUP_DELETE,
            !has_selection || selected_read_only,
        ),
    ] {
        add_popup_item(&mut ui, column, label, command, disabled);
    }
}

fn build_create_popup_tree(runtime: &mut PopupRuntime) {
    let root = runtime.tree().root();
    let mut ui = runtime.builder();
    let surface = ui.menu(root, LayoutSpec::fill()).unwrap_or(NodeId::NONE);
    if surface.is_none() {
        return;
    }
    let mut column = NodeSpec::new(ComponentKind::Column);
    column.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Fill(1),
        padding: Edges::all(7),
        gap: 6,
        ..LayoutSpec::default()
    };
    let Ok(column) = ui.component(surface, column) else {
        return;
    };
    add_popup_item(
        &mut ui,
        column,
        TEXT_CREATE_FOLDER,
        COMMAND_CREATE_FOLDER,
        false,
    );
    add_popup_item(
        &mut ui,
        column,
        TEXT_CREATE_TEXT,
        COMMAND_CREATE_TEXT,
        false,
    );
}

fn add_popup_item<const N: usize>(
    ui: &mut rustos_system_ui::UiBuilder<'_, N>,
    parent: NodeId,
    label: ResourceId,
    command: CommandId,
    disabled: bool,
) {
    let mut spec = NodeSpec::new(ComponentKind::Button);
    spec.layout = LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(38),
        ..LayoutSpec::default()
    };
    spec.content = Content::Text(label);
    spec.command = command;
    spec.role = SemanticRole::MenuItem;
    spec.accessible_name = label;
    if disabled {
        spec.state.insert(NodeState::DISABLED);
    }
    let _ = ui.component(parent, spec);
}

struct ExplorerResources<'a> {
    current: &'a ExplorerPath,
    entries: &'a [ExplorerEntry; MAX_ENTRIES],
    status: &'a FixedText<STATUS_CAPACITY>,
    rename: &'a FixedText<NAME_CAPACITY>,
    renaming: Option<usize>,
    view: ExplorerView,
}

impl ExplorerResources<'_> {
    fn text(&self, resource: ResourceId) -> &str {
        if (TEXT_ENTRY_BASE..TEXT_ENTRY_BASE + MAX_ENTRIES as u32).contains(&resource.0) {
            let index = (resource.0 - TEXT_ENTRY_BASE) as usize;
            return if self.renaming == Some(index) {
                self.rename.as_str()
            } else if self.view == ExplorerView::Grid {
                self.entries[index].grid_label.as_str()
            } else {
                self.entries[index].name.as_str()
            };
        }
        if (TEXT_ENTRY_KIND_BASE..TEXT_ENTRY_KIND_BASE + MAX_ENTRIES as u32).contains(&resource.0) {
            return self.entries[(resource.0 - TEXT_ENTRY_KIND_BASE) as usize]
                .kind_label
                .as_str();
        }
        if (TEXT_ENTRY_SIZE_BASE..TEXT_ENTRY_SIZE_BASE + MAX_ENTRIES as u32).contains(&resource.0) {
            return self.entries[(resource.0 - TEXT_ENTRY_SIZE_BASE) as usize]
                .size_label
                .as_str();
        }
        match resource {
            TEXT_PATH => self.current.as_str(),
            TEXT_STATUS => self.status.as_str(),
            TEXT_BACK => "Назад",
            TEXT_UP => "Вверх",
            TEXT_NEW_FOLDER => "Новая папка",
            TEXT_COPY => "Копировать",
            TEXT_CUT => "Вырезать",
            TEXT_PASTE => "Вставить",
            TEXT_RENAME => "Переименовать",
            TEXT_DELETE => "Удалить",
            TEXT_GRID => "С",
            TEXT_LIST => "Л",
            TEXT_DETAILS => "Т",
            TEXT_HOME => "Домой",
            TEXT_ROOT => "Этот компьютер",
            TEXT_BOOT => "Загрузка",
            TEXT_SYSTEM => "Система",
            TEXT_SOURCE => "Исходники",
            TEXT_NAME_HEADER => "Имя",
            TEXT_EMPTY => "В этой папке пока ничего нет",
            TEXT_POPUP_CREATE => "Создать                         >",
            TEXT_CREATE_FOLDER => "      Папку",
            TEXT_CREATE_TEXT => "      Текстовый файл",
            TEXT_LOCATION => "Текущий путь",
            TEXT_QUICK_ACCESS => "Папки",
            TEXT_MENU_FILE => "Файл",
            TEXT_MENU_EDIT => "Правка",
            TEXT_MENU_VIEW => "Вид",
            TEXT_MENU_FAVORITES => "Избранное",
            TEXT_MENU_HELP => "Справка",
            TEXT_SEARCH => "Поиск",
            TEXT_ADDRESS => "Адрес",
            TEXT_KIND_HEADER => "Тип",
            TEXT_SIZE_HEADER => "Размер",
            TEXT_FS_STATUS => "VaraniaFS · подключено",
            TEXT_SYSTEM_BIN => "Программы",
            TEXT_SYSTEM_LIB => "Библиотеки",
            _ => "",
        }
    }

    fn icon(&self, resource: ResourceId) -> Option<IconKind> {
        if (IMAGE_ENTRY_BASE..IMAGE_ENTRY_BASE + MAX_ENTRIES as u32).contains(&resource.0) {
            let index = (resource.0 - IMAGE_ENTRY_BASE) as usize;
            let entry = self.entries[index];
            return Some(icon_for_path(
                entry.name.as_str(),
                entry.kind == FileKind::Directory,
            ));
        }
        match resource {
            IMAGE_HOME => Some(IconKind::Home),
            IMAGE_ROOT => Some(IconKind::Drive),
            IMAGE_BOOT | IMAGE_SYSTEM | IMAGE_SOURCE | IMAGE_FOLDER => Some(IconKind::Folder),
            IMAGE_TEXT => Some(IconKind::Text),
            IMAGE_TRASH => Some(IconKind::Trash),
            IMAGE_GRID => Some(IconKind::Grid),
            IMAGE_BACK => Some(IconKind::ChevronLeft),
            IMAGE_FORWARD => Some(IconKind::ChevronRight),
            IMAGE_SEARCH => Some(IconKind::Search),
            _ => None,
        }
    }
}

struct ExplorerBackend<'framebuffer, 'resources> {
    framebuffer: &'framebuffer mut Framebuffer,
    resources: &'resources ExplorerResources<'resources>,
    icons: IconPack,
}

impl RenderBackend for ExplorerBackend<'_, '_> {
    fn shadow(&mut self, rect: Rect, radius: u8, color: Color, clip: Rect) {
        self.framebuffer.surface_shadow(rect, radius, color, clip);
    }

    fn fill(&mut self, rect: Rect, color: Color, clip: Rect) {
        self.framebuffer.fill_rect(rect.intersection(clip), color);
    }

    fn border(&mut self, rect: Rect, color: Color, width: u8, clip: Rect) {
        self.framebuffer
            .rounded_border_clipped(rect, 0, width, color, clip);
    }

    fn rounded_fill(&mut self, rect: Rect, color: Color, radius: u8, clip: Rect) {
        self.framebuffer
            .fill_rounded_rect_clipped(rect, radius, color, clip);
    }

    fn rounded_border(&mut self, rect: Rect, color: Color, width: u8, radius: u8, clip: Rect) {
        self.framebuffer
            .rounded_border_clipped(rect, radius, width, color, clip);
    }

    fn text(&mut self, rect: Rect, resource: ResourceId, color: Color, spec: FontSpec, clip: Rect) {
        if !rect.intersection(clip).is_empty() {
            draw_system_ui_text(
                self.framebuffer,
                rect,
                self.resources.text(resource),
                color,
                spec,
                clip,
            );
        }
    }

    fn image(&mut self, rect: Rect, resource: ResourceId, _: Color, clip: Rect) {
        if rect.intersection(clip).is_empty() {
            return;
        }
        if let Some(icon) = self.resources.icon(resource) {
            self.icons.draw(self.framebuffer, icon, rect);
        }
    }
}

fn explorer_theme(scale_milli: u16) -> Theme {
    let mut theme = Theme::light();
    theme.scale_milli = scale_milli;
    theme
}

fn popup_rect(x: i32, y: i32, width: u32, height: u32, viewport: Rect) -> Rect {
    let max_x = viewport.right().saturating_sub(width as i32);
    let max_y = viewport.bottom().saturating_sub(height as i32);
    Rect::new(
        x.clamp(viewport.x, max_x.max(viewport.x)),
        y.clamp(viewport.y, max_y.max(viewport.y)),
        width.min(viewport.width),
        height.min(viewport.height),
    )
}

fn grid_columns(viewport_width: u32) -> usize {
    viewport_width
        .saturating_sub(240)
        .checked_div(150)
        .unwrap_or(1)
        .clamp(2, 6) as usize
}

fn compact_label(value: &str, max_characters: usize) -> FixedText<NAME_CAPACITY> {
    let mut result = FixedText::EMPTY;
    if value.chars().count() <= max_characters {
        result.set(value);
        return result;
    }
    for character in value.chars().take(max_characters.saturating_sub(3)) {
        if result.write_char(character).is_err() {
            break;
        }
    }
    let _ = result.write_str("...");
    result
}

fn entry_after(left: &ExplorerEntry, right: &ExplorerEntry) -> bool {
    if left.kind != right.kind {
        return left.kind == FileKind::File;
    }
    compare_ascii_case_insensitive(left.name.as_str(), right.name.as_str()).is_gt()
}

fn compare_ascii_case_insensitive(left: &str, right: &str) -> core::cmp::Ordering {
    let mut left = left.bytes();
    let mut right = right.bytes();
    loop {
        match (left.next(), right.next()) {
            (Some(a), Some(b)) => {
                let ordering = a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase());
                if !ordering.is_eq() {
                    return ordering;
                }
            }
            (None, Some(_)) => return core::cmp::Ordering::Less,
            (Some(_), None) => return core::cmp::Ordering::Greater,
            (None, None) => return core::cmp::Ordering::Equal,
        }
    }
}

fn log_operation(operation: &str, path: &str) {
    serial::put_str("[explorer] operation=");
    serial::put_str(operation);
    serial::put_str(" path=");
    serial::put_str(path);
    serial::put_str("\n");
}

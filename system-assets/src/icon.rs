//! Семантическая библиотека системных иконок.

use rustos_video::{Color, Rect};

use crate::{PackId, PackMetadata, ResourcePack};

/// Основные типы объектов, нужные desktop, explorer и file picker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IconKind {
    /// Неизвестный файл.
    #[default]
    File,
    /// Каталог.
    Folder,
    /// Открытый каталог.
    FolderOpen,
    /// Обычный текст.
    Text,
    /// Исходник Rust.
    RustSource,
    /// Динамическая библиотека RustOS.
    DynamicLibrary,
    /// Исполняемый RUNE/ELF-файл.
    Executable,
    /// Изображение.
    Image,
    /// Аудио.
    Audio,
    /// Видео.
    Video,
    /// Архив или пакет ресурсов.
    Archive,
    /// Накопитель или раздел.
    Drive,
    /// Терминал.
    Terminal,
    /// Настройки.
    Settings,
    /// Корзина.
    Trash,
    /// Домашняя/обзорная страница.
    Home,
    /// Поиск.
    Search,
    /// Главное меню.
    Menu,
    /// Сетка приложений.
    Grid,
    /// Завершение работы.
    Power,
    /// Информация.
    Info,
    /// Успешное действие.
    Success,
    /// Предупреждение.
    Warning,
    /// Свернуть окно.
    Minimize,
    /// Развернуть окно.
    Maximize,
    /// Восстановить окно.
    Restore,
    /// Закрыть окно.
    Close,
    /// Стрелка назад.
    ChevronLeft,
    /// Стрелка вперёд.
    ChevronRight,
}

/// Минимальный raster API. Он умышленно не зависит от kernel framebuffer:
/// тот же icon pack рисуется в CPU surface, GPU command encoder или тестовый
/// backend.
pub trait IconTarget {
    /// Залить прямоугольник.
    fn fill(&mut self, rect: Rect, color: Color);
    /// Нарисовать однопиксельную рамку.
    fn stroke(&mut self, rect: Rect, color: Color);

    /// Скруглённая заливка. Backend без curved primitive корректно использует
    /// прямоугольный fallback; CPU/GPU backend может дать современную форму.
    fn rounded_fill(&mut self, rect: Rect, _radius: u8, color: Color) {
        self.fill(rect, color);
    }

    /// Скруглённая однопиксельная рамка с прямоугольным fallback.
    fn rounded_stroke(&mut self, rect: Rect, _radius: u8, color: Color) {
        self.stroke(rect, color);
    }
}

/// Цвета стандартной геометрии иконок.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IconPalette {
    /// Основной фон объекта.
    pub surface: Color,
    /// Более светлая грань.
    pub surface_light: Color,
    /// Тёмная грань и контур.
    pub outline: Color,
    /// Акцент.
    pub accent: Color,
    /// Текст/символ внутри иконки.
    pub ink: Color,
    /// Цвет папок.
    pub folder: Color,
}

/// Подключаемый пакет иконок.
#[derive(Clone, Copy)]
pub struct IconPack {
    metadata: PackMetadata,
    /// Палитра пакета.
    pub palette: IconPalette,
    renderer: fn(&mut dyn IconTarget, IconKind, Rect, IconPalette),
}

impl IconPack {
    /// Создаёт пользовательский icon pack с собственной геометрией.
    pub const fn new(
        metadata: PackMetadata,
        palette: IconPalette,
        renderer: fn(&mut dyn IconTarget, IconKind, Rect, IconPalette),
    ) -> Self {
        Self {
            metadata,
            palette,
            renderer,
        }
    }

    /// Рисует семантическую иконку.
    pub fn draw(self, target: &mut dyn IconTarget, kind: IconKind, rect: Rect) {
        (self.renderer)(target, kind, rect, self.palette);
    }
}

impl ResourcePack for IconPack {
    fn metadata(&self) -> PackMetadata {
        self.metadata
    }
}

/// Современный системный пакет: яркий синий акцент, мягкие поверхности и
/// чистая line/filled geometry. Он используется desktop по умолчанию, а
/// classic/midnight/mono остаются переключаемыми темами совместимости.
pub const AURORA_ICON_PACK: IconPack = IconPack {
    metadata: PackMetadata {
        id: PackId(0x2004),
        name: "aurora",
        version: 1,
    },
    palette: IconPalette {
        surface: Color::rgb(239, 245, 255),
        surface_light: Color::rgb(255, 255, 255),
        outline: Color::rgb(35, 52, 78),
        accent: Color::rgb(29, 112, 246),
        ink: Color::rgb(20, 31, 49),
        folder: Color::rgb(255, 187, 63),
    },
    renderer: standard_icon,
};

/// Классическая тема с жёлтыми папками.
pub const CLASSIC_ICON_PACK: IconPack = IconPack {
    metadata: PackMetadata {
        id: PackId(0x2001),
        name: "classic",
        version: 1,
    },
    palette: IconPalette {
        surface: Color::rgb(225, 234, 242),
        surface_light: Color::rgb(249, 252, 254),
        outline: Color::rgb(35, 49, 67),
        accent: Color::rgb(55, 199, 219),
        ink: Color::rgb(15, 25, 38),
        folder: Color::rgb(242, 186, 62),
    },
    renderer: standard_icon,
};

/// Тёмная тема для панелей и профессиональных приложений.
pub const MIDNIGHT_ICON_PACK: IconPack = IconPack {
    metadata: PackMetadata {
        id: PackId(0x2002),
        name: "midnight",
        version: 1,
    },
    palette: IconPalette {
        surface: Color::rgb(39, 53, 72),
        surface_light: Color::rgb(71, 91, 115),
        outline: Color::rgb(7, 12, 20),
        accent: Color::rgb(91, 218, 229),
        ink: Color::rgb(238, 246, 250),
        folder: Color::rgb(224, 161, 49),
    },
    renderer: standard_icon,
};

/// Одноцветная тема доступности и печати.
pub const MONO_ICON_PACK: IconPack = IconPack {
    metadata: PackMetadata {
        id: PackId(0x2003),
        name: "mono",
        version: 1,
    },
    palette: IconPalette {
        surface: Color::rgb(232, 232, 232),
        surface_light: Color::rgb(255, 255, 255),
        outline: Color::rgb(0, 0, 0),
        accent: Color::rgb(70, 70, 70),
        ink: Color::rgb(0, 0, 0),
        folder: Color::rgb(190, 190, 190),
    },
    renderer: standard_icon,
};

/// Выбирает иконку по имени файла. Это presentation policy, а не логика VFS:
/// неизвестные расширения безопасно получают [`IconKind::File`].
pub fn icon_for_path(path: &str, is_directory: bool) -> IconKind {
    if is_directory {
        return IconKind::Folder;
    }
    let extension = path.rsplit_once('.').map(|(_, extension)| extension);
    match extension {
        Some(value) if value.eq_ignore_ascii_case("txt") || value.eq_ignore_ascii_case("md") => {
            IconKind::Text
        }
        Some(value) if value.eq_ignore_ascii_case("rs") => IconKind::RustSource,
        Some(value)
            if value.eq_ignore_ascii_case("dll")
                || value.eq_ignore_ascii_case("rdll")
                || value.eq_ignore_ascii_case("so") =>
        {
            IconKind::DynamicLibrary
        }
        Some(value)
            if value.eq_ignore_ascii_case("rune")
                || value.eq_ignore_ascii_case("elf")
                || value.eq_ignore_ascii_case("exe") =>
        {
            IconKind::Executable
        }
        Some(value)
            if value.eq_ignore_ascii_case("png")
                || value.eq_ignore_ascii_case("jpg")
                || value.eq_ignore_ascii_case("jpeg")
                || value.eq_ignore_ascii_case("bmp") =>
        {
            IconKind::Image
        }
        Some(value)
            if value.eq_ignore_ascii_case("wav")
                || value.eq_ignore_ascii_case("flac")
                || value.eq_ignore_ascii_case("mp3") =>
        {
            IconKind::Audio
        }
        Some(value)
            if value.eq_ignore_ascii_case("mp4")
                || value.eq_ignore_ascii_case("mkv")
                || value.eq_ignore_ascii_case("webm") =>
        {
            IconKind::Video
        }
        Some(value)
            if value.eq_ignore_ascii_case("zip")
                || value.eq_ignore_ascii_case("tar")
                || value.eq_ignore_ascii_case("rpack") =>
        {
            IconKind::Archive
        }
        _ => IconKind::File,
    }
}

fn standard_icon(target: &mut dyn IconTarget, kind: IconKind, rect: Rect, p: IconPalette) {
    match kind {
        IconKind::Folder => folder(target, rect, p, false),
        IconKind::FolderOpen => folder(target, rect, p, true),
        IconKind::Terminal => terminal(target, rect, p),
        IconKind::Trash => trash(target, rect, p),
        IconKind::Drive => drive(target, rect, p),
        IconKind::Settings => settings(target, rect, p),
        IconKind::Text => document(target, rect, p, DocumentMark::Lines),
        IconKind::RustSource => document(target, rect, p, DocumentMark::Rust),
        IconKind::DynamicLibrary => document(target, rect, p, DocumentMark::Blocks),
        IconKind::Executable => document(target, rect, p, DocumentMark::Play),
        IconKind::Image => document(target, rect, p, DocumentMark::Picture),
        IconKind::Audio => document(target, rect, p, DocumentMark::Audio),
        IconKind::Video => document(target, rect, p, DocumentMark::Video),
        IconKind::Archive => document(target, rect, p, DocumentMark::Archive),
        IconKind::File => document(target, rect, p, DocumentMark::None),
        IconKind::Home
        | IconKind::Search
        | IconKind::Menu
        | IconKind::Grid
        | IconKind::Power
        | IconKind::Info
        | IconKind::Success
        | IconKind::Warning
        | IconKind::Minimize
        | IconKind::Maximize
        | IconKind::Restore
        | IconKind::Close
        | IconKind::ChevronLeft
        | IconKind::ChevronRight => ui_icon(target, kind, rect, p),
    }
}

fn folder(target: &mut dyn IconTarget, rect: Rect, p: IconPalette, open: bool) {
    rounded_cell(target, rect, 2, 6, 10, 5, 2, p.folder);
    rounded_cell(target, rect, 2, 8, 20, 13, 3, p.outline);
    rounded_cell(target, rect, 3, 9, 18, 11, 2, p.folder);
    cell(target, rect, 4, 10, 16, 2, p.surface_light);
    if open {
        cell(target, rect, 1, 13, 21, 8, p.outline);
        cell(target, rect, 2, 14, 19, 6, p.folder);
        cell(target, rect, 5, 14, 13, 1, p.surface_light);
    }
}

#[derive(Clone, Copy)]
enum DocumentMark {
    None,
    Lines,
    Rust,
    Blocks,
    Play,
    Picture,
    Audio,
    Video,
    Archive,
}

fn document(target: &mut dyn IconTarget, rect: Rect, p: IconPalette, mark: DocumentMark) {
    cell(target, rect, 4, 2, 16, 20, p.outline);
    cell(target, rect, 5, 3, 14, 18, p.surface);
    cell(target, rect, 14, 3, 5, 5, p.surface_light);
    cell(target, rect, 15, 3, 4, 1, p.outline);
    cell(target, rect, 18, 4, 1, 4, p.outline);
    match mark {
        DocumentMark::None => {}
        DocumentMark::Lines => {
            for y in [10, 13, 16, 19] {
                cell(target, rect, 8, y, 8, 1, p.accent);
            }
        }
        DocumentMark::Rust => {
            cell(target, rect, 7, 10, 2, 8, p.accent);
            cell(target, rect, 9, 10, 5, 2, p.accent);
            cell(target, rect, 12, 12, 2, 3, p.accent);
            cell(target, rect, 9, 14, 5, 2, p.accent);
            cell(target, rect, 13, 16, 3, 2, p.accent);
        }
        DocumentMark::Blocks => {
            for (x, y) in [(7, 10), (12, 10), (7, 15), (12, 15)] {
                cell(target, rect, x, y, 4, 4, p.accent);
                stroke_cell(target, rect, x, y, 4, 4, p.ink);
            }
        }
        DocumentMark::Play => {
            for row in 0..8 {
                let width = if row < 4 { row + 2 } else { 9 - row };
                cell(target, rect, 9, 9 + row, width, 1, p.accent);
            }
        }
        DocumentMark::Picture => {
            cell(target, rect, 7, 9, 10, 10, p.accent);
            cell(target, rect, 8, 10, 8, 8, p.surface_light);
            cell(target, rect, 13, 11, 2, 2, p.accent);
            cell(target, rect, 8, 15, 8, 3, p.folder);
        }
        DocumentMark::Audio => {
            cell(target, rect, 11, 9, 2, 8, p.accent);
            cell(target, rect, 13, 9, 4, 2, p.accent);
            cell(target, rect, 8, 16, 4, 3, p.accent);
            cell(target, rect, 14, 15, 4, 3, p.accent);
        }
        DocumentMark::Video => {
            cell(target, rect, 7, 10, 10, 8, p.ink);
            cell(target, rect, 9, 12, 2, 4, p.accent);
            cell(target, rect, 11, 13, 3, 2, p.accent);
        }
        DocumentMark::Archive => {
            for y in (8..19).step_by(3) {
                cell(target, rect, 11, y, 3, 2, p.accent);
            }
        }
    }
}

fn terminal(target: &mut dyn IconTarget, rect: Rect, p: IconPalette) {
    // Мягкий корпус и чистый prompt сохраняют силуэт в диапазоне 16..64 px.
    rounded_cell(target, rect, 4, 5, 18, 17, 3, Color::rgb(5, 10, 16));
    rounded_cell(target, rect, 2, 2, 19, 18, 4, p.outline);
    rounded_cell(target, rect, 3, 3, 17, 16, 3, p.surface);
    cell(target, rect, 3, 3, 17, 4, p.surface_light);
    for x in [5, 8, 11] {
        cell(target, rect, x, 4, 2, 2, p.accent);
    }
    cell(target, rect, 4, 8, 15, 10, Color::rgb(7, 13, 21));
    cell(target, rect, 7, 11, 2, 2, p.accent);
    cell(target, rect, 9, 12, 2, 2, p.accent);
    cell(target, rect, 7, 15, 2, 1, p.accent);
    cell(target, rect, 11, 15, 5, 1, p.surface_light);
}

fn trash(target: &mut dyn IconTarget, rect: Rect, p: IconPalette) {
    rounded_cell(target, rect, 6, 7, 12, 15, 3, p.outline);
    rounded_cell(target, rect, 7, 8, 10, 13, 2, p.surface);
    rounded_cell(target, rect, 4, 5, 16, 3, 2, p.outline);
    rounded_cell(target, rect, 9, 3, 6, 2, 1, p.outline);
    for x in [9, 12, 15] {
        cell(target, rect, x, 10, 1, 8, p.accent);
    }
}

fn drive(target: &mut dyn IconTarget, rect: Rect, p: IconPalette) {
    rounded_cell(target, rect, 2, 7, 20, 12, 4, p.outline);
    rounded_cell(target, rect, 3, 8, 18, 9, 3, p.surface);
    cell(target, rect, 3, 14, 18, 3, p.surface_light);
    cell(target, rect, 16, 15, 3, 1, p.accent);
}

fn settings(target: &mut dyn IconTarget, rect: Rect, p: IconPalette) {
    cell(target, rect, 10, 2, 4, 20, p.outline);
    cell(target, rect, 2, 10, 20, 4, p.outline);
    cell(target, rect, 5, 5, 14, 14, p.outline);
    rounded_cell(target, rect, 7, 7, 10, 10, 5, p.surface);
    rounded_cell(target, rect, 10, 10, 4, 4, 2, p.accent);
}

fn ui_icon(target: &mut dyn IconTarget, kind: IconKind, rect: Rect, p: IconPalette) {
    match kind {
        IconKind::Home => {
            for row in 0..7 {
                let width = 3 + row * 2;
                cell(target, rect, 12 - width / 2, 4 + row, width, 1, p.accent);
            }
            rounded_cell(target, rect, 6, 10, 12, 10, 2, p.accent);
            cell(target, rect, 11, 15, 3, 5, p.surface_light);
        }
        IconKind::Search => {
            rounded_stroke_cell(target, rect, 4, 3, 13, 13, 7, p.ink);
            diagonal(target, rect, 15, 15, 5, true, p.ink);
        }
        IconKind::Menu => {
            for y in [6, 11, 16] {
                rounded_cell(target, rect, 4, y, 16, 2, 1, p.ink);
            }
        }
        IconKind::Grid => {
            for (x, y) in [(4, 4), (13, 4), (4, 13), (13, 13)] {
                rounded_cell(target, rect, x, y, 7, 7, 2, p.accent);
            }
        }
        IconKind::Power => {
            rounded_stroke_cell(target, rect, 4, 5, 16, 16, 8, p.accent);
            rounded_cell(target, rect, 11, 2, 3, 10, 2, p.accent);
            rounded_cell(target, rect, 9, 2, 7, 5, 2, p.surface);
            rounded_cell(target, rect, 11, 2, 3, 9, 2, p.accent);
        }
        IconKind::Info => {
            rounded_cell(target, rect, 2, 2, 20, 20, 10, p.accent);
            rounded_cell(target, rect, 11, 9, 3, 9, 1, p.surface_light);
            rounded_cell(target, rect, 11, 5, 3, 3, 2, p.surface_light);
        }
        IconKind::Success => {
            rounded_cell(target, rect, 2, 2, 20, 20, 10, Color::rgb(34, 179, 119));
            diagonal(target, rect, 6, 12, 5, true, p.surface_light);
            diagonal(target, rect, 10, 16, 8, false, p.surface_light);
        }
        IconKind::Warning => {
            for row in 0..18 {
                let width = 2 + row;
                cell(
                    target,
                    rect,
                    12 - width / 2,
                    3 + row,
                    width,
                    1,
                    Color::rgb(246, 174, 45),
                );
            }
            rounded_cell(target, rect, 11, 8, 3, 7, 1, p.ink);
            rounded_cell(target, rect, 11, 17, 3, 3, 2, p.ink);
        }
        IconKind::Minimize => rounded_cell(target, rect, 5, 16, 14, 2, 1, p.ink),
        IconKind::Maximize => rounded_stroke_cell(target, rect, 5, 5, 14, 14, 3, p.ink),
        IconKind::Restore => {
            rounded_stroke_cell(target, rect, 7, 4, 13, 13, 3, p.ink);
            rounded_stroke_cell(target, rect, 4, 7, 13, 13, 3, p.ink);
        }
        IconKind::Close => {
            diagonal(target, rect, 6, 6, 12, true, p.ink);
            diagonal(target, rect, 17, 6, 12, false, p.ink);
        }
        IconKind::ChevronLeft => {
            diagonal(target, rect, 14, 5, 7, false, p.ink);
            diagonal(target, rect, 8, 11, 7, true, p.ink);
        }
        IconKind::ChevronRight => {
            diagonal(target, rect, 9, 5, 7, true, p.ink);
            diagonal(target, rect, 15, 11, 7, false, p.ink);
        }
        _ => {}
    }
}

fn diagonal(
    target: &mut dyn IconTarget,
    rect: Rect,
    start_x: u32,
    start_y: u32,
    length: u32,
    down_right: bool,
    color: Color,
) {
    for step in 0..length {
        let x = if down_right {
            start_x.saturating_add(step)
        } else {
            start_x.saturating_sub(step)
        };
        rounded_cell(target, rect, x, start_y + step, 2, 2, 1, color);
    }
}

fn cell(
    target: &mut dyn IconTarget,
    rect: Rect,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: Color,
) {
    let left = rect.x + ((u64::from(x) * u64::from(rect.width)) / 24) as i32;
    let top = rect.y + ((u64::from(y) * u64::from(rect.height)) / 24) as i32;
    let right = rect.x + ((u64::from(x + width) * u64::from(rect.width) + 23) / 24) as i32;
    let bottom = rect.y + ((u64::from(y + height) * u64::from(rect.height) + 23) / 24) as i32;
    target.fill(
        Rect::new(
            left,
            top,
            (right - left).max(1) as u32,
            (bottom - top).max(1) as u32,
        ),
        color,
    );
}

fn rounded_cell(
    target: &mut dyn IconTarget,
    rect: Rect,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    radius: u8,
    color: Color,
) {
    let scaled = scaled_cell(rect, x, y, width, height);
    let scale = rect.width.min(rect.height).max(1);
    let scaled_radius = ((u32::from(radius) * scale + 23) / 24).clamp(1, u32::from(u8::MAX));
    target.rounded_fill(scaled, scaled_radius as u8, color);
}

fn rounded_stroke_cell(
    target: &mut dyn IconTarget,
    rect: Rect,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    radius: u8,
    color: Color,
) {
    let scaled = scaled_cell(rect, x, y, width, height);
    let scale = rect.width.min(rect.height).max(1);
    let scaled_radius = ((u32::from(radius) * scale + 23) / 24).clamp(1, u32::from(u8::MAX));
    target.rounded_stroke(scaled, scaled_radius as u8, color);
}

fn scaled_cell(rect: Rect, x: u32, y: u32, width: u32, height: u32) -> Rect {
    let left = rect.x + ((u64::from(x) * u64::from(rect.width)) / 24) as i32;
    let top = rect.y + ((u64::from(y) * u64::from(rect.height)) / 24) as i32;
    let right = rect.x + ((u64::from(x + width) * u64::from(rect.width) + 23) / 24) as i32;
    let bottom = rect.y + ((u64::from(y + height) * u64::from(rect.height) + 23) / 24) as i32;
    Rect::new(
        left,
        top,
        (right - left).max(1) as u32,
        (bottom - top).max(1) as u32,
    )
}

fn stroke_cell(
    target: &mut dyn IconTarget,
    rect: Rect,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: Color,
) {
    let left = rect.x + ((u64::from(x) * u64::from(rect.width)) / 24) as i32;
    let top = rect.y + ((u64::from(y) * u64::from(rect.height)) / 24) as i32;
    let right = rect.x + ((u64::from(x + width) * u64::from(rect.width)) / 24) as i32;
    let bottom = rect.y + ((u64::from(y + height) * u64::from(rect.height)) / 24) as i32;
    target.stroke(
        Rect::new(
            left,
            top,
            (right - left).max(1) as u32,
            (bottom - top).max(1) as u32,
        ),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Counter(usize);

    impl IconTarget for Counter {
        fn fill(&mut self, _: Rect, _: Color) {
            self.0 += 1;
        }

        fn stroke(&mut self, _: Rect, _: Color) {
            self.0 += 1;
        }
    }

    #[test]
    fn extension_mapping_is_case_insensitive() {
        assert_eq!(
            icon_for_path("lib/UI.RDLL", false),
            IconKind::DynamicLibrary
        );
        assert_eq!(icon_for_path("src/main.RS", false), IconKind::RustSource);
        assert_eq!(
            icon_for_path("system/bin/init.rune", false),
            IconKind::Executable
        );
        assert_eq!(icon_for_path("home", true), IconKind::Folder);
    }

    #[test]
    fn every_icon_emits_primitives() {
        let icons = [
            IconKind::File,
            IconKind::Folder,
            IconKind::FolderOpen,
            IconKind::Text,
            IconKind::RustSource,
            IconKind::DynamicLibrary,
            IconKind::Executable,
            IconKind::Image,
            IconKind::Audio,
            IconKind::Video,
            IconKind::Archive,
            IconKind::Drive,
            IconKind::Terminal,
            IconKind::Settings,
            IconKind::Trash,
            IconKind::Home,
            IconKind::Search,
            IconKind::Menu,
            IconKind::Grid,
            IconKind::Power,
            IconKind::Info,
            IconKind::Success,
            IconKind::Warning,
            IconKind::Minimize,
            IconKind::Maximize,
            IconKind::Restore,
            IconKind::Close,
            IconKind::ChevronLeft,
            IconKind::ChevronRight,
        ];
        for kind in icons {
            for pack in [AURORA_ICON_PACK, CLASSIC_ICON_PACK] {
                let mut counter = Counter::default();
                pack.draw(&mut counter, kind, Rect::new(0, 0, 48, 48));
                assert!(counter.0 > 0, "empty icon {kind:?}");
            }
        }
    }
}

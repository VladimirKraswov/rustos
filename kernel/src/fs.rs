//! Ранний VFS bootstrap: read-only RIFS initramfs + writable RAM overlay.
//!
//! Этот модуль даёт рабочий файловый workflow до появления процессов и IPC.
//! Публичная семантика совпадает с будущим `vfsd`, но реализация временно
//! вызывается terminal напрямую. Persistent VaraniaFS и disk drivers заменят
//! backend, не меняя команды и `vfs.dll` API.

use core::{
    ptr::addr_of_mut,
    slice, str,
    sync::atomic::{AtomicU8, Ordering},
};

use rustos_abi::bootinfo::BootInitramfs;

/// Максимальная длина нормализованного абсолютного пути вместе с запасом под NUL.
pub const PATH_CAPACITY: usize = 96;
/// Максимальный размер файла раннего RAM overlay.
pub const FILE_CAPACITY: usize = 4096;

/// Вмещаемость bootstrap-таблицы: 32 узла × (путь 96 + данные 4 KiB) —
/// сознательно мала, RAM overlay не для больших данных.
const MAX_NODES: usize = 32;
/// Верхняя граница записей одного `list` (дольше — обрезается).
const MAX_LIST_ENTRIES: usize = 32;
/// Максимальная длина имени объекта в каталоге.
const NAME_CAPACITY: usize = 64;
/// Максимальная глубина пути (защита от переполнения стека normalize).
const MAX_COMPONENTS: usize = 24;

// RIFS v1 — плоский образ initramfs, который создаёт tools/pack
// (формат описан в docs/VFS.md и исходниках pack). Header 32 байта:
// magic/version/count/size, дальше таблица из 64-байтовых записей
// (name 48 + reserved + size + offset), данные выровнены по 4 KiB.
const RIFS_MAGIC: u32 = 0x52_49_46_53;
const RIFS_VERSION: u32 = 1;
const RIFS_HEADER_SIZE: usize = 32;
const RIFS_ENTRY_SIZE: usize = 64;
const RIFS_NAME_SIZE: usize = 48;

/// Тип VFS-объекта.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    /// Обычный поток байт.
    File = 1,
    /// Каталог имён.
    Directory = 2,
}

/// Ошибка файловой операции bootstrap VFS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    /// Путь пуст, повреждён или длиннее лимита раннего runtime.
    InvalidPath,
    /// Объект не найден.
    NotFound,
    /// Объект с таким именем уже существует.
    AlreadyExists,
    /// В пути ожидался каталог.
    NotDirectory,
    /// Операция ожидала файл, но получила каталог.
    IsDirectory,
    /// Целевой backend доступен только для чтения.
    ReadOnly,
    /// Каталог содержит дочерние объекты.
    NotEmpty,
    /// Закончились фиксированные bootstrap slots.
    NoSpace,
    /// Данные не помещаются в ранний RAM-file.
    FileTooLarge,
    /// Initramfs имеет повреждённый RIFS layout.
    CorruptImage,
}

impl FsError {
    /// Короткое диагностическое сообщение для terminal.
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidPath => "INVALID PATH",
            Self::NotFound => "NOT FOUND",
            Self::AlreadyExists => "ALREADY EXISTS",
            Self::NotDirectory => "NOT A DIRECTORY",
            Self::IsDirectory => "IS A DIRECTORY",
            Self::ReadOnly => "READ-ONLY FILESYSTEM",
            Self::NotEmpty => "DIRECTORY NOT EMPTY",
            Self::NoSpace => "RAM FILE TABLE FULL",
            Self::FileTooLarge => "FILE TOO LARGE FOR BOOTSTRAP RAMFS",
            Self::CorruptImage => "CORRUPT INITRAMFS",
        }
    }
}

/// Одна запись результата `list`.
#[derive(Clone, Copy)]
pub struct DirectoryEntry {
    name: [u8; NAME_CAPACITY],
    name_len: u8,
    kind: FileKind,
    read_only: bool,
}

impl DirectoryEntry {
    const EMPTY: Self = Self {
        name: [0; NAME_CAPACITY],
        name_len: 0,
        kind: FileKind::File,
        read_only: false,
    };

    /// Имя без родительского пути.
    pub fn name(&self) -> &str {
        str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }

    /// Тип объекта.
    pub const fn kind(&self) -> FileKind {
        self.kind
    }

    /// Находится ли объект на read-only mount.
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// Фиксированный результат чтения каталога без heap.
pub struct DirectoryListing {
    entries: [DirectoryEntry; MAX_LIST_ENTRIES],
    len: usize,
}

impl DirectoryListing {
    const fn new() -> Self {
        Self {
            entries: [DirectoryEntry::EMPTY; MAX_LIST_ENTRIES],
            len: 0,
        }
    }

    /// Заполненные directory entries.
    pub fn entries(&self) -> &[DirectoryEntry] {
        &self.entries[..self.len]
    }

    fn push(&mut self, name: &[u8], kind: FileKind, read_only: bool) {
        if name.is_empty() || name.len() > NAME_CAPACITY || self.len == MAX_LIST_ENTRIES {
            return;
        }
        if self
            .entries()
            .iter()
            .any(|entry| entry.name() == as_str(name))
        {
            return;
        }
        let entry = &mut self.entries[self.len];
        entry.name[..name.len()].copy_from_slice(name);
        entry.name_len = name.len() as u8;
        entry.kind = kind;
        entry.read_only = read_only;
        self.len += 1;
    }
}

/// Метаданные VFS-объекта.
#[derive(Clone, Copy, Debug)]
pub struct FileStat {
    /// Тип объекта.
    pub kind: FileKind,
    /// Размер файла; для каталога ноль.
    pub size: usize,
    /// Находится ли объект на read-only mount.
    pub read_only: bool,
}

#[repr(u8)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum NodeKind {
    Empty = 0,
    File = 1,
    Directory = 2,
}

/// Узел RAM overlay: абсолютный путь + данные файла. Путь хранится
/// целиком (без ссылок на родительские каталоги) — поиск линейный,
/// но при MAX_NODES это не критично.
#[derive(Clone, Copy)]
struct Node {
    kind: NodeKind,
    path_len: u8,
    data_len: u16,
    path: [u8; PATH_CAPACITY],
    data: [u8; FILE_CAPACITY],
}

impl Node {
    const EMPTY: Self = Self {
        kind: NodeKind::Empty,
        path_len: 0,
        data_len: 0,
        path: [0; PATH_CAPACITY],
        data: [0; FILE_CAPACITY],
    };

    fn path(&self) -> &[u8] {
        &self.path[..self.path_len as usize]
    }
}

struct OverlayStorage {
    nodes: [Node; MAX_NODES],
}

impl OverlayStorage {
    const EMPTY: Self = Self {
        nodes: [Node::EMPTY; MAX_NODES],
    };
}

// 32 * ~4 KiB живут в kernel BSS, а не на boot stack. До scheduler
// существует ровно один DesktopSession, поэтому facade BootstrapFs является
// единственным владельцем storage. После процессов storage исчезнет вместе с
// этим bootstrap backend.
static mut OVERLAY: OverlayStorage = OverlayStorage::EMPTY;

// `BootstrapFs` является лёгким клиентом общего bootstrap VFS. Раньше каждый
// новый терминал обнулял OVERLAY, потому что GUI допускал ровно один его
// экземпляр. Для независимых окон это недопустимо: запуск второго shell не
// должен стирать файлы первого. 0 = не готово, 1 = инициализируется, 2 = готово.
// Когда vfsd окончательно заменит bootstrap backend, эта синхронизация станет
// обычным подключением клиента к capability сервиса.
static OVERLAY_STATE: AtomicU8 = AtomicU8::new(0);

/// Возвращает файл из RIFS initramfs без монтирования RAM overlay.
/// Используется ранним ELF loader'ом и bootstrap VFS syscall до запуска
/// отдельного `vfsd`.
pub fn initramfs_file(initramfs: BootInitramfs, path: &str) -> Result<&'static [u8], FsError> {
    let relative = path
        .strip_prefix("/boot/")
        .unwrap_or(path.strip_prefix('/').unwrap_or(path));
    if relative.is_empty() || initramfs.phys_addr == 0 {
        return Err(FsError::NotFound);
    }
    let size = usize::try_from(initramfs.size).map_err(|_| FsError::CorruptImage)?;
    if size < RIFS_HEADER_SIZE {
        return Err(FsError::CorruptImage);
    }
    // SAFETY: initramfs входит в kernel reservation и identity-mapped на весь
    // срок работы. Поэтому возвращаемый slice действительно static.
    let bytes = unsafe { slice::from_raw_parts(initramfs.phys_addr as *const u8, size) };
    Rifs::parse(bytes)?
        .entries()
        .find(|entry| entry.name == relative.as_bytes())
        .map(|entry| entry.data)
        .ok_or(FsError::NotFound)
}

/// Ранний VFS facade, совместимый по семантике с будущим `vfsd`.
pub struct BootstrapFs {
    initramfs: *const u8,
    initramfs_size: usize,
    overlay: *mut OverlayStorage,
}

impl BootstrapFs {
    /// Подключает клиента к общему bootstrap VFS.
    ///
    /// Первый клиент монтирует initramfs и создаёт writable RAM directories;
    /// последующие получают тот же namespace, не очищая уже созданные файлы.
    pub fn new(initramfs: BootInitramfs) -> Self {
        let overlay = addr_of_mut!(OVERLAY);
        let mut fs = Self {
            initramfs: initramfs.phys_addr as *const u8,
            initramfs_size: usize::try_from(initramfs.size).unwrap_or(0),
            overlay,
        };

        if OVERLAY_STATE
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // SAFETY: состояние 1 принадлежит этому потоку; до Release-store
            // ниже ни один другой клиент не обращается к storage. Нулевой
            // NodeKind является валидным Empty.
            unsafe { overlay.write_bytes(0, 1) };
            for path in [
                "/system",
                "/system/bin",
                "/system/lib",
                "/home",
                "/home/user",
                "/src",
                "/build",
            ] {
                let _ = fs.insert_node(path.as_bytes(), NodeKind::Directory, &[]);
            }
            OVERLAY_STATE.store(2, Ordering::Release);
        } else {
            while OVERLAY_STATE.load(Ordering::Acquire) != 2 {
                core::hint::spin_loop();
            }
        }
        fs
    }

    /// Нормализует `input` относительно `cwd` в абсолютный путь.
    pub fn normalize(
        &self,
        cwd: &str,
        input: &str,
        output: &mut [u8; PATH_CAPACITY],
    ) -> Result<usize, FsError> {
        normalize_path(cwd, input, output)
    }

    /// Проверяет тип и размер объекта.
    pub fn stat(&mut self, path: &str) -> Result<FileStat, FsError> {
        if path == "/" || path == "/boot" {
            return Ok(FileStat {
                kind: FileKind::Directory,
                size: 0,
                read_only: path == "/boot",
            });
        }
        if let Some(node) = self.find_node(path.as_bytes()) {
            return Ok(FileStat {
                kind: if node.kind == NodeKind::Directory {
                    FileKind::Directory
                } else {
                    FileKind::File
                },
                size: node.data_len as usize,
                read_only: false,
            });
        }
        if let Some((kind, size)) = self.rifs_stat(path)? {
            return Ok(FileStat {
                kind,
                size,
                read_only: true,
            });
        }
        Err(FsError::NotFound)
    }

    /// Читает directory entries.
    pub fn list(&mut self, path: &str) -> Result<DirectoryListing, FsError> {
        if self.stat(path)?.kind != FileKind::Directory {
            return Err(FsError::NotDirectory);
        }
        let mut listing = DirectoryListing::new();
        if path == "/" {
            listing.push(b"boot", FileKind::Directory, true);
        }
        self.list_overlay(path.as_bytes(), &mut listing);
        self.list_rifs(path, &mut listing)?;
        Ok(listing)
    }

    /// Читает файл целиком в предоставленный buffer.
    pub fn read(&mut self, path: &str, output: &mut [u8]) -> Result<usize, FsError> {
        if let Some(node) = self.find_node(path.as_bytes()) {
            if node.kind == NodeKind::Directory {
                return Err(FsError::IsDirectory);
            }
            let len = node.data_len as usize;
            if len > output.len() {
                return Err(FsError::FileTooLarge);
            }
            output[..len].copy_from_slice(&node.data[..len]);
            return Ok(len);
        }
        if matches!(self.rifs_stat(path)?, Some((FileKind::Directory, _)))
            || matches!(path, "/" | "/boot")
        {
            return Err(FsError::IsDirectory);
        }
        let data = self.rifs_file(path)?;
        if data.len() > output.len() {
            return Err(FsError::FileTooLarge);
        }
        output[..data.len()].copy_from_slice(data);
        Ok(data.len())
    }

    /// Создаёт каталог в RAM overlay.
    pub fn make_dir(&mut self, path: &str) -> Result<(), FsError> {
        self.ensure_new_writable_path(path)?;
        self.insert_node(path.as_bytes(), NodeKind::Directory, &[])
    }

    /// Создаёт либо атомарно заменяет небольшой RAM-file.
    pub fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        if data.len() > FILE_CAPACITY {
            return Err(FsError::FileTooLarge);
        }
        if path == "/" {
            return Err(FsError::IsDirectory);
        }
        if path.starts_with("/boot/") || path == "/boot" {
            return Err(FsError::ReadOnly);
        }
        if let Some(node) = self.find_node_mut(path.as_bytes()) {
            if node.kind == NodeKind::Directory {
                return Err(FsError::IsDirectory);
            }
            node.data[..data.len()].copy_from_slice(data);
            node.data[data.len()..].fill(0);
            node.data_len = data.len() as u16;
            return Ok(());
        }
        self.ensure_parent_directory(path)?;
        self.insert_node(path.as_bytes(), NodeKind::File, data)
    }

    /// Дописывает данные в конец RAM-file.
    pub fn append(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        if path.starts_with("/boot/") || path == "/boot" {
            return Err(FsError::ReadOnly);
        }
        if self.find_node(path.as_bytes()).is_none() {
            return self.write(path, data);
        }
        let node = self
            .find_node_mut(path.as_bytes())
            .ok_or(FsError::NotFound)?;
        if node.kind == NodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        let old_len = node.data_len as usize;
        let new_len = old_len
            .checked_add(data.len())
            .ok_or(FsError::FileTooLarge)?;
        if new_len > FILE_CAPACITY {
            return Err(FsError::FileTooLarge);
        }
        node.data[old_len..new_len].copy_from_slice(data);
        node.data_len = new_len as u16;
        Ok(())
    }

    /// Создаёт пустой файл, не обрезая существующий.
    pub fn touch(&mut self, path: &str) -> Result<(), FsError> {
        match self.stat(path) {
            Ok(stat) if stat.kind == FileKind::File && !stat.read_only => Ok(()),
            Ok(stat) if stat.kind == FileKind::Directory => Err(FsError::IsDirectory),
            Ok(_) => Err(FsError::ReadOnly),
            Err(FsError::NotFound) => self.write(path, &[]),
            Err(error) => Err(error),
        }
    }

    /// Удаляет RAM-file или пустой RAM-directory.
    pub fn remove(&mut self, path: &str) -> Result<(), FsError> {
        if path == "/" || path == "/boot" || path.starts_with("/boot/") {
            return Err(FsError::ReadOnly);
        }
        let index = self
            .find_node_index(path.as_bytes())
            .ok_or(FsError::NotFound)?;
        let kind = self.overlay().nodes[index].kind;
        if kind == NodeKind::Directory && self.has_children(path.as_bytes()) {
            return Err(FsError::NotEmpty);
        }
        self.overlay().nodes[index] = Node::EMPTY;
        Ok(())
    }

    fn overlay(&mut self) -> &mut OverlayStorage {
        // SAFETY: BootstrapFs — единственный владелец raw pointer; GUI loop
        // однопоточен и не сохраняет ссылки между вызовами.
        unsafe { &mut *self.overlay }
    }

    fn find_node_index(&mut self, path: &[u8]) -> Option<usize> {
        self.overlay()
            .nodes
            .iter()
            .position(|node| node.kind != NodeKind::Empty && node.path() == path)
    }

    fn find_node(&mut self, path: &[u8]) -> Option<&Node> {
        let index = self.find_node_index(path)?;
        Some(&self.overlay().nodes[index])
    }

    fn find_node_mut(&mut self, path: &[u8]) -> Option<&mut Node> {
        let index = self.find_node_index(path)?;
        Some(&mut self.overlay().nodes[index])
    }

    fn insert_node(&mut self, path: &[u8], kind: NodeKind, data: &[u8]) -> Result<(), FsError> {
        if path.len() >= PATH_CAPACITY || data.len() > FILE_CAPACITY {
            return Err(FsError::NoSpace);
        }
        if self.find_node_index(path).is_some() {
            return Err(FsError::AlreadyExists);
        }
        let node = self
            .overlay()
            .nodes
            .iter_mut()
            .find(|node| node.kind == NodeKind::Empty)
            .ok_or(FsError::NoSpace)?;
        node.kind = kind;
        node.path[..path.len()].copy_from_slice(path);
        node.path_len = path.len() as u8;
        node.data[..data.len()].copy_from_slice(data);
        node.data_len = data.len() as u16;
        Ok(())
    }

    fn ensure_new_writable_path(&mut self, path: &str) -> Result<(), FsError> {
        if path == "/" || path == "/boot" || path.starts_with("/boot/") {
            return Err(FsError::ReadOnly);
        }
        if self.stat(path).is_ok() {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_parent_directory(path)
    }

    fn ensure_parent_directory(&mut self, path: &str) -> Result<(), FsError> {
        let slash = path.rfind('/').ok_or(FsError::InvalidPath)?;
        let parent = if slash == 0 { "/" } else { &path[..slash] };
        if parent == "/boot" || parent.starts_with("/boot/") {
            return Err(FsError::ReadOnly);
        }
        match self.stat(parent)? {
            FileStat {
                kind: FileKind::Directory,
                ..
            } => Ok(()),
            _ => Err(FsError::NotDirectory),
        }
    }

    fn has_children(&mut self, path: &[u8]) -> bool {
        let prefix_len = path.len();
        self.overlay().nodes.iter().any(|node| {
            node.kind != NodeKind::Empty
                && node.path().len() > prefix_len
                && node.path().starts_with(path)
                && node.path().get(prefix_len) == Some(&b'/')
        })
    }

    fn list_overlay(&mut self, parent: &[u8], listing: &mut DirectoryListing) {
        for node in &self.overlay().nodes {
            if let Some(name) = immediate_child(parent, node.path()) {
                listing.push(
                    name,
                    if node.kind == NodeKind::Directory {
                        FileKind::Directory
                    } else {
                        FileKind::File
                    },
                    false,
                );
            }
        }
    }

    fn rifs(&self) -> Result<Rifs<'_>, FsError> {
        if self.initramfs.is_null() || self.initramfs_size < RIFS_HEADER_SIZE {
            return Err(FsError::CorruptImage);
        }
        // SAFETY: BootInfo гарантирует identity-mapped initramfs range;
        // загрузчик держит его в kernel reservation до выключения.
        let bytes = unsafe { slice::from_raw_parts(self.initramfs, self.initramfs_size) };
        Rifs::parse(bytes)
    }

    fn rifs_file(&self, path: &str) -> Result<&[u8], FsError> {
        let relative = path.strip_prefix("/boot/").ok_or(FsError::NotFound)?;
        self.rifs()?
            .entries()
            .find(|entry| entry.name == relative.as_bytes())
            .map(|entry| entry.data)
            .ok_or(FsError::NotFound)
    }

    fn rifs_stat(&self, path: &str) -> Result<Option<(FileKind, usize)>, FsError> {
        let Some(relative) = path.strip_prefix("/boot/") else {
            return Ok(None);
        };
        let mut implicit_directory = false;
        for entry in self.rifs()?.entries() {
            if entry.name == relative.as_bytes() {
                return Ok(Some((FileKind::File, entry.data.len())));
            }
            if entry.name.starts_with(relative.as_bytes())
                && entry.name.get(relative.len()) == Some(&b'/')
            {
                implicit_directory = true;
            }
        }
        Ok(implicit_directory.then_some((FileKind::Directory, 0)))
    }

    fn list_rifs(&self, parent: &str, listing: &mut DirectoryListing) -> Result<(), FsError> {
        if parent != "/boot" && !parent.starts_with("/boot/") {
            return Ok(());
        }
        let relative_parent = parent.strip_prefix("/boot").unwrap_or("");
        let relative_parent = relative_parent.strip_prefix('/').unwrap_or(relative_parent);
        for entry in self.rifs()?.entries() {
            let Some(child) = immediate_child(relative_parent.as_bytes(), entry.name) else {
                continue;
            };
            let consumed = if relative_parent.is_empty() {
                child.len()
            } else {
                relative_parent.len() + 1 + child.len()
            };
            let is_directory = entry.name.get(consumed) == Some(&b'/');
            listing.push(
                child,
                if is_directory {
                    FileKind::Directory
                } else {
                    FileKind::File
                },
                true,
            );
        }
        Ok(())
    }
}

/// Разобранная RIFS-таблица initramfs (весь образ заимствован из
/// identity-mapped области BootInfo, см. [`BootstrapFs::rifs`]).
struct Rifs<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> Rifs<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, FsError> {
        let magic = read_u32(bytes, 0).ok_or(FsError::CorruptImage)?;
        let version = read_u32(bytes, 4).ok_or(FsError::CorruptImage)?;
        let count = read_u32(bytes, 8).ok_or(FsError::CorruptImage)? as usize;
        let declared_size = read_u64(bytes, 16).ok_or(FsError::CorruptImage)? as usize;
        let table_end = RIFS_HEADER_SIZE
            .checked_add(
                count
                    .checked_mul(RIFS_ENTRY_SIZE)
                    .ok_or(FsError::CorruptImage)?,
            )
            .ok_or(FsError::CorruptImage)?;
        if magic != RIFS_MAGIC
            || version != RIFS_VERSION
            || declared_size != bytes.len()
            || table_end > bytes.len()
        {
            return Err(FsError::CorruptImage);
        }
        for index in 0..count {
            let base = RIFS_HEADER_SIZE + index * RIFS_ENTRY_SIZE;
            let name = bytes
                .get(base..base + RIFS_NAME_SIZE)
                .ok_or(FsError::CorruptImage)?;
            let name_len = name
                .iter()
                .position(|byte| *byte == 0)
                .ok_or(FsError::CorruptImage)?;
            if !valid_relative_path(&name[..name_len]) {
                return Err(FsError::CorruptImage);
            }
            let size = read_u64(bytes, base + 48).ok_or(FsError::CorruptImage)? as usize;
            let offset = read_u64(bytes, base + 56).ok_or(FsError::CorruptImage)? as usize;
            if !offset.is_multiple_of(4096)
                || offset
                    .checked_add(size)
                    .filter(|end| *end <= bytes.len())
                    .is_none()
            {
                return Err(FsError::CorruptImage);
            }
        }
        Ok(Self { bytes, count })
    }

    fn entries(&self) -> RifsEntries<'a> {
        RifsEntries {
            bytes: self.bytes,
            index: 0,
            count: self.count,
        }
    }
}

struct RifsEntry<'a> {
    name: &'a [u8],
    data: &'a [u8],
}

/// Итератор по записям RIFS-таблицы (парсинг повторяет валидацию
/// [`Rifs::parse`], но без ошибок: вызывается только после parse).
struct RifsEntries<'a> {
    bytes: &'a [u8],
    index: usize,
    count: usize,
}

impl<'a> Iterator for RifsEntries<'a> {
    type Item = RifsEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.count {
            return None;
        }
        let base = RIFS_HEADER_SIZE + self.index * RIFS_ENTRY_SIZE;
        self.index += 1;
        let name_bytes = self.bytes.get(base..base + RIFS_NAME_SIZE)?;
        let name_len = name_bytes.iter().position(|byte| *byte == 0)?;
        let size = read_u64(self.bytes, base + 48)? as usize;
        let offset = read_u64(self.bytes, base + 56)? as usize;
        let end = offset.checked_add(size)?;
        Some(RifsEntry {
            name: &name_bytes[..name_len],
            data: self.bytes.get(offset..end)?,
        })
    }
}

/// Разворачивает `input` относительно `cwd` в абсолютный путь без
/// `.`/`..` и кратных слешей. `..` выше корня просто игнорируется.
/// Запись идёт в `output` (с запасом под NUL), возвращается длина.
fn normalize_path(
    cwd: &str,
    input: &str,
    output: &mut [u8; PATH_CAPACITY],
) -> Result<usize, FsError> {
    if input.as_bytes().contains(&0) || cwd.as_bytes().contains(&0) {
        return Err(FsError::InvalidPath);
    }
    let mut raw = [0u8; PATH_CAPACITY * 2];
    let mut raw_len = 0usize;
    if input.starts_with('/') {
        append_bytes(&mut raw, &mut raw_len, input.as_bytes())?;
    } else {
        append_bytes(&mut raw, &mut raw_len, cwd.as_bytes())?;
        if !cwd.ends_with('/') {
            append_bytes(&mut raw, &mut raw_len, b"/")?;
        }
        append_bytes(&mut raw, &mut raw_len, input.as_bytes())?;
    }

    output.fill(0);
    output[0] = b'/';
    let mut out_len = 1usize;
    let mut component_starts = [0usize; MAX_COMPONENTS];
    let mut depth = 0usize;
    let mut cursor = 0usize;
    while cursor < raw_len {
        while cursor < raw_len && raw[cursor] == b'/' {
            cursor += 1;
        }
        let start = cursor;
        while cursor < raw_len && raw[cursor] != b'/' {
            cursor += 1;
        }
        let component = &raw[start..cursor];
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            if depth > 0 {
                depth -= 1;
                let component_start = component_starts[depth];
                out_len = if component_start == 1 {
                    1
                } else {
                    component_start - 1
                };
                output[out_len..].fill(0);
            }
            continue;
        }
        if depth == MAX_COMPONENTS {
            return Err(FsError::InvalidPath);
        }
        if out_len > 1 {
            if out_len >= PATH_CAPACITY - 1 {
                return Err(FsError::InvalidPath);
            }
            output[out_len] = b'/';
            out_len += 1;
        }
        component_starts[depth] = out_len;
        depth += 1;
        let end = out_len
            .checked_add(component.len())
            .filter(|end| *end < PATH_CAPACITY)
            .ok_or(FsError::InvalidPath)?;
        output[out_len..end].copy_from_slice(component);
        out_len = end;
    }
    Ok(out_len)
}

fn append_bytes<const N: usize>(
    target: &mut [u8; N],
    length: &mut usize,
    value: &[u8],
) -> Result<(), FsError> {
    let end = length
        .checked_add(value.len())
        .ok_or(FsError::InvalidPath)?;
    if end > N {
        return Err(FsError::InvalidPath);
    }
    target[*length..end].copy_from_slice(value);
    *length = end;
    Ok(())
}

fn immediate_child<'a>(parent: &[u8], candidate: &'a [u8]) -> Option<&'a [u8]> {
    let rest = if parent == b"/" || parent.is_empty() {
        candidate.strip_prefix(b"/").unwrap_or(candidate)
    } else {
        let rest = candidate.strip_prefix(parent)?;
        rest.strip_prefix(b"/")?
    };
    if rest.is_empty() {
        return None;
    }
    let length = rest
        .iter()
        .position(|byte| *byte == b'/')
        .unwrap_or(rest.len());
    Some(&rest[..length])
}

fn valid_relative_path(path: &[u8]) -> bool {
    !path.is_empty()
        && !path.starts_with(b"/")
        && str::from_utf8(path).is_ok()
        && path
            .split(|byte| *byte == b'/')
            .all(|component| !component.is_empty() && component != b"." && component != b"..")
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn as_str(bytes: &[u8]) -> &str {
    str::from_utf8(bytes).unwrap_or("?")
}

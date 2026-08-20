//! Пути и текущий каталог процесса RustOS.
//!
//! CWD намеренно является состоянием пользовательского runtime, а не ядра:
//! ядро не знает о строковых путях и файловых системах. Наследование между
//! процессами выполняется обычной переменной `PWD` в startup environment.

use crate::ffi::{OsStr, OsString};
use crate::marker::PhantomData;
use crate::path::{Component, Path, PathBuf};
use crate::sync::Mutex;
use crate::{fmt, io};

static CURRENT_DIRECTORY: Mutex<Option<PathBuf>> = Mutex::new(None);

fn normalize(base: &Path, path: &Path) -> io::Result<PathBuf> {
    let mut result = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    for component in path.components() {
        match component {
            Component::RootDir => result = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if result != Path::new("/") {
                    result.pop();
                }
            }
            Component::Normal(part) => result.push(part),
            Component::Prefix(_) => {
                return Err(io::const_error!(
                    io::ErrorKind::InvalidInput,
                    "RustOS path cannot contain a platform prefix",
                ));
            }
        }
    }
    if result.as_os_str().is_empty() {
        result.push("/");
    }
    Ok(result)
}

fn initial_directory() -> PathBuf {
    let candidate = crate::env::var_os("PWD")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    normalize(Path::new("/"), &candidate).unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn getcwd() -> io::Result<PathBuf> {
    let mut current = CURRENT_DIRECTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(current.get_or_insert_with(initial_directory).clone())
}

pub fn chdir(path: &Path) -> io::Result<()> {
    let absolute = normalize(&getcwd()?, path)?;
    if !crate::fs::metadata(&absolute)?.is_dir() {
        return Err(io::const_error!(
            io::ErrorKind::NotADirectory,
            "working directory is not a directory",
        ));
    }
    *CURRENT_DIRECTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(absolute.clone());
    // Command наследует полный environment snapshot, поэтому держим PWD в
    // согласованном состоянии с process-local CWD.
    unsafe { crate::env::set_var("PWD", absolute.as_os_str()) };
    Ok(())
}

/// Приводит путь к нормализованному абсолютному виду перед VFS RPC.
pub(crate) fn absolute(path: &Path) -> io::Result<PathBuf> {
    normalize(&getcwd()?, path)
}

pub struct SplitPaths<'a> {
    inner: crate::vec::IntoIter<PathBuf>,
    marker: PhantomData<&'a OsStr>,
}

pub fn split_paths(unparsed: &OsStr) -> SplitPaths<'_> {
    let parts = unparsed
        .to_string_lossy()
        .split(':')
        .map(PathBuf::from)
        .collect::<crate::vec::Vec<_>>();
    SplitPaths {
        inner: parts.into_iter(),
        marker: PhantomData,
    }
}

impl Iterator for SplitPaths<'_> {
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[derive(Debug)]
pub struct JoinPathsError;

pub fn join_paths<I, T>(paths: I) -> Result<OsString, JoinPathsError>
where
    I: Iterator<Item = T>,
    T: AsRef<OsStr>,
{
    let mut joined = crate::string::String::new();
    for (index, path) in paths.enumerate() {
        let path = path.as_ref().to_str().ok_or(JoinPathsError)?;
        if path.contains(':') {
            return Err(JoinPathsError);
        }
        if index != 0 {
            joined.push(':');
        }
        joined.push_str(path);
    }
    Ok(OsString::from(joined))
}

impl fmt::Display for JoinPathsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RustOS path segment contains ':' or is not UTF-8")
    }
}

impl crate::error::Error for JoinPathsError {}

pub fn current_exe() -> io::Result<PathBuf> {
    let executable = crate::env::args_os().next().ok_or_else(|| {
        io::const_error!(
            io::ErrorKind::NotFound,
            "process has no executable argument"
        )
    })?;
    absolute(Path::new(&executable))
}

pub fn temp_dir() -> PathBuf {
    crate::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn home_dir() -> Option<PathBuf> {
    crate::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

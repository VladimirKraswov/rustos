//! Process-local environment RustOS.
//!
//! Kernel только передаёт неизменяемый initial snapshot. Дальнейшие изменения
//! живут в памяти процесса, как и ожидают переносимые Rust-программы; никакой
//! глобальной «таблицы окружения всей ОС» и соответствующей гонки нет.

use crate::ffi::{OsStr, OsString};
use crate::sync::Mutex;
use crate::{fmt, io, str};

static ENVIRONMENT: Mutex<Option<crate::vec::Vec<(OsString, OsString)>>> = Mutex::new(None);

pub struct Env {
    inner: crate::vec::IntoIter<(OsString, OsString)>,
}

fn with_environment<T>(
    operation: impl FnOnce(&mut crate::vec::Vec<(OsString, OsString)>) -> T,
) -> T {
    let mut guard = ENVIRONMENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let variables = guard.get_or_insert_with(read_initial_environment);
    operation(variables)
}

fn read_initial_environment() -> crate::vec::Vec<(OsString, OsString)> {
    let Some((address, length, expected_count)) = crate::sys::pal::rustos_environment() else {
        return crate::vec::Vec::new();
    };
    if length == 0 {
        return crate::vec::Vec::new();
    }
    // SAFETY: PAL проверил диапазон внутри read-only startup mapping.
    let bytes = unsafe { core::slice::from_raw_parts(address, length) };
    let mut result = crate::vec::Vec::with_capacity(expected_count);
    let mut offset = 0usize;
    while offset < bytes.len() && result.len() < expected_count {
        let Some(length) = bytes[offset..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let item = &bytes[offset..offset + length];
        if let Some(separator) = item.iter().position(|byte| *byte == b'=') {
            if separator != 0 {
                if let (Ok(name), Ok(value)) = (
                    str::from_utf8(&item[..separator]),
                    str::from_utf8(&item[separator + 1..]),
                ) {
                    result.push((OsString::from(name), OsString::from(value)));
                }
            }
        }
        offset += length + 1;
    }
    result
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.inner.as_slice()).finish()
    }
}

impl Iterator for Env {
    type Item = (OsString, OsString);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

pub fn env() -> Env {
    let values = with_environment(|variables| variables.clone());
    Env {
        inner: values.into_iter(),
    }
}

pub fn getenv(name: &OsStr) -> Option<OsString> {
    with_environment(|variables| {
        variables
            .iter()
            .find(|(candidate, _)| candidate.as_os_str() == name)
            .map(|(_, value)| value.clone())
    })
}

pub unsafe fn setenv(name: &OsStr, value: &OsStr) -> io::Result<()> {
    validate_name(name)?;
    with_environment(|variables| {
        if let Some((_, current)) = variables
            .iter_mut()
            .find(|(candidate, _)| candidate.as_os_str() == name)
        {
            *current = value.to_os_string();
        } else {
            variables.push((name.to_os_string(), value.to_os_string()));
        }
    });
    Ok(())
}

pub unsafe fn unsetenv(name: &OsStr) -> io::Result<()> {
    validate_name(name)?;
    with_environment(|variables| {
        variables.retain(|(candidate, _)| candidate.as_os_str() != name);
    });
    Ok(())
}

fn validate_name(name: &OsStr) -> io::Result<()> {
    let Some(name) = name.to_str() else {
        return Err(io::const_error!(
            io::ErrorKind::InvalidInput,
            "environment name is not UTF-8"
        ));
    };
    if name.is_empty() || name.as_bytes().contains(&b'=') || name.as_bytes().contains(&0) {
        return Err(io::const_error!(
            io::ErrorKind::InvalidInput,
            "invalid environment name"
        ));
    }
    Ok(())
}

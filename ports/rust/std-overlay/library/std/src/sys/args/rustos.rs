//! Аргументы процесса из RustOS startup ABI.

use crate::ffi::OsString;
use crate::{fmt, str};

pub struct Args {
    inner: crate::vec::IntoIter<OsString>,
}

pub fn args() -> Args {
    let (count, vector) = crate::sys::pal::rustos_arguments();
    let mut values = crate::vec::Vec::with_capacity(count.max(0) as usize);
    if !vector.is_null() {
        for index in 0..count.max(0) as usize {
            // SAFETY: CRT построил argv из read-only ProcessStartInfo и
            // завершил массив null-указателем. Здесь действует защитный лимит
            // длины одной UTF-8 строки.
            let pointer = unsafe { vector.add(index).read() };
            if let Some(value) = unsafe { read_argument(pointer) } {
                values.push(value);
            } else {
                break;
            }
        }
    }
    Args {
        inner: values.into_iter(),
    }
}

unsafe fn read_argument(pointer: *const u8) -> Option<OsString> {
    if pointer.is_null() {
        return None;
    }
    let mut length = 0usize;
    while length < 64 * 1024 {
        // SAFETY: вызывающая сторона гарантирует C string; предел защищает от
        // бесконечного сканирования при нарушенном ABI.
        if unsafe { pointer.add(length).read() } == 0 {
            // SAFETY: диапазон до найденного NUL доступен в startup mapping.
            let bytes = unsafe { core::slice::from_raw_parts(pointer, length) };
            return str::from_utf8(bytes).ok().map(OsString::from);
        }
        length += 1;
    }
    None
}

impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.inner.as_slice()).finish()
    }
}

impl Iterator for Args {
    type Item = OsString;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl DoubleEndedIterator for Args {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back()
    }
}

impl ExactSizeIterator for Args {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

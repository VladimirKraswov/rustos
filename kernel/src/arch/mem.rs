//! C-совместимые mem-builtins (`memset`, `memcpy`, `memmove`, `memcmp`).
//!
//! Нужны, потому что kernel targets RustOS собираются через
//! `-Zbuild-std=core` без фич `mem`/`unmangled-names` у `compiler-builtins`,
//! а `core` (например `rust_begin_unwind`) ссылается на C-символы.
//! Ядро само предоставляет runtime — стандартная практика для ОС.
//!
//! Подписи используют `c_void`, чтобы совпадать с канонической C-ABI
//! (и не триггерить `suspicious_runtime_symbol_definitions`).
//!
//! Реализации простые и корректные (побайтовые); SIMD-ускорение выбирается
//! отдельным backend'ом после обнаружения SSE/AVX или NEON, а не в общем ABI.
//!
//! Важно: здесь нельзя вызывать `core::ptr::write_bytes`/`copy_nonoverlapping` —
//! они снижаются в те же самые C-символы и приведут к рекурсии.

use core::ffi::c_void;

/// `memset(s, c, n)`: заполнить `n` байт от `s` значением `c as u8`; вернуть `s`.
///
/// # Safety
///
/// `s` валиден для записи `n` байт (контракт C memset).
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut c_void, c: i32, n: usize) -> *mut c_void {
    // SAFETY: контракт C memset — вызывающий передаёт валидную область на n байт.
    unsafe {
        let p = s as *mut u8;
        let b = c as u8;
        let mut i = 0usize;
        while i < n {
            *p.add(i) = b;
            i += 1;
        }
    }
    s
}

/// `memcpy(dest, src, n)`: скопировать `n` байт из `src` в `dest`; вернуть `dest`.
///
/// Области не должны пересекаться (для пересечения — `memmove`).
///
/// # Safety
///
/// Оба указателя валидны для `n` байт; области не пересекаются (контракт C).
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    // SAFETY: контракт C memcpy — валидные непересекающиеся области.
    unsafe {
        let d = dest as *mut u8;
        let s = src as *const u8;
        let mut i = 0usize;
        while i < n {
            *d.add(i) = *s.add(i);
            i += 1;
        }
    }
    dest
}

/// `memmove(dest, src, n)`: скопировать `n` байт, допуская пересечение; вернуть `dest`.
///
/// # Safety
///
/// Оба указателя валидны для `n` байт (контракт C memmove).
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    if n == 0 {
        return dest;
    }
    // SAFETY: контракт C memmove — валидные области (пересечение допустимо).
    unsafe {
        let d = dest as *mut u8;
        let s = src as *const u8;
        let (du, su) = (d as usize, s as usize);
        if su < du {
            // Пересечение вперёд: копируем с конца, чтобы не затереть источник.
            let mut i = n;
            while i > 0 {
                i -= 1;
                *d.add(i) = *s.add(i);
            }
        } else {
            // Нет пересечения (или оно назад): копируем с начала.
            let mut i = 0usize;
            while i < n {
                *d.add(i) = *s.add(i);
                i += 1;
            }
        }
    }
    dest
}

/// `memcmp(s1, s2, n)`: сравнить `n` байт; вернуть <0 / 0 / >0.
///
/// # Safety
///
/// Оба указателя валидны для чтения `n` байт (контракт C).
#[no_mangle]
pub unsafe extern "C" fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> i32 {
    // SAFETY: контракт C memcmp — валидные области на n байт.
    let (a, b) = (s1 as *const u8, s2 as *const u8);
    let mut i = 0usize;
    unsafe {
        while i < n {
            let x = *a.add(i);
            let y = *b.add(i);
            if x != y {
                return x.cmp(&y) as i32;
            }
            i += 1;
        }
    }
    0
}

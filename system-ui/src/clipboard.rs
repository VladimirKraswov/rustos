//! Единая граница системного clipboard.
//!
//! Trait не раскрывает process pointers: production-реализация передаст bytes
//! через bounded shared memory capability, а headless/runtime tests используют
//! `LocalClipboard` с тем же контрактом.

/// Тип clipboard payload. Значения 0x8000_0000+ зарезервированы package custom
/// formats, согласованными через системный registry.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardFormat(pub u32);

impl ClipboardFormat {
    /// UTF-8 plain text. В RustOS это основной Unicode text format.
    pub const TEXT: Self = Self(1);
    /// RichText span stream будущей версии SystemUI.
    pub const RICH_TEXT: Self = Self(2);
    /// Системное изображение/encoded image resource.
    pub const IMAGE: Self = Self(3);
    /// Список RUNE/VFS paths, а не host pointers.
    pub const FILE_LIST: Self = Self(4);
    /// Первый ID для application-defined formats.
    pub const CUSTOM_BASE: u32 = 0x8000_0000;
}

/// Ошибка clipboard service/fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardError {
    /// Payload превышает bounded transport/storage.
    Capacity,
    /// Запрошенный format отсутствует.
    FormatUnavailable,
    /// UTF-8 text payload невалиден.
    InvalidText,
    /// Caller output buffer мал.
    OutputTooSmall,
    /// Сервис/capability временно недоступен.
    Unavailable,
}

/// Публичный clipboard API. Большой payload caller может разместить в shared
/// staging и реализовать эти операции поверх одного IPC descriptor.
pub trait Clipboard {
    /// Атомарно заменяет clipboard одним format payload.
    fn write(&mut self, format: ClipboardFormat, bytes: &[u8]) -> Result<(), ClipboardError>;
    /// Копирует payload; возвращает фактический размер.
    fn read(&mut self, format: ClipboardFormat, output: &mut [u8])
        -> Result<usize, ClipboardError>;
    /// Доступен ли format без чтения payload.
    fn contains(&self, format: ClipboardFormat) -> bool;
}

/// Bounded clipboard для headless tests, bootstrap и process-local fallback.
pub struct LocalClipboard<const N: usize> {
    bytes: [u8; N],
    len: usize,
    format: Option<ClipboardFormat>,
}

impl<const N: usize> LocalClipboard<N> {
    /// Пустой clipboard.
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
            format: None,
        }
    }

    /// Сбрасывает ownership текущего payload.
    pub fn clear(&mut self) {
        self.len = 0;
        self.format = None;
    }
}

impl<const N: usize> Clipboard for LocalClipboard<N> {
    fn write(&mut self, format: ClipboardFormat, bytes: &[u8]) -> Result<(), ClipboardError> {
        if bytes.len() > N {
            return Err(ClipboardError::Capacity);
        }
        if format == ClipboardFormat::TEXT && core::str::from_utf8(bytes).is_err() {
            return Err(ClipboardError::InvalidText);
        }
        self.bytes[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
        self.format = Some(format);
        Ok(())
    }

    fn read(
        &mut self,
        format: ClipboardFormat,
        output: &mut [u8],
    ) -> Result<usize, ClipboardError> {
        if self.format != Some(format) {
            return Err(ClipboardError::FormatUnavailable);
        }
        if output.len() < self.len {
            return Err(ClipboardError::OutputTooSmall);
        }
        output[..self.len].copy_from_slice(&self.bytes[..self.len]);
        Ok(self.len)
    }

    fn contains(&self, format: ClipboardFormat) -> bool {
        self.format == Some(format)
    }
}

impl<const N: usize> Default for LocalClipboard<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_text_round_trip_is_atomic() {
        let mut clipboard = LocalClipboard::<32>::new();
        clipboard
            .write(ClipboardFormat::TEXT, "Привет".as_bytes())
            .unwrap();
        assert!(clipboard.contains(ClipboardFormat::TEXT));
        assert_eq!(
            clipboard.write(ClipboardFormat::TEXT, &[0xff]),
            Err(ClipboardError::InvalidText)
        );
        let mut output = [0u8; 32];
        let len = clipboard.read(ClipboardFormat::TEXT, &mut output).unwrap();
        assert_eq!(core::str::from_utf8(&output[..len]).unwrap(), "Привет");
    }
}

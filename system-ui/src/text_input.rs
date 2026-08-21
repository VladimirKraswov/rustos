//! Однострочный bounded UTF-8 буфер текстового ввода.
//!
//! `TextInputBuffer<N>` — renderer-neutral модель однострочного поля: валидный
//! UTF-8 текст длиной не более `N` байт и курсор как byte offset на границе
//! extended grapheme cluster. Модель не использует heap, `alloc` и `unsafe`:
//! вместимость задаёт владелец через const generic, а каждая мутация сначала
//! проверяет лимит и не публикует частичное состояние при ошибке.
//!
//! Результат предназначен для будущих TextField, Terminal, inline rename
//! Проводника и редактора. Намеренно не реализованы selection, multiline,
//! clipboard, IME, shaping, undo и event handling — это забота контролов и
//! event routing.

use unicode_segmentation::UnicodeSegmentation;

/// Ошибки текстового буфера.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextInputError {
    /// Вставка превышает лимит `N` байтов.
    Capacity,
    /// Позиция не находится на границе grapheme cluster.
    InvalidPosition,
}

/// Однострочный буфер фиксированной ёмкости `N` байт UTF-8.
///
/// Invariant: `bytes[..len]` всегда валидный UTF-8, а `cursor` — byte offset
/// на границе extended grapheme cluster в `0..=len`. Все мутаторы сохраняют
/// invariant; при ошибке ни `bytes`, ни `cursor` не меняются.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextInputBuffer<const N: usize> {
    bytes: [u8; N],
    len: usize,
    cursor: usize,
}

impl<const N: usize> TextInputBuffer<N> {
    /// Создаёт пустой буфер с курсором в начале.
    pub const fn new() -> Self {
        Self {
            bytes: [0u8; N],
            len: 0,
            cursor: 0,
        }
    }

    /// Возвращает текущий текст как `&str`.
    ///
    /// Invariant буфера гарантирует, что `bytes[..len]` — валидный UTF-8:
    /// каждый мутатор копирует только валидные последовательности и не меняет
    /// `len` при ошибке. Поэтому `from_utf8` всегда возвращает `Ok`, а ветка
    /// `Err` недостижима. Мы не используем `unsafe`, а проверяем через
    /// `core::str::from_utf8`.
    pub fn as_str(&self) -> &str {
        match core::str::from_utf8(&self.bytes[..self.len]) {
            Ok(text) => text,
            Err(_) => unreachable!("invariant нарушен: bytes[..len] не валидный UTF-8"),
        }
    }

    /// Текущая длина текста в байтах.
    pub const fn len_bytes(&self) -> usize {
        self.len
    }

    /// Вместимость буфера в байтах.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Проверяет отсутствие текста.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Позиция курсора как byte offset на границе grapheme cluster.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Атомарно перемещает курсор на `offset` байт.
    ///
    /// Принимает только `0..=len` на границе extended grapheme cluster.
    /// Позиция за концом текста, внутри code point или между code points одной
    /// видимой grapheme отклоняется
    /// с `InvalidPosition`, не меняя ни `bytes`, ни `cursor`.
    pub fn set_cursor(&mut self, offset: usize) -> Result<(), TextInputError> {
        if offset > self.len {
            return Err(TextInputError::InvalidPosition);
        }
        let is_grapheme_boundary = offset == self.len
            || self
                .as_str()
                .grapheme_indices(true)
                .any(|(boundary, _)| boundary == offset);
        if !is_grapheme_boundary {
            return Err(TextInputError::InvalidPosition);
        }
        self.cursor = offset;
        Ok(())
    }

    /// Заменяет весь текст и ставит курсор в конец.
    ///
    /// Операция атомарна: при превышении лимита `N` состояние не меняется.
    pub fn set_text(&mut self, text: &str) -> Result<(), TextInputError> {
        if text.len() > N {
            return Err(TextInputError::Capacity);
        }
        self.bytes[..text.len()].copy_from_slice(text.as_bytes());
        self.len = text.len();
        self.cursor = self.len;
        Ok(())
    }

    /// Вставляет `text` на позицию курсора и сдвигает курсор вперёд.
    ///
    /// Операция атомарна: при превышении лимита `N` состояние не меняется.
    pub fn insert_str(&mut self, text: &str) -> Result<(), TextInputError> {
        let insert_len = text.len();
        if self.len.saturating_add(insert_len) > N {
            return Err(TextInputError::Capacity);
        }
        // Сдвигаем хвост вправо, оставляя место под вставку. `copy_within`
        // корректно обрабатывает пересечение диапазонов.
        self.bytes
            .copy_within(self.cursor..self.len, self.cursor + insert_len);
        // Записываем новый текст на позицию курсора.
        self.bytes[self.cursor..self.cursor + insert_len].copy_from_slice(text.as_bytes());
        self.len += insert_len;
        self.cursor += insert_len;
        Ok(())
    }

    /// Вставляет один символ на позицию курсора.
    pub fn insert_char(&mut self, ch: char) -> Result<(), TextInputError> {
        let mut buf = [0u8; 4];
        self.insert_str(ch.encode_utf8(&mut buf))
    }

    /// Удаляет видимую grapheme перед курсором. На позиции 0 — no-op.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.as_str()[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(boundary, _)| boundary);
        self.bytes.copy_within(self.cursor..self.len, start);
        self.len -= self.cursor - start;
        self.cursor = start;
    }

    /// Удаляет видимую grapheme после курсора. В конце — no-op.
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.len {
            return;
        }
        let width = self.as_str()[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
        let end = self.cursor + width;
        // Удаляем code point [cursor, end) и сдвигаем хвост влево.
        self.bytes.copy_within(end..self.len, self.cursor);
        self.len -= width;
    }

    /// Сдвигает курсор на одну видимую grapheme влево.
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.as_str()[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(boundary, _)| boundary);
    }

    /// Сдвигает курсор на одну видимую grapheme вправо.
    pub fn move_right(&mut self) {
        if self.cursor >= self.len {
            return;
        }
        let width = self.as_str()[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
        self.cursor = (self.cursor + width).min(self.len);
    }

    /// Перемещает курсор в начало текста.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Перемещает курсор в конец текста.
    pub fn move_end(&mut self) {
        self.cursor = self.len;
    }
}

impl<const N: usize> Default for TextInputBuffer<N> {
    /// Пустой буфер — то же состояние, что даёт `new()`.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_insert_backspace_delete_and_movement() {
        let mut buf = TextInputBuffer::<16>::new();
        assert!(buf.is_empty());
        assert_eq!(buf.as_str(), "");
        assert_eq!(buf.cursor(), 0);
        assert_eq!(buf.capacity(), 16);

        // Вставка ASCII-символов.
        buf.insert_str("hello").unwrap();
        assert_eq!(buf.as_str(), "hello");
        assert_eq!(buf.len_bytes(), 5);
        assert_eq!(buf.cursor(), 5);

        // Движение к началу и вправо.
        buf.move_home();
        assert_eq!(buf.cursor(), 0);
        buf.move_right();
        assert_eq!(buf.cursor(), 1);

        // Вставка в середине.
        buf.insert_str("J").unwrap();
        assert_eq!(buf.as_str(), "hJello");
        assert_eq!(buf.cursor(), 2);

        // Backspace удаляет символ перед курсором.
        buf.backspace();
        assert_eq!(buf.as_str(), "hello");
        assert_eq!(buf.cursor(), 1);

        // delete_forward удаляет символ после курсора ('e').
        buf.delete_forward();
        assert_eq!(buf.as_str(), "hllo");
        assert_eq!(buf.cursor(), 1);

        // Движение в конец и влево.
        buf.move_end();
        assert_eq!(buf.cursor(), 4);
        buf.move_left();
        assert_eq!(buf.cursor(), 3);

        // insert_char вставляет один символ.
        buf.insert_char('!').unwrap();
        assert_eq!(buf.as_str(), "hll!o");
        assert_eq!(buf.cursor(), 4);
    }

    #[test]
    fn cyrillic_moves_on_utf8_boundaries() {
        let mut buf = TextInputBuffer::<32>::new();
        // «Привет» — 6 кириллических символов по 2 байта = 12 байт.
        buf.insert_str("Привет").unwrap();
        assert_eq!(buf.len_bytes(), 12);
        assert_eq!(buf.cursor(), 12);

        // move_left от конца: 12 -> 10 -> 8 (по границам code point).
        buf.move_left();
        assert_eq!(buf.cursor(), 10);
        buf.move_left();
        assert_eq!(buf.cursor(), 8);

        // move_right: 8 -> 10 -> 12.
        buf.move_right();
        assert_eq!(buf.cursor(), 10);
        buf.move_right();
        assert_eq!(buf.cursor(), 12);

        // backspace удаляет целый code point (2 байта).
        buf.backspace();
        assert_eq!(buf.as_str(), "Приве");
        assert_eq!(buf.len_bytes(), 10);
        assert_eq!(buf.cursor(), 10);

        // delete_forward удаляет целый code point.
        buf.move_home();
        buf.delete_forward();
        assert_eq!(buf.as_str(), "риве");
        assert_eq!(buf.len_bytes(), 8);
        assert_eq!(buf.cursor(), 0);
    }

    #[test]
    fn cursor_and_delete_follow_visible_graphemes() {
        let mut buf = TextInputBuffer::<64>::new();
        let family = "👩‍👩‍👧‍👦";
        buf.set_text(family).unwrap();
        buf.move_left();
        assert_eq!(buf.cursor(), 0);
        buf.move_right();
        assert_eq!(buf.cursor(), family.len());
        buf.backspace();
        assert!(buf.is_empty());

        buf.set_text("e\u{301}!").unwrap();
        assert_eq!(buf.set_cursor(1), Err(TextInputError::InvalidPosition));
        buf.move_home();
        buf.delete_forward();
        assert_eq!(buf.as_str(), "!");
    }

    #[test]
    fn set_cursor_rejects_invalid_positions() {
        // ASCII: каждый байт — граница, невалидна только позиция за концом.
        let mut buf = TextInputBuffer::<16>::new();
        buf.insert_str("hello").unwrap();
        assert_eq!(buf.cursor(), 5);

        // Валидные позиции принимаются.
        buf.set_cursor(2).unwrap();
        assert_eq!(buf.cursor(), 2);
        buf.set_cursor(0).unwrap();
        assert_eq!(buf.cursor(), 0);
        buf.set_cursor(5).unwrap();
        assert_eq!(buf.cursor(), 5);

        // Позиция за концом отклоняется, cursor не меняется.
        assert_eq!(buf.set_cursor(6), Err(TextInputError::InvalidPosition));
        assert_eq!(buf.cursor(), 5);
        assert_eq!(buf.set_cursor(100), Err(TextInputError::InvalidPosition));
        assert_eq!(buf.cursor(), 5);

        // Кириллица: нечётные offsets — continuation bytes, отклоняются.
        let mut cyr = TextInputBuffer::<32>::new();
        cyr.insert_str("Привет").unwrap(); // 12 байт, cursor=12
        assert_eq!(cyr.cursor(), 12);

        // Валидные границы принимаются.
        cyr.set_cursor(10).unwrap(); // начало «т»
        assert_eq!(cyr.cursor(), 10);
        cyr.set_cursor(2).unwrap(); // начало «р»
        assert_eq!(cyr.cursor(), 2);
        cyr.set_cursor(0).unwrap();
        assert_eq!(cyr.cursor(), 0);
        cyr.set_cursor(12).unwrap(); // конец
        assert_eq!(cyr.cursor(), 12);

        // Continuation bytes отклоняются, cursor не меняется.
        cyr.set_cursor(10).unwrap();
        assert_eq!(cyr.set_cursor(11), Err(TextInputError::InvalidPosition));
        assert_eq!(cyr.cursor(), 10);
        assert_eq!(cyr.set_cursor(1), Err(TextInputError::InvalidPosition));
        assert_eq!(cyr.cursor(), 10);
        assert_eq!(cyr.set_cursor(3), Err(TextInputError::InvalidPosition));
        assert_eq!(cyr.cursor(), 10);
    }

    #[test]
    fn fills_capacity_exactly() {
        let mut buf = TextInputBuffer::<4>::new();
        // «аб» = 4 байта, заполняем ровно до capacity.
        buf.insert_str("аб").unwrap();
        assert_eq!(buf.len_bytes(), 4);
        assert_eq!(buf.as_str(), "аб");
        assert_eq!(buf.cursor(), 4);

        // Дальнейшая вставка не помещается.
        assert_eq!(buf.insert_str("в"), Err(TextInputError::Capacity));
        assert_eq!(buf.insert_char('г'), Err(TextInputError::Capacity));
        // Состояние не изменилось после отказа.
        assert_eq!(buf.as_str(), "аб");
        assert_eq!(buf.cursor(), 4);
    }

    #[test]
    fn set_text_rejects_without_partial_change() {
        let mut buf = TextInputBuffer::<4>::new();
        buf.insert_str("аб").unwrap();
        assert_eq!(buf.as_str(), "аб");
        assert_eq!(buf.cursor(), 4);

        // set_text с превышением capacity не меняет состояние.
        assert_eq!(buf.set_text("абвгд"), Err(TextInputError::Capacity));
        assert_eq!(buf.as_str(), "аб");
        assert_eq!(buf.cursor(), 4);

        // insert_str с превышением capacity не меняет состояние.
        assert_eq!(buf.insert_str("вг"), Err(TextInputError::Capacity));
        assert_eq!(buf.as_str(), "аб");
        assert_eq!(buf.cursor(), 4);

        // Успешный set_text заменяет текст и ставит курсор в конец.
        buf.set_text("я").unwrap();
        assert_eq!(buf.as_str(), "я");
        assert_eq!(buf.len_bytes(), 2);
        assert_eq!(buf.cursor(), 2);
    }

    #[test]
    fn zero_capacity_buffer() {
        let mut buf = TextInputBuffer::<0>::new();
        assert!(buf.is_empty());
        assert_eq!(buf.capacity(), 0);
        assert_eq!(buf.as_str(), "");
        assert_eq!(buf.cursor(), 0);

        // Любая непустая вставка не помещается.
        assert_eq!(buf.insert_str("a"), Err(TextInputError::Capacity));
        assert_eq!(buf.insert_char('a'), Err(TextInputError::Capacity));
        assert_eq!(buf.set_text("a"), Err(TextInputError::Capacity));
        // Пустая вставка допустима.
        buf.insert_str("").unwrap();
        buf.set_text("").unwrap();
        assert!(buf.is_empty());
        assert_eq!(buf.as_str(), "");

        // Операции над пустым буфером — no-op.
        buf.backspace();
        buf.delete_forward();
        buf.move_left();
        buf.move_right();
        buf.move_home();
        buf.move_end();
        assert_eq!(buf.cursor(), 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn default_equals_new() {
        // Default даёт то же пустое состояние, что new(), включая N = 0.
        let a: TextInputBuffer<8> = Default::default();
        let b: TextInputBuffer<8> = TextInputBuffer::new();
        assert_eq!(a, b);
        assert!(a.is_empty());
        assert_eq!(a.cursor(), 0);
        assert_eq!(a.capacity(), 8);

        let c: TextInputBuffer<0> = Default::default();
        let d: TextInputBuffer<0> = TextInputBuffer::new();
        assert_eq!(c, d);
        assert!(c.is_empty());
        assert_eq!(c.capacity(), 0);
    }
}

//! Однострочный bounded UTF-8 буфер текстового ввода.
//!
//! `TextInputBuffer<N>` — renderer-neutral модель однострочного поля: валидный
//! UTF-8 текст длиной не более `N` байт и курсор как byte offset на границе
//! extended grapheme cluster. Модель не использует heap, `alloc` и `unsafe`:
//! вместимость задаёт владелец через const generic, а каждая мутация сначала
//! проверяет лимит и не публикует частичное состояние при ошибке.
//!
//! Результат предназначен для будущих TextField, Terminal, inline rename
//! Проводника и редактора. Selection задаётся `anchor` и `cursor` как byte
//! offsets на границах extended grapheme cluster. Намеренно не реализованы
//! multiline, clipboard, IME, shaping, undo и event handling — это забота
//! контролов и event routing.

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
/// Invariant: `bytes[..len]` всегда валидный UTF-8, а `cursor` и `anchor` —
/// byte offsets на границах extended grapheme cluster в `0..=len`. Все
/// мутаторы сохраняют invariant; при ошибке ни `bytes`, ни `cursor`, ни
/// `anchor` не меняются.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextInputBuffer<const N: usize> {
    bytes: [u8; N],
    len: usize,
    cursor: usize,
    anchor: usize,
}

impl<const N: usize> TextInputBuffer<N> {
    /// Создаёт пустой буфер с курсором в начале.
    pub const fn new() -> Self {
        Self {
            bytes: [0u8; N],
            len: 0,
            cursor: 0,
            anchor: 0,
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

    /// Позиция anchor selection как byte offset на границе grapheme cluster.
    ///
    /// Вместе с `cursor()` задаёт направление выделения: `anchor` может
    /// находиться после `cursor`.
    pub const fn anchor(&self) -> usize {
        self.anchor
    }

    /// Атомарно перемещает курсор на `offset` байт и сворачивает selection
    /// в новую позицию.
    ///
    /// Принимает только `0..=len` на границе extended grapheme cluster.
    /// Позиция за концом текста, внутри code point или между code points одной
    /// видимой grapheme отклоняется
    /// с `InvalidPosition`, не меняя ни `bytes`, ни `cursor`, ни `anchor`.
    pub fn set_cursor(&mut self, offset: usize) -> Result<(), TextInputError> {
        if !self.is_grapheme_boundary(offset) {
            return Err(TextInputError::InvalidPosition);
        }
        self.cursor = offset;
        self.anchor = offset;
        Ok(())
    }

    /// Проверяет, что selection не свёрнут (`anchor != cursor`).
    pub const fn has_selection(&self) -> bool {
        self.anchor != self.cursor
    }

    /// Возвращает нормализованный полуоткрытый диапазон selection
    /// `(start, end)` в байтах, или `None`, если selection свёрнут.
    ///
    /// Диапазон всегда нормализован: направление ввода (`anchor` после
    /// `cursor`) на результат не влияет.
    pub const fn selection_range(&self) -> Option<(usize, usize)> {
        if self.anchor == self.cursor {
            return None;
        }
        if self.anchor < self.cursor {
            Some((self.anchor, self.cursor))
        } else {
            Some((self.cursor, self.anchor))
        }
    }

    /// Устанавливает selection: `anchor` — точка начала выделения,
    /// `cursor` — текущая позиция. Направление сохраняется: `anchor` может
    /// находиться после `cursor`.
    ///
    /// Оба offset должны находиться в `0..=len` на границе extended grapheme
    /// cluster. Ошибка `InvalidPosition` не меняет ни `bytes`, ни `cursor`,
    /// ни `anchor`.
    pub fn set_selection(&mut self, anchor: usize, cursor: usize) -> Result<(), TextInputError> {
        if !self.is_grapheme_boundary(anchor) || !self.is_grapheme_boundary(cursor) {
            return Err(TextInputError::InvalidPosition);
        }
        self.anchor = anchor;
        self.cursor = cursor;
        Ok(())
    }

    /// Сворачивает selection в позицию курсора.
    pub fn clear_selection(&mut self) {
        self.anchor = self.cursor;
    }

    /// Выделяет весь текст: `anchor` в начале, `cursor` в конце.
    ///
    /// Для пустого буфера selection остаётся свёрнутым.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.len;
    }

    /// Заменяет весь текст и ставит курсор в конец.
    ///
    /// Операция атомарна: при превышении лимита `N` состояние не меняется.
    /// Selection остаётся свёрнутым в конце текста.
    pub fn set_text(&mut self, text: &str) -> Result<(), TextInputError> {
        if text.len() > N {
            return Err(TextInputError::Capacity);
        }
        self.bytes[..text.len()].copy_from_slice(text.as_bytes());
        self.len = text.len();
        self.cursor = self.len;
        self.anchor = self.len;
        Ok(())
    }

    /// Вставляет `text` на позицию курсора и сдвигает курсор вперёд.
    ///
    /// Если selection не свёрнут, вставка заменяет выбранный диапазон.
    /// Успешная вставка оставляет selection свёрнутым в позиции вставки.
    /// Операция атомарна: при превышении лимита `N` состояние не меняется.
    pub fn insert_str(&mut self, text: &str) -> Result<(), TextInputError> {
        let (start, end) = self.selection_range().unwrap_or((self.cursor, self.cursor));
        let insert_len = text.len();
        let retained_len = self.len - (end - start);
        let Some(new_len) = retained_len.checked_add(insert_len) else {
            return Err(TextInputError::Capacity);
        };
        if new_len > N {
            return Err(TextInputError::Capacity);
        }
        // Сдвигаем хвост вправо, оставляя место под вставку. `copy_within`
        // корректно обрабатывает пересечение диапазонов.
        self.bytes.copy_within(end..self.len, start + insert_len);
        // Записываем новый текст на начало диапазона.
        self.bytes[start..start + insert_len].copy_from_slice(text.as_bytes());
        self.len = new_len;
        self.cursor = start + insert_len;
        self.anchor = self.cursor;
        Ok(())
    }

    /// Вставляет один символ на позицию курсора.
    pub fn insert_char(&mut self, ch: char) -> Result<(), TextInputError> {
        let mut buf = [0u8; 4];
        self.insert_str(ch.encode_utf8(&mut buf))
    }

    /// Удаляет видимую grapheme перед курсором. На позиции 0 — no-op.
    ///
    /// Если selection не свёрнут, удаляется весь выбранный диапазон.
    pub fn backspace(&mut self) {
        if let Some((start, end)) = self.selection_range() {
            self.delete_range(start, end);
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let start = self.as_str()[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(boundary, _)| boundary);
        self.delete_range(start, self.cursor);
    }

    /// Удаляет видимую grapheme после курсора. В конце — no-op.
    ///
    /// Если selection не свёрнут, удаляется весь выбранный диапазон.
    pub fn delete_forward(&mut self) {
        if let Some((start, end)) = self.selection_range() {
            self.delete_range(start, end);
            return;
        }
        if self.cursor >= self.len {
            return;
        }
        let width = self.as_str()[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
        self.delete_range(self.cursor, self.cursor + width);
    }

    /// Сдвигает курсор на одну видимую grapheme влево.
    ///
    /// Selection сворачивается в новую позицию: расширение выделения —
    /// задача event routing через `set_selection`.
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            self.anchor = self.cursor;
            return;
        }
        self.cursor = self.as_str()[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(boundary, _)| boundary);
        self.anchor = self.cursor;
    }

    /// Сдвигает курсор на одну видимую grapheme вправо.
    ///
    /// Selection сворачивается в новую позицию: расширение выделения —
    /// задача event routing через `set_selection`.
    pub fn move_right(&mut self) {
        if self.cursor >= self.len {
            self.anchor = self.cursor;
            return;
        }
        let width = self.as_str()[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
        self.cursor = (self.cursor + width).min(self.len);
        self.anchor = self.cursor;
    }

    /// Перемещает курсор в начало текста и сворачивает selection.
    pub fn move_home(&mut self) {
        self.cursor = 0;
        self.anchor = 0;
    }

    /// Перемещает курсор в конец текста и сворачивает selection.
    pub fn move_end(&mut self) {
        self.cursor = self.len;
        self.anchor = self.len;
    }

    /// Проверяет, что `offset` находится в `0..=len` на границе extended
    /// grapheme cluster.
    fn is_grapheme_boundary(&self, offset: usize) -> bool {
        offset <= self.len
            && (offset == self.len
                || self
                    .as_str()
                    .grapheme_indices(true)
                    .any(|(boundary, _)| boundary == offset))
    }

    /// Удаляет диапазон `[start, end)` и сдвигает хвост влево.
    ///
    /// `start` и `end` — границы grapheme cluster в `0..=len`. Cursor и
    /// anchor сворачиваются в `start`.
    fn delete_range(&mut self, start: usize, end: usize) {
        self.bytes.copy_within(end..self.len, start);
        self.len -= end - start;
        self.cursor = start;
        self.anchor = start;
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

    #[test]
    fn ascii_selection_both_directions() {
        let mut buf = TextInputBuffer::<16>::new();
        buf.insert_str("hello").unwrap();
        assert!(!buf.has_selection());
        assert_eq!(buf.selection_range(), None);

        // Слева направо: anchor=1, cursor=4 — выделяем "ell".
        buf.set_selection(1, 4).unwrap();
        assert!(buf.has_selection());
        assert_eq!(buf.selection_range(), Some((1, 4)));
        assert_eq!(buf.anchor(), 1);
        assert_eq!(buf.cursor(), 4);

        // Вставка заменяет selection: "hello" -> "hJo".
        buf.insert_str("J").unwrap();
        assert_eq!(buf.as_str(), "hJo");
        assert_eq!(buf.cursor(), 2);
        assert_eq!(buf.anchor(), 2);
        assert!(!buf.has_selection());

        // Справа налево: anchor=2, cursor=0 — направление сохраняется.
        buf.set_selection(2, 0).unwrap();
        assert!(buf.has_selection());
        assert_eq!(buf.selection_range(), Some((0, 2)));
        assert_eq!(buf.anchor(), 2);
        assert_eq!(buf.cursor(), 0);

        // Замена в обратном направлении: "hJo" -> "X" + "o" = "Xo".
        buf.insert_str("X").unwrap();
        assert_eq!(buf.as_str(), "Xo");
        assert_eq!(buf.cursor(), 1);
        assert!(!buf.has_selection());
    }

    #[test]
    fn replace_selected_cyrillic() {
        let mut buf = TextInputBuffer::<32>::new();
        // «Привет» — 12 байт: П(0..2) р(2..4) и(4..6) в(6..8) е(8..10) т(10..12).
        buf.insert_str("Привет").unwrap();
        // Выделяем «рив» (2..8) и заменяем на «ы»: «П» + «ы» + «ет».
        buf.set_selection(2, 8).unwrap();
        assert_eq!(buf.selection_range(), Some((2, 8)));
        buf.insert_str("ы").unwrap();
        assert_eq!(buf.as_str(), "Пыет");
        assert_eq!(buf.len_bytes(), 8);
        assert_eq!(buf.cursor(), 4);
        assert_eq!(buf.anchor(), 4);
        assert!(!buf.has_selection());
    }

    #[test]
    fn zwj_emoji_is_single_grapheme_for_selection() {
        let mut buf = TextInputBuffer::<64>::new();
        // 👩‍👩‍👧‍👦 — одна extended grapheme из 7 code points = 25 байт.
        let family = "👩‍👩‍👧‍👦";
        buf.insert_str("a").unwrap();
        buf.insert_str(family).unwrap();
        buf.insert_str("b").unwrap();
        assert_eq!(buf.len_bytes(), 27);

        // Границы grapheme: 0, 1, 26, 27.
        buf.set_selection(1, 26).unwrap();
        assert_eq!(buf.selection_range(), Some((1, 26)));
        buf.insert_str("x").unwrap();
        assert_eq!(buf.as_str(), "axb");
        assert_eq!(buf.cursor(), 2);
        assert!(!buf.has_selection());

        // Позиции внутри emoji отклоняются атомарно.
        buf.set_text("a").unwrap();
        buf.insert_str(family).unwrap();
        assert_eq!(buf.cursor(), 26);
        assert_eq!(buf.set_cursor(2), Err(TextInputError::InvalidPosition));
        assert_eq!(
            buf.set_selection(1, 2),
            Err(TextInputError::InvalidPosition)
        );
        assert_eq!(
            buf.set_selection(2, 26),
            Err(TextInputError::InvalidPosition)
        );
        assert_eq!(
            buf.set_selection(26, 27),
            Err(TextInputError::InvalidPosition)
        );
        // Состояние не изменилось: cursor=26, selection свёрнут.
        assert_eq!(buf.cursor(), 26);
        assert_eq!(buf.anchor(), 26);
        assert!(!buf.has_selection());
        assert_eq!(buf.as_str(), "a👩‍👩‍👧‍👦");
    }

    #[test]
    fn set_selection_rejects_invalid_positions_atomically() {
        let mut buf = TextInputBuffer::<32>::new();
        buf.insert_str("Привет").unwrap(); // 12 байт, cursor=12
        buf.set_selection(0, 12).unwrap();
        assert_eq!(buf.selection_range(), Some((0, 12)));

        // Continuation byte в anchor: отклоняется, selection не меняется.
        assert_eq!(
            buf.set_selection(1, 12),
            Err(TextInputError::InvalidPosition)
        );
        assert_eq!(buf.selection_range(), Some((0, 12)));
        assert_eq!(buf.cursor(), 12);
        assert_eq!(buf.anchor(), 0);

        // Continuation byte в cursor: отклоняется, selection не меняется.
        assert_eq!(
            buf.set_selection(0, 11),
            Err(TextInputError::InvalidPosition)
        );
        assert_eq!(buf.selection_range(), Some((0, 12)));

        // Позиция за концом: отклоняется.
        assert_eq!(
            buf.set_selection(0, 13),
            Err(TextInputError::InvalidPosition)
        );
        assert_eq!(buf.selection_range(), Some((0, 12)));

        // Валидная установка после ошибок.
        buf.set_selection(2, 8).unwrap();
        assert_eq!(buf.selection_range(), Some((2, 8)));
        assert_eq!(buf.anchor(), 2);
        assert_eq!(buf.cursor(), 8);
    }

    #[test]
    fn capacity_on_replace_keeps_state_atomic() {
        let mut buf = TextInputBuffer::<8>::new();
        buf.insert_str("абв").unwrap(); // 6 байт, cursor=6
        buf.select_all();
        assert_eq!(buf.selection_range(), Some((0, 6)));

        // Замена всего текста на 10 байт не помещается в N=8.
        assert_eq!(buf.insert_str("абвгд"), Err(TextInputError::Capacity));
        assert_eq!(buf.as_str(), "абв");
        assert_eq!(buf.len_bytes(), 6);
        assert_eq!(buf.cursor(), 6);
        assert_eq!(buf.anchor(), 0);
        assert_eq!(buf.selection_range(), Some((0, 6)));

        // Замена части selection: 6 - 4 + 8 = 10 > 8.
        buf.set_selection(2, 6).unwrap();
        assert_eq!(buf.insert_str("абвг"), Err(TextInputError::Capacity));
        assert_eq!(buf.as_str(), "абв");
        assert_eq!(buf.cursor(), 6);
        assert_eq!(buf.selection_range(), Some((2, 6)));

        // Успешная замена укладывается в лимит: 6 - 4 + 2 = 4.
        // «абв» без «бв» плюс «г» = «аг».
        buf.insert_str("г").unwrap();
        assert_eq!(buf.as_str(), "аг");
        assert_eq!(buf.cursor(), 4);
        assert_eq!(buf.anchor(), 4);
        assert!(!buf.has_selection());
    }

    #[test]
    fn backspace_and_delete_remove_selection() {
        let mut buf = TextInputBuffer::<16>::new();
        buf.insert_str("hello world").unwrap(); // 11 байт, cursor=11

        // Backspace удаляет выделение " world" (5..11).
        buf.set_selection(5, 11).unwrap();
        buf.backspace();
        assert_eq!(buf.as_str(), "hello");
        assert_eq!(buf.cursor(), 5);
        assert_eq!(buf.anchor(), 5);
        assert!(!buf.has_selection());

        // Delete удаляет выделение "ell" (1..4): "hello" -> "ho".
        buf.set_selection(1, 4).unwrap();
        buf.delete_forward();
        assert_eq!(buf.as_str(), "ho");
        assert_eq!(buf.cursor(), 1);
        assert!(!buf.has_selection());

        // Без selection поведение не меняется: backspace удаляет grapheme.
        buf.move_end();
        buf.backspace();
        assert_eq!(buf.as_str(), "h");
        assert_eq!(buf.cursor(), 1);
        assert!(!buf.has_selection());
    }

    #[test]
    fn select_all_clear_and_collapse_rules() {
        let mut buf = TextInputBuffer::<16>::new();

        // Пустой буфер: select_all не создаёт selection.
        buf.select_all();
        assert!(!buf.has_selection());
        assert_eq!(buf.selection_range(), None);
        assert_eq!(buf.cursor(), 0);
        assert_eq!(buf.anchor(), 0);

        buf.insert_str("абв").unwrap(); // 6 байт, cursor=6
        buf.move_home();
        buf.select_all();
        assert!(buf.has_selection());
        assert_eq!(buf.selection_range(), Some((0, 6)));
        assert_eq!(buf.anchor(), 0);
        assert_eq!(buf.cursor(), 6);

        // clear_selection сворачивает в позицию курсора.
        buf.clear_selection();
        assert!(!buf.has_selection());
        assert_eq!(buf.selection_range(), None);
        assert_eq!(buf.cursor(), 6);
        assert_eq!(buf.anchor(), 6);

        // set_cursor сворачивает selection в новую позицию.
        buf.select_all();
        buf.set_cursor(2).unwrap();
        assert!(!buf.has_selection());
        assert_eq!(buf.cursor(), 2);
        assert_eq!(buf.anchor(), 2);

        // Стрелка на границе не двигает cursor, но снимает selection.
        buf.set_selection(6, 0).unwrap();
        buf.move_left();
        assert_eq!(buf.cursor(), 0);
        assert!(!buf.has_selection());
        buf.set_selection(0, 6).unwrap();
        buf.move_right();
        assert_eq!(buf.cursor(), 6);
        assert!(!buf.has_selection());

        // set_text оставляет selection свёрнутым в конце.
        buf.select_all();
        buf.set_text("я").unwrap();
        assert!(!buf.has_selection());
        assert_eq!(buf.cursor(), 2);
        assert_eq!(buf.anchor(), 2);

        // set_selection с равными offset сворачивает selection.
        buf.set_selection(2, 2).unwrap();
        assert!(!buf.has_selection());
        assert_eq!(buf.selection_range(), None);
    }

    #[test]
    fn empty_insert_replaces_selection() {
        let mut buf = TextInputBuffer::<16>::new();
        buf.insert_str("hello").unwrap();
        buf.set_selection(1, 4).unwrap();
        // Пустая вставка заменяет selection на ничего: "hello" -> "ho".
        buf.insert_str("").unwrap();
        assert_eq!(buf.as_str(), "ho");
        assert_eq!(buf.cursor(), 1);
        assert_eq!(buf.anchor(), 1);
        assert!(!buf.has_selection());
    }
}

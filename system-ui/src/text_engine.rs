//! Renderer-neutral text engine для больших редактируемых документов.
//!
//! Документ использует bounded piece table: исходный и добавленный текст не
//! перемещаются при каждой вставке, меняется только небольшой массив pieces.
//! Такая реализация подходит раннему `no_std` runtime. Будущий ring-3 `uid`
//! сможет заменить фиксированные массивы growable arena, сохранив публичные
//! позиции, selection и controller semantics.

use crate::{Clipboard, ClipboardError, ClipboardFormat, ScrollConfig, ScrollState};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

/// Ошибки документа/editor controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextError {
    /// Не хватает byte storage исходного или add buffer.
    Capacity,
    /// Не хватает pieces после split/replace.
    PieceCapacity,
    /// Диапазон выходит за документ или режет UTF-8 code point.
    InvalidRange,
    /// Выходной буфер меньше запрошенного диапазона.
    OutputTooSmall,
}

/// Полуоткрытый byte range `[start, end)` на границах UTF-8 code point.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextRange {
    /// Первый byte.
    pub start: u32,
    /// Byte после последнего.
    pub end: u32,
}

impl TextRange {
    /// Нормализованный range двух позиций.
    pub const fn between(a: u32, b: u32) -> Self {
        Self {
            start: if a < b { a } else { b },
            end: if a < b { b } else { a },
        }
    }

    /// Пустой caret range.
    pub const fn caret(position: u32) -> Self {
        Self {
            start: position,
            end: position,
        }
    }

    /// Длина в байтах.
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Пуст ли диапазон.
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Anchor/active selection. Направление хранится для Shift-навигации.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextSelection {
    /// Неподвижный конец selection.
    pub anchor: u32,
    /// Caret/активный конец.
    pub active: u32,
}

impl TextSelection {
    /// Collapsed selection.
    pub const fn caret(position: u32) -> Self {
        Self {
            anchor: position,
            active: position,
        }
    }

    /// Нормализованный byte range.
    pub const fn range(self) -> TextRange {
        TextRange::between(self.anchor, self.active)
    }

    /// Selection не содержит выбранного текста.
    pub const fn is_collapsed(self) -> bool {
        self.anchor == self.active
    }
}

/// Position → line/column conversion. Byte/scalar/grapheme columns различны.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextLocation {
    /// Номер строки с нуля.
    pub line: u32,
    /// Byte offset от начала строки.
    pub byte_column: u32,
    /// Unicode scalar column.
    pub scalar_column: u32,
    /// Пользовательски воспринимаемая grapheme column.
    pub grapheme_column: u32,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PieceSource {
    Original = 0,
    Add = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Piece {
    source: PieceSource,
    start: u32,
    len: u32,
}

impl Piece {
    const EMPTY: Self = Self {
        source: PieceSource::Original,
        start: 0,
        len: 0,
    };
}

/// Piece-table документ с независимыми лимитами bytes и fragments.
///
/// `B` ограничивает исходный текст и суммарные bytes, добавленные после его
/// открытия. `P` ограничивает fragmentation; отказ не публикует partial edit.
pub struct TextDocument<const B: usize, const P: usize> {
    original: [u8; B],
    original_len: u32,
    add: [u8; B],
    add_len: u32,
    pieces: [Piece; P],
    piece_len: usize,
    len: u32,
    revision: u64,
}

impl<const B: usize, const P: usize> TextDocument<B, P> {
    /// Пустой документ.
    pub const fn new() -> Self {
        Self {
            original: [0; B],
            original_len: 0,
            add: [0; B],
            add_len: 0,
            pieces: [Piece::EMPTY; P],
            piece_len: 0,
            len: 0,
            revision: 0,
        }
    }

    /// Текущая длина UTF-8 текста в байтах.
    pub const fn len_bytes(&self) -> u32 {
        self.len
    }

    /// Документ пуст.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Monotonic revision меняется ровно один раз на committed edit.
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Число metadata pieces; не равно числу строк/символов.
    pub const fn piece_count(&self) -> usize {
        self.piece_len
    }

    /// Открывает/заменяет документ. Полное копирование происходит только при
    /// загрузке нового документа, а не при последующих edits.
    pub fn set_text(&mut self, text: &str) -> Result<(), TextError> {
        if text.len() > B || text.len() > u32::MAX as usize {
            return Err(TextError::Capacity);
        }
        if !text.is_empty() && P == 0 {
            return Err(TextError::PieceCapacity);
        }
        self.original[..text.len()].copy_from_slice(text.as_bytes());
        self.original_len = text.len() as u32;
        self.add_len = 0;
        self.pieces = [Piece::EMPTY; P];
        self.piece_len = usize::from(!text.is_empty());
        if !text.is_empty() {
            self.pieces[0] = Piece {
                source: PieceSource::Original,
                start: 0,
                len: text.len() as u32,
            };
        }
        self.len = text.len() as u32;
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Вставляет текст, не перемещая существующие document bytes.
    pub fn insert(&mut self, position: u32, text: &str) -> Result<(), TextError> {
        self.replace(TextRange::caret(position), text)
    }

    /// Удаляет диапазон без копирования оставшейся части документа.
    pub fn delete(&mut self, range: TextRange) -> Result<(), TextError> {
        self.replace(range, "")
    }

    /// Атомарная замена диапазона.
    pub fn replace(&mut self, range: TextRange, text: &str) -> Result<(), TextError> {
        self.validate_range(range)?;
        let add_start = usize::try_from(self.add_len).map_err(|_| TextError::Capacity)?;
        let add_end = add_start
            .checked_add(text.len())
            .ok_or(TextError::Capacity)?;
        if add_end > B || add_end > u32::MAX as usize {
            return Err(TextError::Capacity);
        }
        let inserted = Piece {
            source: PieceSource::Add,
            start: self.add_len,
            len: text.len() as u32,
        };
        let mut next = [Piece::EMPTY; P];
        let mut next_len = 0usize;
        let mut document_start = 0u32;
        let mut inserted_once = false;
        for piece in self.pieces.iter().copied().take(self.piece_len) {
            let document_end = document_start.saturating_add(piece.len);
            if document_end <= range.start {
                push_piece(&mut next, &mut next_len, piece)?;
            } else if document_start >= range.end {
                if !inserted_once {
                    push_piece(&mut next, &mut next_len, inserted)?;
                    inserted_once = true;
                }
                push_piece(&mut next, &mut next_len, piece)?;
            } else {
                if document_start < range.start {
                    push_piece(
                        &mut next,
                        &mut next_len,
                        Piece {
                            source: piece.source,
                            start: piece.start,
                            len: range.start - document_start,
                        },
                    )?;
                }
                if !inserted_once {
                    push_piece(&mut next, &mut next_len, inserted)?;
                    inserted_once = true;
                }
                if document_end > range.end {
                    let removed_prefix = range.end.saturating_sub(document_start);
                    push_piece(
                        &mut next,
                        &mut next_len,
                        Piece {
                            source: piece.source,
                            start: piece.start.saturating_add(removed_prefix),
                            len: document_end - range.end,
                        },
                    )?;
                }
            }
            document_start = document_end;
        }
        if !inserted_once {
            push_piece(&mut next, &mut next_len, inserted)?;
        }

        // Metadata capacity полностью проверена. Только теперь публикуем bytes
        // и pieces, сохраняя атомарность при любом предыдущем отказе.
        self.add[add_start..add_end].copy_from_slice(text.as_bytes());
        self.add_len = add_end as u32;
        self.pieces = next;
        self.piece_len = next_len;
        self.len = self
            .len
            .saturating_sub(range.len())
            .saturating_add(text.len() as u32);
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Копирует byte range в caller-owned/shared buffer.
    pub fn copy_range(&self, range: TextRange, output: &mut [u8]) -> Result<usize, TextError> {
        self.validate_range(range)?;
        if output.len() < range.len() as usize {
            return Err(TextError::OutputTooSmall);
        }
        let mut written = 0usize;
        self.for_each_slice(range, |slice| {
            output[written..written + slice.len()].copy_from_slice(slice);
            written += slice.len();
        })?;
        Ok(written)
    }

    /// Вызывает callback для contiguous fragments диапазона без сборки
    /// временного `String`.
    pub fn for_each_slice<F>(&self, range: TextRange, mut callback: F) -> Result<(), TextError>
    where
        F: FnMut(&[u8]),
    {
        self.validate_range(range)?;
        let mut document_start = 0u32;
        for piece in self.pieces.iter().copied().take(self.piece_len) {
            let document_end = document_start.saturating_add(piece.len);
            let start = document_start.max(range.start);
            let end = document_end.min(range.end);
            if start < end {
                let local_start = piece.start.saturating_add(start - document_start) as usize;
                let local_end = local_start + (end - start) as usize;
                callback(&self.source(piece.source)[local_start..local_end]);
            }
            document_start = document_end;
            if document_start >= range.end {
                break;
            }
        }
        Ok(())
    }

    /// Число строк; пустой документ содержит одну пустую строку.
    pub fn line_count(&self) -> u32 {
        let mut lines = 1u32;
        for index in 0..self.len {
            if self.byte_at(index) == Some(b'\n') {
                lines = lines.saturating_add(1);
            }
        }
        lines
    }

    /// Byte offset начала строки.
    pub fn line_start(&self, line: u32) -> Option<u32> {
        if line == 0 {
            return Some(0);
        }
        let mut current = 0u32;
        for index in 0..self.len {
            if self.byte_at(index) == Some(b'\n') {
                current = current.saturating_add(1);
                if current == line {
                    return Some(index.saturating_add(1));
                }
            }
        }
        None
    }

    /// Byte offset конца строки без `\n` и опционального `\r`.
    pub fn line_end(&self, line: u32) -> Option<u32> {
        let start = self.line_start(line)?;
        let mut end = start;
        while end < self.len && self.byte_at(end) != Some(b'\n') {
            end += 1;
        }
        if end > start && self.byte_at(end - 1) == Some(b'\r') {
            end -= 1;
        }
        Some(end)
    }

    /// Преобразует byte position в line и три вида column.
    pub fn location(&self, position: u32) -> Result<TextLocation, TextError> {
        if !self.is_boundary(position) {
            return Err(TextError::InvalidRange);
        }
        let mut line = 0u32;
        let mut line_start = 0u32;
        for index in 0..position {
            if self.byte_at(index) == Some(b'\n') {
                line += 1;
                line_start = index + 1;
            }
        }
        let mut scalar_column = 0u32;
        let mut cursor = line_start;
        while cursor < position {
            let (_, width) = self.decode_at(cursor).ok_or(TextError::InvalidRange)?;
            cursor += width;
            scalar_column += 1;
        }
        let mut grapheme_column = 0u32;
        cursor = line_start;
        while cursor < position {
            cursor = self.next_grapheme(cursor).ok_or(TextError::InvalidRange)?;
            grapheme_column += 1;
        }
        Ok(TextLocation {
            line,
            byte_column: position - line_start,
            scalar_column,
            grapheme_column,
        })
    }

    /// Следующая extended grapheme boundary по Unicode UAX #29.
    /// Cursor читает piece table по частям и не требует склеивать
    /// документ во временный contiguous buffer.
    pub fn next_grapheme(&self, position: u32) -> Option<u32> {
        if position == self.len {
            return Some(self.len);
        }
        if position > self.len || !self.is_boundary(position) {
            return None;
        }
        let mut cursor = GraphemeCursor::new(position as usize, self.len as usize, true);
        let mut piece_index = self.piece_at_or_after(position)?.0;
        loop {
            let (chunk, chunk_start) = self.piece_chunk(piece_index)?;
            match cursor.next_boundary(chunk, chunk_start) {
                Ok(Some(boundary)) => return u32::try_from(boundary).ok(),
                Ok(None) => return Some(self.len),
                Err(GraphemeIncomplete::NextChunk) => {
                    piece_index = piece_index.checked_add(1)?;
                }
                Err(GraphemeIncomplete::PreContext(context_end)) => {
                    let (context, context_start) = self.context_ending_at(context_end)?;
                    cursor.provide_context(context, context_start);
                }
                Err(GraphemeIncomplete::PrevChunk) => return None,
                Err(GraphemeIncomplete::InvalidOffset) => return None,
            }
        }
    }

    /// Предыдущая extended grapheme boundary по Unicode UAX #29.
    pub fn previous_grapheme(&self, position: u32) -> Option<u32> {
        if position == 0 {
            return Some(0);
        }
        if position > self.len || !self.is_boundary(position) {
            return None;
        }
        let mut cursor = GraphemeCursor::new(position as usize, self.len as usize, true);
        let mut piece_index = self.piece_at_or_before(position)?.0;
        loop {
            let (chunk, chunk_start) = self.piece_chunk(piece_index)?;
            match cursor.prev_boundary(chunk, chunk_start) {
                Ok(Some(boundary)) => return u32::try_from(boundary).ok(),
                Ok(None) => return Some(0),
                Err(GraphemeIncomplete::PrevChunk) => {
                    piece_index = piece_index.checked_sub(1)?;
                }
                Err(GraphemeIncomplete::PreContext(context_end)) => {
                    let (context, context_start) = self.context_ending_at(context_end)?;
                    cursor.provide_context(context, context_start);
                }
                Err(GraphemeIncomplete::NextChunk) => return None,
                Err(GraphemeIncomplete::InvalidOffset) => return None,
            }
        }
    }

    fn validate_range(&self, range: TextRange) -> Result<(), TextError> {
        if range.start > range.end
            || range.end > self.len
            || !self.is_boundary(range.start)
            || !self.is_boundary(range.end)
        {
            return Err(TextError::InvalidRange);
        }
        Ok(())
    }

    fn is_boundary(&self, position: u32) -> bool {
        position <= self.len
            && (position == self.len
                || self
                    .byte_at(position)
                    .is_some_and(|byte| byte & 0xc0 != 0x80))
    }

    fn byte_at(&self, position: u32) -> Option<u8> {
        if position >= self.len {
            return None;
        }
        let mut document_start = 0u32;
        for piece in self.pieces.iter().copied().take(self.piece_len) {
            let document_end = document_start.saturating_add(piece.len);
            if position < document_end {
                let source_index = piece.start.saturating_add(position - document_start) as usize;
                return self.source(piece.source).get(source_index).copied();
            }
            document_start = document_end;
        }
        None
    }

    fn decode_at(&self, position: u32) -> Option<(char, u32)> {
        let lead = self.byte_at(position)?;
        let width = utf8_width(lead) as u32;
        if position.saturating_add(width) > self.len {
            return None;
        }
        let mut bytes = [0u8; 4];
        for index in 0..width {
            bytes[index as usize] = self.byte_at(position + index)?;
        }
        let text = core::str::from_utf8(&bytes[..width as usize]).ok()?;
        Some((text.chars().next()?, width))
    }

    fn piece_at_or_after(&self, position: u32) -> Option<(usize, u32)> {
        let mut document_start = 0u32;
        for (index, piece) in self.pieces.iter().copied().take(self.piece_len).enumerate() {
            let document_end = document_start.checked_add(piece.len)?;
            if position >= document_start && position < document_end {
                return Some((index, document_start));
            }
            document_start = document_end;
        }
        None
    }

    fn piece_at_or_before(&self, position: u32) -> Option<(usize, u32)> {
        let mut document_start = 0u32;
        for (index, piece) in self.pieces.iter().copied().take(self.piece_len).enumerate() {
            let document_end = document_start.checked_add(piece.len)?;
            if position > document_start && position <= document_end {
                return Some((index, document_start));
            }
            document_start = document_end;
        }
        None
    }

    fn piece_chunk(&self, index: usize) -> Option<(&str, usize)> {
        let piece = *self.pieces.get(index)?;
        if index >= self.piece_len {
            return None;
        }
        let document_start = self
            .pieces
            .iter()
            .take(index)
            .try_fold(0usize, |offset, piece| {
                offset.checked_add(piece.len as usize)
            })?;
        let source_start = piece.start as usize;
        let source_end = source_start.checked_add(piece.len as usize)?;
        let chunk =
            core::str::from_utf8(self.source(piece.source).get(source_start..source_end)?).ok()?;
        Some((chunk, document_start))
    }

    /// Возвращает непустой chunk, заканчивающийся ровно в
    /// `document_end`. Это контракт `GraphemeCursor::provide_context`.
    fn context_ending_at(&self, document_end: usize) -> Option<(&str, usize)> {
        let mut piece_start = 0usize;
        for piece in self.pieces.iter().copied().take(self.piece_len) {
            let piece_end = piece_start.checked_add(piece.len as usize)?;
            if document_end > piece_start && document_end <= piece_end {
                let local_end = document_end - piece_start;
                let source_start = piece.start as usize;
                let source_end = source_start.checked_add(local_end)?;
                let context =
                    core::str::from_utf8(self.source(piece.source).get(source_start..source_end)?)
                        .ok()?;
                return Some((context, piece_start));
            }
            piece_start = piece_end;
        }
        None
    }

    fn source(&self, source: PieceSource) -> &[u8] {
        match source {
            PieceSource::Original => &self.original[..self.original_len as usize],
            PieceSource::Add => &self.add[..self.add_len as usize],
        }
    }
}

impl<const B: usize, const P: usize> Default for TextDocument<B, P> {
    fn default() -> Self {
        Self::new()
    }
}

fn push_piece<const P: usize>(
    output: &mut [Piece; P],
    len: &mut usize,
    piece: Piece,
) -> Result<(), TextError> {
    if piece.len == 0 {
        return Ok(());
    }
    if *len != 0 {
        let previous = &mut output[*len - 1];
        if previous.source == piece.source
            && previous.start.saturating_add(previous.len) == piece.start
        {
            previous.len = previous.len.saturating_add(piece.len);
            return Ok(());
        }
    }
    if *len == P {
        return Err(TextError::PieceCapacity);
    }
    output[*len] = piece;
    *len += 1;
    Ok(())
}

const fn utf8_width(lead: u8) -> u8 {
    if lead < 0x80 {
        1
    } else if lead & 0xe0 == 0xc0 {
        2
    } else if lead & 0xf0 == 0xe0 {
        3
    } else {
        4
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditKind {
    Insert = 0,
    Delete = 1,
    Replace = 2,
}

/// Общая command model редактора. Menu, shortcut, toolbar и command palette
/// передают один и тот же enum controller'у.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextCommand {
    /// Отменить последнюю transaction.
    Undo = 1,
    /// Повторить отменённую transaction.
    Redo = 2,
    /// Скопировать selection и удалить её.
    Cut = 3,
    /// Скопировать selection.
    Copy = 4,
    /// Вставить Unicode text clipboard.
    Paste = 5,
    /// Удалить selection/следующую grapheme.
    Delete = 6,
    /// Выбрать документ целиком.
    SelectAll = 7,
    /// Открыть application find UI.
    Find = 8,
    /// Открыть application replace UI.
    Replace = 9,
}

/// Ошибка исполнения общей edit command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextCommandError {
    /// Ошибка документа/history.
    Text(TextError),
    /// Ошибка системного clipboard.
    Clipboard(ClipboardError),
    /// Command требует UI (Find/Replace dialog), которого нет в controller.
    RequiresApplicationUi,
}

impl From<TextError> for TextCommandError {
    fn from(value: TextError) -> Self {
        Self::Text(value)
    }
}

impl From<ClipboardError> for TextCommandError {
    fn from(value: ClipboardError) -> Self {
        Self::Clipboard(value)
    }
}

#[derive(Clone, Copy)]
struct EditRecord<const E: usize> {
    position: u32,
    removed: [u8; E],
    removed_len: u32,
    inserted: [u8; E],
    inserted_len: u32,
    before: TextSelection,
    after: TextSelection,
    timestamp_ms: u64,
    kind: EditKind,
}

impl<const E: usize> EditRecord<E> {
    const EMPTY: Self = Self {
        position: 0,
        removed: [0; E],
        removed_len: 0,
        inserted: [0; E],
        inserted_len: 0,
        before: TextSelection::caret(0),
        after: TextSelection::caret(0),
        timestamp_ms: 0,
        kind: EditKind::Insert,
    };

    fn removed_text(&self) -> &str {
        core::str::from_utf8(&self.removed[..self.removed_len as usize])
            .expect("record создаётся только из валидного TextDocument")
    }

    fn inserted_text(&self) -> &str {
        core::str::from_utf8(&self.inserted[..self.inserted_len as usize])
            .expect("record получает только валидный &str")
    }
}

/// TextEditor controller объединяет document, selection, scrolling и bounded
/// undo history. Renderer читает только visible line range и fragments.
pub struct TextEditorController<const B: usize, const P: usize, const H: usize, const E: usize> {
    document: TextDocument<B, P>,
    selection: TextSelection,
    scroll: ScrollState,
    history: [EditRecord<E>; H],
    history_len: usize,
    history_cursor: usize,
    line_height: u16,
    overscan_lines: u16,
}

impl<const B: usize, const P: usize, const H: usize, const E: usize>
    TextEditorController<B, P, H, E>
{
    /// Пустой editor с vertical/horizontal scroll model.
    pub const fn new() -> Self {
        Self {
            document: TextDocument::new(),
            selection: TextSelection::caret(0),
            scroll: ScrollState::new(ScrollConfig::BOTH),
            history: [EditRecord::EMPTY; H],
            history_len: 0,
            history_cursor: 0,
            line_height: 20,
            overscan_lines: 2,
        }
    }

    /// Read-only документ.
    pub const fn document(&self) -> &TextDocument<B, P> {
        &self.document
    }

    /// Текущая selection/caret.
    pub const fn selection(&self) -> TextSelection {
        self.selection
    }

    /// Scroll state TextEditor.
    pub const fn scroll(&self) -> ScrollState {
        self.scroll
    }

    /// Загружает новый документ и разрывает undo history.
    pub fn set_text(&mut self, text: &str) -> Result<(), TextError> {
        self.document.set_text(text)?;
        self.selection = TextSelection::caret(self.document.len_bytes());
        self.history_len = 0;
        self.history_cursor = 0;
        self.update_extents();
        Ok(())
    }

    /// Обновляет viewport; layout/renderer используют logical pixels.
    pub fn set_viewport(&mut self, width: u32, height: u32) {
        self.scroll.horizontal.set_extents(width, u64::from(width));
        self.scroll.vertical.set_extents(
            height,
            u64::from(self.document.line_count()) * u64::from(self.line_height),
        );
        self.ensure_caret_visible();
    }

    /// Диапазон строк для shaping/raster: viewport + bounded overscan.
    pub fn visible_lines(&self) -> TextRange {
        let line_count = self.document.line_count();
        let first = (self.scroll.vertical.offset() / u64::from(self.line_height.max(1)))
            .min(u64::from(line_count)) as u32;
        let visible = self
            .scroll
            .vertical
            .viewport_size()
            .div_ceil(u32::from(self.line_height.max(1)))
            .saturating_add(1);
        TextRange {
            start: first.saturating_sub(u32::from(self.overscan_lines)),
            end: first
                .saturating_add(visible)
                .saturating_add(u32::from(self.overscan_lines))
                .min(line_count),
        }
    }

    /// Устанавливает selection после проверки границ.
    pub fn set_selection(&mut self, selection: TextSelection) -> Result<(), TextError> {
        self.document.validate_range(selection.range())?;
        self.selection = selection;
        self.ensure_caret_visible();
        Ok(())
    }

    /// Ввод/paste заменяет selection и группируется по времени/позиции.
    pub fn insert_text(&mut self, text: &str, now_ms: u64) -> Result<(), TextError> {
        self.commit_edit(self.selection.range(), text, now_ms, EditKind::Insert)
    }

    /// Backspace удаляет selection или предыдущую grapheme cluster.
    pub fn backspace(&mut self, now_ms: u64) -> Result<bool, TextError> {
        let range = if self.selection.is_collapsed() {
            let active = self.selection.active;
            let previous = self.document.previous_grapheme(active).unwrap_or(active);
            TextRange::between(previous, active)
        } else {
            self.selection.range()
        };
        if range.is_empty() {
            return Ok(false);
        }
        self.commit_edit(range, "", now_ms, EditKind::Delete)?;
        Ok(true)
    }

    /// Delete удаляет selection или следующую grapheme cluster.
    pub fn delete_forward(&mut self, now_ms: u64) -> Result<bool, TextError> {
        let range = if self.selection.is_collapsed() {
            let active = self.selection.active;
            let next = self.document.next_grapheme(active).unwrap_or(active);
            TextRange::between(active, next)
        } else {
            self.selection.range()
        };
        if range.is_empty() {
            return Ok(false);
        }
        self.commit_edit(range, "", now_ms, EditKind::Delete)?;
        Ok(true)
    }

    /// Grapheme navigation; `extend` соответствует Shift.
    pub fn move_left(&mut self, extend: bool) {
        let next = if !extend && !self.selection.is_collapsed() {
            self.selection.range().start
        } else {
            self.document
                .previous_grapheme(self.selection.active)
                .unwrap_or(self.selection.active)
        };
        self.move_caret(next, extend);
    }

    /// Grapheme navigation вправо.
    pub fn move_right(&mut self, extend: bool) {
        let next = if !extend && !self.selection.is_collapsed() {
            self.selection.range().end
        } else {
            self.document
                .next_grapheme(self.selection.active)
                .unwrap_or(self.selection.active)
        };
        self.move_caret(next, extend);
    }

    /// Home/End текущей строки.
    pub fn move_line_boundary(&mut self, end: bool, extend: bool) {
        let Ok(location) = self.document.location(self.selection.active) else {
            return;
        };
        let next = if end {
            self.document.line_end(location.line)
        } else {
            self.document.line_start(location.line)
        };
        if let Some(next) = next {
            self.move_caret(next, extend);
        }
    }

    /// Ctrl+Home/Ctrl+End.
    pub fn move_document_boundary(&mut self, end: bool, extend: bool) {
        self.move_caret(if end { self.document.len_bytes() } else { 0 }, extend);
    }

    /// Выбирает весь документ в O(1).
    pub fn select_all(&mut self) {
        self.selection = TextSelection {
            anchor: 0,
            active: self.document.len_bytes(),
        };
        self.ensure_caret_visible();
    }

    /// Копирует selection в caller-owned clipboard/shared-memory staging.
    pub fn copy_selection(&self, output: &mut [u8]) -> Result<usize, TextError> {
        self.document.copy_range(self.selection.range(), output)
    }

    /// Cut сначала атомарно копирует, затем удаляет selection.
    pub fn cut_selection(&mut self, output: &mut [u8], now_ms: u64) -> Result<usize, TextError> {
        let range = self.selection.range();
        let bytes = self.document.copy_range(range, output)?;
        if !range.is_empty() {
            self.commit_edit(range, "", now_ms, EditKind::Delete)?;
        }
        Ok(bytes)
    }

    /// Исполняет общую edit command через системный Clipboard contract.
    /// `scratch` является caller-owned shared-memory staging; controller не
    /// делает скрытую allocation и не передаёт pointer другому процессу.
    pub fn execute_command<C: Clipboard>(
        &mut self,
        command: TextCommand,
        clipboard: &mut C,
        scratch: &mut [u8],
        now_ms: u64,
    ) -> Result<bool, TextCommandError> {
        match command {
            TextCommand::Undo => self.undo().map_err(Into::into),
            TextCommand::Redo => self.redo().map_err(Into::into),
            TextCommand::Copy => {
                let bytes = self.copy_selection(scratch)?;
                clipboard.write(ClipboardFormat::TEXT, &scratch[..bytes])?;
                Ok(true)
            }
            TextCommand::Cut => {
                let bytes = self.copy_selection(scratch)?;
                clipboard.write(ClipboardFormat::TEXT, &scratch[..bytes])?;
                if !self.selection.is_collapsed() {
                    self.commit_edit(self.selection.range(), "", now_ms, EditKind::Delete)?;
                }
                Ok(true)
            }
            TextCommand::Paste => {
                let bytes = clipboard.read(ClipboardFormat::TEXT, scratch)?;
                let text = core::str::from_utf8(&scratch[..bytes])
                    .map_err(|_| ClipboardError::InvalidText)?;
                self.insert_text(text, now_ms)?;
                Ok(true)
            }
            TextCommand::Delete => self.delete_forward(now_ms).map_err(Into::into),
            TextCommand::SelectAll => {
                self.select_all();
                Ok(true)
            }
            TextCommand::Find | TextCommand::Replace => {
                Err(TextCommandError::RequiresApplicationUi)
            }
        }
    }

    /// Undo одной transaction/group.
    pub fn undo(&mut self) -> Result<bool, TextError> {
        if self.history_cursor == 0 {
            return Ok(false);
        }
        let record = self.history[self.history_cursor - 1];
        self.document.replace(
            TextRange {
                start: record.position,
                end: record.position.saturating_add(record.inserted_len),
            },
            record.removed_text(),
        )?;
        self.selection = record.before;
        self.history_cursor -= 1;
        self.update_extents();
        self.ensure_caret_visible();
        Ok(true)
    }

    /// Redo одной transaction/group.
    pub fn redo(&mut self) -> Result<bool, TextError> {
        if self.history_cursor >= self.history_len {
            return Ok(false);
        }
        let record = self.history[self.history_cursor];
        self.document.replace(
            TextRange {
                start: record.position,
                end: record.position.saturating_add(record.removed_len),
            },
            record.inserted_text(),
        )?;
        self.selection = record.after;
        self.history_cursor += 1;
        self.update_extents();
        self.ensure_caret_visible();
        Ok(true)
    }

    fn commit_edit(
        &mut self,
        range: TextRange,
        text: &str,
        now_ms: u64,
        kind: EditKind,
    ) -> Result<(), TextError> {
        self.document.validate_range(range)?;
        let removed_len = range.len() as usize;
        if removed_len > E || text.len() > E {
            return Err(TextError::Capacity);
        }
        let before = self.selection;
        let after_position = range.start.saturating_add(text.len() as u32);
        let after = TextSelection::caret(after_position);
        let mut record = EditRecord::EMPTY;
        record.position = range.start;
        record.removed_len = range.len();
        record.inserted_len = text.len() as u32;
        record.before = before;
        record.after = after;
        record.timestamp_ms = now_ms;
        record.kind = if !range.is_empty() && !text.is_empty() {
            EditKind::Replace
        } else {
            kind
        };
        self.document
            .copy_range(range, &mut record.removed[..removed_len])?;
        record.inserted[..text.len()].copy_from_slice(text.as_bytes());
        self.document.replace(range, text)?;
        self.selection = after;
        self.push_history(record);
        self.update_extents();
        self.ensure_caret_visible();
        Ok(())
    }

    fn push_history(&mut self, record: EditRecord<E>) {
        self.history_len = self.history_cursor;
        if record.kind == EditKind::Insert && record.removed_len == 0 && self.history_cursor != 0 {
            let previous = &mut self.history[self.history_cursor - 1];
            let adjacent = previous.kind == EditKind::Insert
                && previous.removed_len == 0
                && previous.position.saturating_add(previous.inserted_len) == record.position
                && record.timestamp_ms.saturating_sub(previous.timestamp_ms) <= 1_000;
            let combined = previous.inserted_len as usize + record.inserted_len as usize;
            if adjacent && combined <= E {
                previous.inserted[previous.inserted_len as usize..combined]
                    .copy_from_slice(&record.inserted[..record.inserted_len as usize]);
                previous.inserted_len = combined as u32;
                previous.after = record.after;
                previous.timestamp_ms = record.timestamp_ms;
                self.history_len = self.history_cursor;
                return;
            }
        }
        if H == 0 {
            return;
        }
        if self.history_cursor == H {
            self.history.copy_within(1..H, 0);
            self.history_cursor -= 1;
            self.history_len = self.history_len.saturating_sub(1);
        }
        self.history[self.history_cursor] = record;
        self.history_cursor += 1;
        self.history_len = self.history_cursor;
    }

    fn move_caret(&mut self, position: u32, extend: bool) {
        self.selection = if extend {
            TextSelection {
                anchor: self.selection.anchor,
                active: position,
            }
        } else {
            TextSelection::caret(position)
        };
        self.ensure_caret_visible();
    }

    fn update_extents(&mut self) {
        let height = u64::from(self.document.line_count()) * u64::from(self.line_height);
        self.scroll
            .vertical
            .set_extents(self.scroll.vertical.viewport_size(), height);
    }

    fn ensure_caret_visible(&mut self) {
        let Ok(location) = self.document.location(self.selection.active) else {
            return;
        };
        let start = u64::from(location.line) * u64::from(self.line_height);
        self.scroll
            .vertical
            .ensure_visible(start, start.saturating_add(u64::from(self.line_height)));
    }
}

impl<const B: usize, const P: usize, const H: usize, const E: usize> Default
    for TextEditorController<B, P, H, E>
{
    fn default() -> Self {
        Self::new()
    }
}

/// Concept-level IME events отделены от raw keys. Полноценный IME service
/// сможет присылать их без изменения TextEditor API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositionEvent<'a> {
    /// Начало composition в текущей selection.
    Start,
    /// Временный pre-edit текст, ещё не попадающий в undo history.
    Update(&'a str),
    /// Commit итоговой строки.
    Commit(&'a str),
    /// Отмена pre-edit.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalClipboard;

    type Document = TextDocument<4096, 64>;
    type Editor = TextEditorController<4096, 96, 16, 128>;

    fn text<const B: usize, const P: usize>(document: &TextDocument<B, P>) -> [u8; 128] {
        let mut output = [0u8; 128];
        let len = document
            .copy_range(TextRange::between(0, document.len_bytes()), &mut output)
            .unwrap();
        output[len..].fill(0);
        output
    }

    #[test]
    fn piece_table_edits_without_copying_original_document() {
        let mut document = Document::new();
        document.set_text("alpha\nbeta\ngamma").unwrap();
        document.insert(6, "big ").unwrap();
        document.delete(TextRange::between(0, 6)).unwrap();
        let bytes = text(&document);
        assert_eq!(
            core::str::from_utf8(&bytes[..14]).unwrap(),
            "big beta\ngamma"
        );
        assert!(document.piece_count() <= 4);
        assert_eq!(document.line_count(), 2);
    }

    #[test]
    fn failed_piece_capacity_does_not_publish_partial_edit() {
        let mut document = TextDocument::<64, 1>::new();
        document.set_text("abcd").unwrap();
        let revision = document.revision();
        assert_eq!(document.insert(2, "X"), Err(TextError::PieceCapacity));
        assert_eq!(document.revision(), revision);
        let mut output = [0u8; 4];
        document
            .copy_range(TextRange::between(0, 4), &mut output)
            .unwrap();
        assert_eq!(&output, b"abcd");
    }

    #[test]
    fn cyrillic_and_combining_sequence_use_distinct_offsets() {
        let mut document = Document::new();
        document.set_text("Привет e\u{301}!").unwrap();
        let e = "Привет ".len() as u32;
        assert_eq!(document.next_grapheme(e), Some(e + 3));
        assert_eq!(document.previous_grapheme(e + 3), Some(e));
        let location = document.location(e + 3).unwrap();
        assert_eq!(location.scalar_column, 9);
        assert_eq!(location.grapheme_column, 8);
        assert_eq!(document.insert(1, "x"), Err(TextError::InvalidRange));
    }

    #[test]
    fn zwj_emoji_moves_as_one_grapheme() {
        let mut document = Document::new();
        let family = "👩‍👩‍👧‍👦";
        document.set_text(family).unwrap();
        assert_eq!(document.next_grapheme(0), Some(family.len() as u32));
        assert_eq!(document.previous_grapheme(family.len() as u32), Some(0));
    }

    #[test]
    fn uax29_handles_indic_conjuncts_and_piece_boundaries() {
        let mut indic = Document::new();
        indic.set_text("हिन्दी").unwrap();
        assert_eq!(indic.next_grapheme(0), Some("हि".len() as u32));

        let mut split = Document::new();
        split.set_text("👩👩").unwrap();
        split.insert("👩".len() as u32, "\u{200d}").unwrap();
        let joined = "👩‍👩";
        assert!(split.piece_count() >= 3);
        assert_eq!(split.next_grapheme(0), Some(joined.len() as u32));
        assert_eq!(split.previous_grapheme(joined.len() as u32), Some(0));
    }

    #[test]
    fn editor_selection_clipboard_and_grouped_undo_redo() {
        let mut editor = Editor::new();
        editor.set_text("").unwrap();
        editor.insert_text("h", 100).unwrap();
        editor.insert_text("e", 200).unwrap();
        editor.insert_text("llo", 300).unwrap();
        assert!(editor.undo().unwrap());
        assert!(editor.document().is_empty());
        assert!(editor.redo().unwrap());
        let mut output = [0u8; 16];
        editor.select_all();
        let copied = editor.copy_selection(&mut output).unwrap();
        assert_eq!(&output[..copied], b"hello");
        let cut = editor.cut_selection(&mut output, 2_000).unwrap();
        assert_eq!(cut, 5);
        assert!(editor.document().is_empty());
        assert!(editor.undo().unwrap());
        assert_eq!(editor.document().len_bytes(), 5);
    }

    #[test]
    fn visible_line_range_is_bounded_by_viewport() {
        let mut editor = Editor::new();
        editor.set_text("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n").unwrap();
        editor.set_viewport(800, 60);
        editor.scroll.vertical.scroll_to(100);
        let visible = editor.visible_lines();
        assert!(visible.len() <= 8);
        assert!(visible.start > 0);
    }

    #[test]
    fn menu_and_shortcut_commands_share_clipboard_path() {
        let mut source = Editor::new();
        source.set_text("данные").unwrap();
        source.select_all();
        let mut clipboard = LocalClipboard::<64>::new();
        let mut scratch = [0u8; 64];
        source
            .execute_command(TextCommand::Copy, &mut clipboard, &mut scratch, 100)
            .unwrap();
        let mut target = Editor::new();
        target
            .execute_command(TextCommand::Paste, &mut clipboard, &mut scratch, 200)
            .unwrap();
        let mut output = [0u8; 32];
        let bytes = target
            .document()
            .copy_range(
                TextRange::between(0, target.document().len_bytes()),
                &mut output,
            )
            .unwrap();
        assert_eq!(core::str::from_utf8(&output[..bytes]).unwrap(), "данные");
    }
}

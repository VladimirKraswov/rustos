# UTF-8 контракт RustOS

## Цель

RustOS использует UTF-8 как единственную текстовую кодировку на
публичных границах ядра, сервисов, RUNE DLL, VFS и SystemUI. Это
убирает неявную «локальную кодировку» и даёт один контракт Rust, C ABI
и shared-memory IPC.

## Обязательные правила

1. Публичная текстовая строка — это валидная UTF-8 byte sequence и
   явная длина. Указатель без length не является строкой ABI.
2. Получатель проверяет shared/user bytes ровно на trust boundary до
   публикации state. Ошибка возвращается caller; сервис и система не
   падают.
3. После проверки код работает с `&str` либо типом, invariant которого
   гарантирует UTF-8. `from_utf8_unchecked` допустим только рядом с явной
   доказанной проверкой.
4. Невалидные bytes не заменяются молча на `?` или U+FFFD в state.
   Replacement character разрешён только как явная presentation-policy для
   лога/терминала, где исходные bytes не изменяются.
5. RustOS не нормализует Unicode автоматически. NFC/NFD меняет bytes и
   применяется только явно для поиска, locale policy или сравнения. VFS не
   должна незаметно переименовывать файл.

## Системные границы

| Граница | Контракт | Отказ |
|---|---|---|
| Process path, argv, environment | UTF-8; argv/env также имеют точную NUL-таблицу | `INVALID_ARGUMENT` |
| VFS path и directory name | UTF-8, без встроенного NUL; разделитель `/` | `INVALID_ARGUMENT` / `InvalidPath` |
| RUNE DLL/resource names | UTF-8 по длине либо NUL-terminated там, где это зафиксировано ABI | loader error |
| `ClipboardFormat::TEXT` | валидный UTF-8 payload | `InvalidText`, старое значение сохраняется |
| Text input/IME commit | UTF-8 string; composition публикуется атомарно | input event rejected |
| Program stdout/stderr | byte stream; terminal декодирует UTF-8 с явной replacement-policy | bytes не теряются |

Filesystem содержимое и IPC binary payload не являются текстом и не
проверяются как UTF-8.

## Позиции и редактирование

Одно слово «символ» недостаточно. API явно различает:

- byte offset — file/IPC/piece-table storage;
- Unicode scalar value — `char`, decoding и character properties;
- extended grapheme cluster — caret, selection movement, Backspace/Delete;
- shaped glyph cluster — будущий font shaping и hit testing;
- display cell — monospace terminal width, а не число bytes.

SystemUI использует extended grapheme segmentation Unicode UAX #29. Piece-table
cursor получает контекст соседних pieces, поэтому emoji ZWJ sequence или
Indic conjunct не ломаются на границе edit fragment.

## Что ещё не обещано

UTF-8 не равен полному Unicode UI. Отдельными milestones остаются:

- font fallback для символов вне текущих Latin/Cyrillic bitmap fonts;
- OpenType shaping, ligatures и complex scripts;
- bidirectional layout для Arabic/Hebrew;
- line breaking, locale-aware word boundaries и collation;
- отдельный IME service и composition transport в ring 3.

Эти ограничения не меняю encoding contract и могут добавляться без
миграции файлов и API.

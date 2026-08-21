# OpenCode 001: bounded UTF-8 buffer для текстового ввода

## Цель

Добавить в `rustos-system-ui` независимую от renderer модель однострочного
текстового ввода. Результат используется будущими TextField, Terminal, inline
rename Проводника и редактором, но в этой задаче ни одно приложение не
интегрируется.

## Разрешено менять

- новый `system-ui/src/text_input.rs`;
- `system-ui/src/lib.rs` только для подключения module и public re-export.

Все остальные файлы запрещены. В частности, не меняй `Cargo.toml`, ABI, kernel,
desktop/Terminal, renderer, event routing, документацию и существующие dirty
files.

## Перед работой

Полностью прочитай `AGENTS.md`, `system-ui/AGENTS.md`, начало
`docs/SYSTEM_UI.md`, `system-ui/src/lib.rs` и стиль тестов в
`system-ui/src/collections.rs`.

## Контракт

Реализуй public `TextInputBuffer<const N: usize>` без heap, `alloc`, `unsafe` и
зависимостей. `N` — вместимость UTF-8 в байтах. Модель хранит валидный UTF-8 и
cursor как byte offset на границе code point.

Нужны операции:

- `new`, `Default`, `as_str`, `len_bytes`, `capacity`, `is_empty`, `cursor`;
- атомарный `set_cursor(byte_offset)`, принимающий только `0..=len` на границе
  UTF-8 code point;
- атомарный `set_text` и `insert_str`;
- `insert_char`, `backspace`, `delete_forward`;
- `move_left`, `move_right`, `move_home`, `move_end`.

Ошибки capacity и невалидной позиции должны быть typed и не менять ни bytes,
ни cursor. Внутренний invariant позволяет получать `&str` без `unsafe`:
используй проверяемый `core::str::from_utf8` и поясни невозможную ветку. Не
реализуй selection, multiline, clipboard, IME, shaping, undo и event handling.

Public API и нетривиальные invariants документируй на русском. Имена кода —
английские.

## Тесты приёмки

Unit tests в новом module обязаны покрыть:

- ASCII insert/backspace/delete и движения;
- кириллицу с корректным движением по границам UTF-8;
- отказ `set_cursor` для позиции за концом и continuation byte без изменения
  cursor;
- заполнение capacity в точности;
- отказ `set_text`/`insert_str` без частичного изменения state;
- `N = 0` и операции над пустым buffer.
- эквивалентность `Default` и `new`, включая `N = 0`.

Выполни:

```bash
cargo test -p rustos-system-ui
cargo fmt --all -- --check
git diff --check
git diff --name-only
```

В финальном отчёте явно скажи, что renderer, приложения, selection и ABI не
менялись. Не выполняй commit или push.

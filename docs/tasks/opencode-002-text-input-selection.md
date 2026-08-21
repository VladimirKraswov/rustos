# OpenCode 002: UTF-8 selection для `TextInputBuffer`

## Цель

Добавить renderer-neutral selection в существующий
`TextInputBuffer<const N: usize>`. Модель должна позволить будущим `TextField`,
Terminal и inline rename заменять выбранный текст, не повреждая UTF-8 и
extended grapheme clusters.

## Разрешённые изменения

- `system-ui/src/text_input.rs`

Этот task-файл является постановкой задачи и не должен редактироваться.

## Запрещённые изменения

- `abi/`, `kernel/`, `runtime/`, `userspace/`, `varaniafs/`;
- `Cargo.toml`, зависимости и lock-файлы;
- renderer, framebuffer, event routing и приложения;
- `system-ui/src/selection.rs`: это selection коллекций, а не текста;
- `.opencode/**` и любые файлы вне репозитория.

Не выполняй commit, push, destructive Git-команды, `make clean` и массовое
форматирование workspace.

## Перед реализацией

1. Прочитай корневой `AGENTS.md` и `system-ui/AGENTS.md` полностью.
2. Выполни `git status --short --branch` и перечисли исходные изменения.
3. Прочитай `system-ui/src/text_input.rs` полностью.
4. Коротко зафиксируй, что меняешь и какие границы не затрагиваешь.

## Контракт

- Selection задаётся `anchor` и `cursor` как byte offsets.
- Оба offset находятся только на границе extended grapheme cluster.
- Публичный API предоставляет `has_selection()`, `selection_range()`,
  `set_selection(anchor, cursor)`, `clear_selection()` и `select_all()`.
- Направление сохраняется: `anchor` может находиться после `cursor`.
- `selection_range()` возвращает нормализованный полуоткрытый диапазон.
- `insert_str()` и `insert_char()` заменяют выбранный диапазон.
- `backspace()` и `delete_forward()` сначала удаляют selection.
- `set_cursor()` сворачивает selection в новую позицию.
- `set_text()` и успешная вставка оставляют selection свёрнутым.
- Ошибка `Capacity` или `InvalidPosition` не меняет `bytes`, `len`, `cursor`
  и `anchor` даже частично.
- Реализация остаётся без heap, `alloc`, `unsafe` и новых зависимостей.
- Поведение существующего буфера со свёрнутым selection не меняется.
- Event handling, clipboard, multiline, shaping, IME и undo остаются вне scope.

## Обязательные тесты

- ASCII selection слева направо и справа налево;
- замена выбранной кириллицы;
- emoji с ZWJ считается одной grapheme;
- anchor/cursor внутри UTF-8 code point или grapheme отклоняется атомарно;
- `Capacity` при замене selection не публикует частичное состояние;
- `Backspace` и `Delete` удаляют selection;
- `select_all()` и `clear_selection()`;
- все существующие тесты продолжают проходить.

## Проверки

```bash
cargo test -p rustos-system-ui -p rustos-rui -p rustos-abi
cargo fmt --all -- --check
git diff --check
git diff --name-only
```

## Отчёт

Укажи реализованный контракт, дословные команды и результаты проверок, список
изменённых файлов и оставшиеся ограничения. Отдельно подтверди, что ABI,
renderer, приложения и event routing не изменялись.

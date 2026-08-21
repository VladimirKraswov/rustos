# OpenCode 003: клавиатурное поведение `TextInputBuffer`

## Цель

Научить renderer-neutral `TextInputBuffer` применять один `KeyEvent`, соблюдая
selection и UTF-8/grapheme boundaries. Интеграция с `Runtime` будет отдельным
следующим заданием: эта задача изменяет только чистую bounded модель.

## Разрешённые изменения

- `system-ui/src/event.rs`;
- `system-ui/src/text_input.rs`.

Task-файл не редактируй.

## Запрещено

- `abi/`, `kernel/`, `runtime/` верхнего уровня, `userspace/`, `varaniafs/`;
- `system-ui/src/runtime.rs`, renderer/display list, layout, themes, приложения
  и examples;
- `system-ui/src/text_engine.rs`, clipboard и collection selection;
- зависимости, Cargo files, `.opencode/**`, commit и push.

## Сначала

Прочитай полностью корневой `AGENTS.md`, `system-ui/AGENTS.md`, этот task,
`event.rs` и `text_input.rs`. Выполни `git status --short --branch`. Продолжай
существующую модель, не создавай второй input buffer.

## Контракт модели

1. В `Key` добавь отдельные стабильные варианты `Backspace` и `Delete`, не
   меняя существующие discriminants.
2. Добавь public bounded result применения клавиши с полями/методами,
   позволяющими различить `consumed`, `changed` и `TextInputError`. Не panic при
   заполненном буфере.
3. Public метод `TextInputBuffer` принимает `KeyEvent` и обрабатывает только
   key-down:
   - `Character` без Control/Alt/System вставляет или заменяет selection;
   - Backspace/Delete используют готовую grapheme-aware модель;
   - Left/Right/Home/End двигают по grapheme boundaries;
   - Shift+navigation расширяет selection, сохраняя anchor;
   - navigation без Shift при selection сворачивает Left в начало, Right в
     конец; Home/End идут в начало/конец строки;
   - Control+A и Control+a выполняют `select_all`;
   - Enter, Tab, Up/Down/PageUp/PageDown, shortcuts с Alt/System и key-up не
     меняют модель и не объявляются обработанными;
   - Capacity возвращается в result, состояние остаётся атомарным.
4. Не дублируй UTF-8/grapheme mutation: используй существующие методы
   `TextInputBuffer` и добавляй только минимальные private helpers.

## Обязательные тесты

- ASCII, кириллица, combining grapheme и ZWJ emoji navigation/delete;
- Shift+Left/Right/Home/End и collapse без Shift;
- Control+A, replacement и Capacity atomicity;
- key-up/Enter/Alt/System игнорируются;
- result различает ignored/consumed/changed/error;
- существующие selection и grapheme tests остаются зелёными.

## Проверки

```bash
cargo test -p rustos-system-ui -p rustos-rui -p rustos-abi
cargo fmt --all -- --check
git diff --check
git diff --name-only
```

В отчёте перечисли контракт, команды/результаты, изменённые файлы, явно
отложенные runtime adapter/clipboard/IME/TextArea и подтверждение, что
ABI/renderer/application не затронуты.

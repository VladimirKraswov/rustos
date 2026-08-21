# OpenCode 004: исправить прокрутку виртуального `ListView`

## Один наблюдаемый результат

`ListView` с десятками тысяч logical items должен прокручиваться колесом,
трекпадом и программным `scroll_to`, менять `visible_range`, корректно доходить
до последнего item и возвращаться вверх. Число живых delegate nodes остаётся
bounded и не зависит от `item_count`.

## Разрешённые изменения

- `system-ui/src/collections.rs`;
- `system-ui/src/event.rs`;
- `system-ui/src/layout.rs`;
- `system-ui/src/runtime.rs`;
- `system-ui/src/tree.rs`.

Task-файл не редактируй. Сначала найди минимальную причину; не меняй все пять
файлов, если исправление помещается в меньший набор.

## Запрещено

- ABI, kernel, приложения, renderer/display-list, темы и assets;
- новый component model или второй scroll offset рядом с `ScrollModel`;
- materialization одного `Node` на каждый logical item;
- зависимости, Cargo files, `.opencode/**`, commit и push;
- исправление посторонних dirty files.

## Сначала

Полностью прочитай корневой `AGENTS.md`, `system-ui/AGENTS.md`, этот task,
`collections.rs`, scroll paths в `event.rs`, `scroll_container` в `layout.rs`,
`configure_list_view`/`scroll_to`/`render` в `runtime.rs` и соответствующие
методы `tree.rs`. Выполни `git status --short --branch`.

## Контракт

1. Единственный runtime scroll source — `node.scroll.vertical`; logical
   `ListViewState` только вычисляет range и selection.
2. После первого layout viewport/content extents должны быть ненулевыми и
   соответствовать `item_count * item_extent` с checked/saturating `u64`.
3. Wheel/trackpad над дочерним delegate обязан найти scrollable `ListView`
   ancestor, изменить offset и повредить только list/связанный scrollbar.
4. `visible_range` после scroll обязан продвигаться; range bounded,
   `start <= end <= item_count`, overscan не создаёт logical items за концом.
5. `scroll_to(u64::MAX)` доходит до maximum, и range содержит последний item;
   обратный scroll возвращает range к началу.
6. Resize или повторная configure с меньшим item count clamps offset и target.
7. Layout размещает только существующие recycled delegates. Делегаты сверх
   текущего `visible_range.len()` не должны оставаться hit-testable как
   фантомные строки за концом списка.
8. При реальном изменении offset `DispatchResult` имеет `consumed=true` и
   `changed=true`; delta на границе сохраняет существующий nested-scroll
   chaining contract.

## Обязательные тесты

- runtime `ListView` на 50_000 строк: первый render, wheel над дочерним
  delegate, новый offset и продвинутый range;
- bottom clamp через `scroll_to(u64::MAX)`, последний item входит в range;
- wheel назад до начала;
- reconfigure 50_000 -> 3 и resize clamps offset/target;
- лишние recycled delegates у конца получают пустые bounds и не выбираются;
- соседний control и полный viewport не получают лишний damage;
- существующие nested scroll и keyboard selection tests остаются зелёными.

Если часть контракта уже работает, тест обязан это подтвердить; исправляй только
реальную недостающую часть, найденную regression test.

## Проверки

```bash
cargo test -p rustos-system-ui -p rustos-rui -p rustos-abi
cargo fmt --all -- --check
git diff --check
git diff --name-only
```

В отчёте укажи точную первопричину, regression tests, изменённые файлы и явно
подтверди, что ABI/kernel/apps/renderer не затронуты.

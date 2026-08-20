# System UI: нативная компонентная платформа RustOS

## Статус

Документ фиксирует архитектуру System UI ABI v1 и состояние первого рабочего
вертикального среза. Уже реализованы:

- отдельный `no_std` crate `rustos-system-ui` без framebuffer и heap;
- единая component model для Rust builder и скомпилированного `.rui`;
- Row, Column, Stack, Grid и container breakpoint;
- типизированные размеры, constraints, padding, gap и alignment;
- тёмная, светлая и high-contrast темы;
- renderer-neutral display list и сменный render backend;
- dirty rectangles и раздельная invalidation layout/paint/semantics;
- hit testing, pointer capture, focus, Tab и Enter/Space activation;
- capture/target/bubble route и отдельное accessibility tree;
- bounded virtual-list range для десятков тысяч items;
- стабильный wire ABI приложения к будущему `uid`;
- AOT-компилятор `rustos-rui`, headless-тесты и UI Gallery в desktop.
- компонентный desktop shell: Start `Button` + `Image`, `Menu`, пункты-команды,
  taskbar clock/date и keyboard focus без ручного hit-test пунктов.

Это фундамент, а не заявление, что весь каталог controls готов. Text editing,
shaping, popup/portal, animations, persisted state и IPC-сервис расширяются
поверх зафиксированных границ ниже.

## Выбранная архитектура

```text
RUI source ── rustos-rui ──> typed .rui IR ─┐
                                             ├─> component tree
Rust API / UiBuilder ───────> NodeSpec ──────┘          │
                                                        ▼
application state ─> typed updates ─> invalidation + layout
                                         │              │
                                         │              ├─> semantics tree
                                         ▼              │
                                    display list <──────┘
                                         │
                      ┌──────────────────┼──────────────────┐
                      ▼                  ▼                  ▼
                 CPU backend       future GPU          headless
                      │                  │                  │
                   surface         command buffers      snapshots
                      │
                 displayd/window surface
```

Выбран retained component tree и renderer-neutral display list. Immediate-mode
API хуже подходит для системных тем, accessibility, автоматического сохранения
состояния и инкрементального layout. DOM/CSS/JavaScript не используется: он
существенно увеличил бы trusted code и стоимость запуска.

Композиция является основным механизмом расширения. Пользователь собирает
компонент из Panel/Text/Button и behaviours; наследование внутренних
Rust-структур runtime отсутствует.

## Границы процессов и библиотек

Целевая production-схема:

```text
application.rune
  ├─ system-ui Rust facade        compile-time types/macros, небольшой wrapper
  ├─ application UI state         приватная память процесса
  └─ ui.rui                       read-only package resource
          │ stable ABI + shared-memory queues
          ▼
system-ui@1.rune / uid            component runtime, layout, styles, resources
          │ window surface capability
          ▼
displayd                          compositor, input routing, scanout
```

Публичный ABI находится в `abi/src/ui.rs`. Он состоит только из `#[repr(C)]`
структур фиксированной ширины и не экспортирует Rust ABI. `UiSessionOpen`
связывает read-only IR capability с окном; `UiUpdate` передаёт batches
изменений; `UiEvent` возвращает команды, значения, focus и virtual-list
requests. Bulk text, image и collection data идут через shared memory, а не
копируются в каждое IPC-сообщение.

`use rustos_system_ui::prelude::*` сейчас подключает compile-time facade.
После выделения `uid` он станет RUNE DLL `org.rustos.system-ui/1`: builders и
типы останутся локальными, а compact state batches уйдут в runtime через
capability IPC. Interface ID и ABI version не зависят от имени файла.

Runtime не дублируется статически в каждом процессе, потому что glyph/image/
theme caches должны разделяться, исправления controls — применяться системно,
а accessibility/inspector — видеть единый контракт. Facade остаётся в
приложении ради type checking и не делает IPC на каждый builder call.

## Память и отказоустойчивость

Kernel не имеет heap, поэтому первая реализация использует const-generic
budgets:

```rust
type AppUi = Runtime<256, 1024, 32>;
//                   nodes commands damage rectangles
```

Переполнение tree/display list возвращает ошибку и не публикует частичный
frame. Damage metadata безопасно схлопывается в больший rectangle. Удалённый
`NodeId` содержит generation и не начинает указывать на новый компонент
повторно использованного slot.

User-space runtime сможет держать те же структуры в growable arenas, сохраняя
checked IDs, bounded messages, отсутствие указателей в ABI, атомарную проверку
IR, cache budgets и завершение только виновного процесса при плохом IR/update.

## Component model и lifecycle

`NodeSpec` — единая точка создания узла. В ней находятся kind, layout, style
class, state, resource content, command, semantic role и Tab index. Rust
builder и `.rui` decoder создают один и тот же `NodeSpec`.

Lifecycle v1:

1. decode/validate `NodeSpec`;
2. attach к parent и получение context;
3. measure/layout;
4. построение display commands;
5. hit-test/focus/event delivery;
6. property/state updates и точечная invalidation;
7. detach subtree;
8. generation invalidation и освобождение slot/resources.

Приложение не вызывает промежуточные lifecycle hooks вручную. Асинхронный
handler хранит weak `(session, NodeId generation)`; completion для уничтоженного
компонента отбрасывается.

Состояния — независимые flags: hovered, pressed, focused, selected, checked,
disabled, loading, invalid и read-only. Theme разрешает их в `ComputedStyle`.
Behavior не меняется при смене темы.

## Properties, state и bindings

В v1 работают typed properties через `NodeSpec`, `set_state` и `set_content`.
Каждое изменение имеет явную invalidation category:

- цвет/state — paint и при необходимости semantics;
- содержимое — paint/semantics, а после text measurement также layout;
- size/constraint — layout вверх до ближайшей устойчивой границы;
- window resize — root layout и container queries.

Следующее расширение добавит generated property schemas и однонаправленные
bindings `state -> property`. Dependency graph строится AOT, циклы являются
ошибкой компиляции. Двунаправленное изменение остаётся явным событием.

Общие stores живут в приложении. UI-runtime получает immutable snapshot/batch,
публикует command/value events и не исполняет произвольный код из `.rui`.

## Layout

Layout использует логические целочисленные пиксели и поддерживает Auto, Px,
Percent (0..1000), weighted Fill, min/max, padding, gap, alignment, Row,
Column, Stack, Grid, resize окна и container breakpoint. Breakpoint зависит от
места внутри родителя, а не от глобальной ширины экрана.

DPI и пользовательский scale находятся в `Theme::scale_milli`; применение ко
всем метрикам станет отдельным проходом, чтобы физические пиксели не попадали
в API приложения. Будущий measure cache получает ключ `(component,
constraints, text/font version)`; независимые subtree смогут layout'иться на
worker над immutable snapshot.

## Render pipeline

Компоненты создают `Fill`, `Border`, `Text`, `Image`, `Fraction` и
`SelectionMark`; они не получают framebuffer. `RenderBackend` реализует эти
операции для CPU, GPU, remote или headless target.

Runtime хранит три независимых dirty flags. Изменение hover кнопки:

1. не запускает layout;
2. перестраивает bounded display list;
3. добавляет bounds одной кнопки в damage;
4. исполняет только commands, пересекающие damage;
5. передаёт те же rectangles scanout driver'у.

Это проверяет unit-тест: damage кнопки меньше четверти окна. Перестроение
display list в v1 полное, raster/present инкрементальны. Следующая оптимизация
— cached command ranges на subtree и reusable layers; API при этом не меняется.

Общесистемный resource cache индексируется `(package capability, resource id,
scale, theme, decoder version)`. Entry содержит только разрешённый ресурс;
приватный pixel buffer другого приложения не доступен через lookup.

## Input, focus и commands

Window server нормализует устройства в pointer/key/touch события. Runtime
делает hit testing. Down устанавливает pointer capture; Up приходит исходному
control даже за его пределами. Hover без смены target не инвалидируется.

Маршрут event: Capture от root к parent target, Target, затем Bubble к root.
Demo использует итоговый `DispatchResult`; полный маршрут уже доступен custom
behaviours, inspector и recorder. Focus переходит по стабильному
`(tab_index, document_order)`. Disabled controls исключены из hit test и Tab.

`CommandId` связывает кнопку, menu item, toolbar, shortcut и command palette.
Control не содержит application callback: runtime возвращает command event,
приложение обновляет state. Следующий `CommandRegistry` добавит общие
enabled/visible/checked/title/icon и shortcut scopes.

Start menu является проверкой этого контракта на системном UI. Window server
передаёт ему нормализованные `Down/Up/Move`, runtime удерживает pointer capture
и возвращает `OpenTerminal`, `OpenGallery` либо `Shutdown`. Нажатие не сверяет
ручные прямоугольники пунктов. `Menu` исключён из Tab-порядка как focus scope,
поэтому первый Tab выбирает первый дочерний `Button`; Escape остаётся глобальной
командой закрытия popup. Клик не проходит сквозь surface к окну под ним.

## Accessibility

Семантика не извлекается из пикселей. `SemanticsTree` строится из role,
accessible name, bounds, state и actions. Disabled control не публикует
действия. Decoration с ролью `None` не засоряет дерево.

Отдельный assistive service сможет читать структуру без доступа к приватной
surface, менять focus, активировать разрешённые действия и работать при high
contrast, большом scale и без мыши. `rustos-rui` будет усиливаться: Button без
имени, плохой Tab order и недоступное с клавиатуры действие станут ошибками.

## Декларативный RUI

Source v1 — компактный line-based язык, но не runtime ABI:

```text
rui 1
Column id=page width=fill height=fill padding=18 gap=12
Text id=title parent=page text=1 width=fill height=px:34 role=heading
Button id=save parent=page text=2 command=7 width=fill height=px:40 style=1
```

`text=1` — ID локализованной строки в RUNE resources. Parent объявляется
раньше ребёнка. Length: `auto`, `fill[:weight]`, `px:N`, `pct:0..1000`.

```bash
cargo run -p rustos-rui -- check sdk/ui/gallery.rui
cargo run -p rustos-rui -- compile sdk/ui/gallery.rui build/gallery.rui
```

Компилятор проверяет enums, числа, parents и ranges, затем запускает runtime
validator. `.rui` содержит 32-byte header и 64-byte records. Runtime сначала
проверяет весь input, строит candidate tree и только после успеха заменяет
live tree. Source syntax можно улучшать без смены IR; macro DSL тоже генерирует
`NodeSpec` или `.rui`, отдельной UI-системы не появится.

## Потоки

Выбрана модель одного UI-owner потока на приложение:

- state mutation, event dispatch и lifecycle — UI thread;
- immutable layout snapshot — сначала UI thread, позднее worker pool;
- display-list build — UI thread или worker после snapshot;
- raster/composition — render thread `displayd`;
- I/O/вычисления — worker threads приложения;
- результат worker — message/command completion в UI queue.

Worker не получает mutable component. Cancellation token принадлежит lifecycle
scope; detach отменяет token, generation check блокирует поздний completion.

## Версионирование

Независимо версионируются System UI RUNE interface major
(`org.rustos.system-ui/1`), wire `UI_ABI_VERSION`, typed `UI_IR_VERSION` и
theme tokens. Compatible поля добавляются через property/record IDs и feature
flags. Unknown required feature отклоняет session до показа окна.
Несовместимая семантика получает новый major/interface ID.

Система хранит предыдущий major, пока установленные приложения от него
зависят, либо предоставляет протестированный compatibility provider. Новая
тема может обновить цвета старого приложения, но не behavior его ABI major.

## UI Gallery

`kernel/src/apps/ui_showcase.rs` пока делит bootstrap window с terminal. Это
временная интеграция до ring-3 display client: UI tree не обращается к window
manager/framebuffer, а `FramebufferBackend` является сменным адаптером.

Открыть приложение можно через Start → `UI GALLERY` или командой `uidemo`.
Проверяются adaptive Row/Column, sans Cyrillic/Latin, light/dark theme,
Button, CheckBox, Switch, TextField, ProgressBar, ListView, mouse, Tab/Enter,
local damage и virtual range на 50 000 элементов.

## Тесты и бюджеты

```bash
cargo test -p rustos-system-ui -p rustos-rui -p rustos-abi
cargo run -p rustos-rui -- check sdk/ui/gallery.rui
./scripts/build.sh
./scripts/test-boot.sh
./scripts/test-gui.sh
```

`PerformanceCounters` считает frames, layout passes, display-list rebuilds,
backend commands, rasterized pixels, nodes и commands. Следующий inspector
добавит monotonic time и budgets. Цели CPU backend для 1280×720:

- pointer hover без смены target: ноль UI redraw;
- hover/click: damage одного control;
- drag окна: существующий outline preview, не полный UI raster;
- scroll: viewport + два overscan item с каждой стороны;
- frame: 16.6 ms при 60 Hz;
- layout/build: до 4 ms для 1 000 узлов;
- ноль allocations в kernel UI hot path.

CI должен добавить snapshot hashes разных размеров, focus/event routes,
semantic tree, malformed IR corpus, virtual collections и pixel budgets.
Snapshot backend обязан исполнять те же display commands.

## Следующие этапы

1. Вынести `uid` и displayd в ring 3; подключить ABI queue.
2. Добавить resource table, локализацию, fallback fonts, shaping и bidi.
3. Реализовать TextEditor: selection, IME, clipboard, undo/redo.
4. Добавить ScrollView physics, recycled delegates и variable extents.
5. Ввести popup/portal, focus scopes, modal barriers, menu/dialog/tooltip.
6. Добавить typed property schema, AOT bindings и command registry.
7. Добавить compositor animations, reduced motion и frame clock.
8. Реализовать persisted UI state с package/version namespace.
9. Добавить inspector, record/replay, snapshots и budget reports.
10. Подключить GPU backend без изменения component/API/IR.

Сначала стабилизируются process/service boundaries и текст, затем расширяется
каталог controls. Сотни widgets поверх нестабильного ABI дали бы больше кода,
но не системную UI-платформу.

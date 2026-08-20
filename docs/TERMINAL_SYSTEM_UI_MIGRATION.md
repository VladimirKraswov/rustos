# Перенос Terminal на System UI

Это спецификация небольшого следующего change, а не новая архитектура terminal.
Она нужна, чтобы перенос presentation не затронул одновременно shell, VFS,
process manager и будущую ring-3 границу.

## Исходное состояние

`kernel/src/apps/terminal.rs` сейчас совмещает:

- независимое состояние terminal: клетки, cursor, input, cwd и команды;
- bootstrap shell/VFS orchestration;
- прямой CPU raster через `Framebuffer` и `font` в `draw`/`draw_input_line`.

Несколько экземпляров уже имеют отдельный `Terminal` и уничтожаются вместе с
`ApplicationMemory`. Это поведение нельзя сломать. `system-ui` уже используется
UI Gallery, Desktop Settings и shell, но Terminal пока обходит component tree.

## Цель первого переноса

Заменить только presentation Terminal на retained `rustos-system-ui` tree.
Terminal остаётся bootstrap-приложением kernel. Команды, VFS, process launch,
display/mouse/cursor actions и keyboard parsing сохраняют текущую семантику.

```text
DesktopSession
  └─ Terminal (владелец независимого состояния)
       ├─ Terminal model: cells/input/cwd/commands
       └─ TerminalUi: Runtime + NodeId видимых строк
              └─ TerminalBackend: единственный adapter к Framebuffer/font
```

Предпочтительное размещение adapter — новый
`kernel/src/apps/terminal_ui.rs`. Публичным API crate он не становится.

## Component tree

Минимальное дерево:

- root `Panel`/`Column` с фоном terminal;
- `ListView`, содержащий фиксированное число узлов видимых строк;
- строковые узлы с `ResourceId`, которые Terminal backend разрешает в строки
  текущего model; не создавай `String` или новый node на каждый frame;
- отдельная последняя строка/caret может быть `TextField` либо строковым узлом,
  но её доступная роль должна оставаться `TextField`.

Текущий terminal хранит цвет на клетку. На первом этапе один display-list Text
может обозначать целую строку, а Terminal-specific backend — рисовать её
цветовые runs из read-only model. Это временный resource adapter, а не причина
добавлять terminal semantics в общий `system-ui` crate.

`TerminalUi` должен владеть `Runtime` и стабильными `NodeId`. Backend получает
только read-only snapshot/model и `&mut Framebuffer`; он не выполняет команды и
не меняет cwd. Разделение полей должно позволять Rust borrow checker доказать,
что runtime и model не alias mutable.

## Invalidation и производительность

- Ввод обычной буквы повреждает только текущую строку, не весь viewport.
- Для этого используй готовый `Runtime::invalidate_content(NodeId)`: resource
  строки сохраняет стабильный ID, а backend перечитывает изменившиеся клетки.
- Newline/scroll/clear/resize могут инвалидировать все видимые строки.
- Resize вызывает `Runtime::resize` и пересчитывает число видимых строк без
  выхода за `ROWS`/`COLS`.
- Полный repaint вызывается только при восстановлении surface/окна.
- Никаких allocation, framebuffer-sized копий и перестроения component tree на
  каждую клавишу.
- Ошибка capacity/render возвращает безопасный результат и пишет ограниченную
  serial diagnostic; нельзя молча рисовать половину frame.

После интеграции `FrameResult` damage должен доходить до существующего
compositor/present path. Старый special-case `draw_focused_terminal_line`
можно удалить только если новый путь сохраняет локальный present; временный
тонкий adapter допустим, если он использует damage runtime, а не старый raster.

## Что запрещено в этом change

- переносить terminal или shell в ring 3;
- менять команды, формат RUNE, VFS ABI или capability policy;
- добавлять terminal-специфичный компонент/primitive в общий System UI;
- объединять состояние разных окон в singleton/static;
- копировать backend UI Gallery целиком без выделения только нужного adapter;
- ухудшать кириллицу, цвета, font settings, caret или resize;
- переписывать window manager и desktop.

## Проверяемые критерии

1. В `Terminal::draw` и input redraw больше нет прямого цикла raster клеток;
   отрисовкой управляет `rustos-system-ui::Runtime`.
2. Два terminal windows сохраняют разные cwd/input/scrollback; закрытие и
   повторный запуск создают чистое состояние.
3. `help`, `pwd`, `write`, `cat`, font changes и `run` продолжают работать.
4. Русский banner, цветной вывод, caret и resize видны корректно.
5. Одна введённая буква публикует damage существенно меньше площади окна.
6. Добавлены unit-тесты модели/view: стабильные resource IDs, локальный damage,
   scroll/resize bounds и независимость двух экземпляров.

Обязательные команды:

```bash
cargo test -p rustos-system-ui
cargo -Zjson-target-spec -Zbuild-std=core,alloc build \
  -p rustos-kernel --target targets/x86_64-unknown-rustos.json
make test-arch
make test-gui
cargo fmt --all -- --check
git diff --check
```

Следующий отдельный milestone после этого change — вынести shell/model в ring 3
и заменить kernel adapter на UI session ABI. Его нельзя «заодно» включать в
первый перенос.

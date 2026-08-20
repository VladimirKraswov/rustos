# Оконная система RustOS

## Контракт

Оконная система разделена на два независимых слоя:

- `rustos-abi::window` — стабильные 40-байтные create/command/event-пакеты без
  указателей. Их можно передавать через capability IPC или shared-memory ring;
- `rustos-video::window` — проверяемая state machine оконного сервера,
  ограничения geometry, hit-test рамки и bounded FIFO событий.

Такое разделение намеренное: приложение не получает framebuffer и не меняет
состояние compositor'а напрямую. Оно отправляет команду, оконный сервер
проверяет capability и ограничения, а затем возвращает событие с **фактически
применённой** позицией, размером и состоянием.

Сейчас bootstrap desktop вызывает этот контракт внутри kernel session. Уже
работают таблица независимых окон, focus, Z-order, отдельные taskbar entries и
динамическое владение памятью приложения. При переносе compositor'а в
изолированный `displayd` структуры ABI и код клиентов останутся прежними;
изменится transport вызова и владелец surface capability.

## Создание и настройка

`WindowCreateRequest::standard(rect, min_width, min_height)` создаёт обычное
окно. `WindowCreateRequest::new` принимает явный `WindowStyle`. ID назначает
оконный сервер и возвращает его в первом событии `SHOWN`; нулевой ID запрещён.

Флаги `WindowStyle` независимы:

| Флаг | Эффект |
|---|---|
| `BORDER` | рисовать рамку |
| `TITLE_BAR` | рисовать системный заголовок |
| `MOVABLE` | разрешить перемещение |
| `RESIZABLE` | разрешить edge/corner resize |
| `BUTTON_MINIMIZE` | показать кнопку сворачивания |
| `BUTTON_MAXIMIZE` | показать maximize/restore |
| `BUTTON_CLOSE` | показать кнопку закрытия |

`STANDARD` включает все флаги, `FRAMELESS` — ни одного. Например, окно без
кнопки maximize, но с остальным стандартным оформлением:

```rust
let style = WindowStyle::STANDARD.without(WindowStyle::BUTTON_MAXIMIZE);
let create = WindowCreateRequest::new(
    WindowRect::new(120, 80, 900, 600),
    480,
    300,
    style,
);
```

Скрытие кнопки меняет только системное оформление. Управляющая сторона с
подходящей capability всё ещё может отправить соответствующую команду — это
нужно для taskbar, горячих клавиш и supervisor'а.

## Команды приложения и window manager

Для частых операций есть типизированные constructors:

```rust
WindowCommand::move_to(id, 240, 120);
WindowCommand::resize(id, WindowRect::new(240, 120, 1024, 640));
WindowCommand::minimize(id);
WindowCommand::maximize(id);
WindowCommand::restore(id);
WindowCommand::close(id);
WindowCommand::show(id);
WindowCommand::set_style(id, style);
```

Каждая команда содержит ABI version и `WindowId`. Сервер отвергает неизвестную
версию, чужой ID, неизвестные style bits, запрещённые style-операции и неверный
переход состояния. Размер ограничивается minimum size и рабочей областью;
перемещённое окно нельзя полностью потерять за границей экрана.

`rustos-video::resize_from_edges` одинаково обрабатывает левую, правую,
верхнюю, нижнюю стороны и четыре угла. Противоположный край остаётся на месте,
а итоговая geometry повторно валидируется сервером. Во время жеста desktop
показывает только лёгкий title/outline preview и публикует узкие damage-области;
полное содержимое окна рисуется один раз при отпускании мыши.

## События приложению

На каждое принятое изменение сервер выдаёт `WindowEvent`:

- `SHOWN`, `MOVED`, `RESIZED`;
- `MINIMIZED`, `MAXIMIZED`, `RESTORED`;
- `CLOSE_REQUESTED`, `CLOSED`;
- `STYLE_CHANGED`.

Событие несёт полный snapshot `rect`/`state` и монотонный `serial`. Клиент не
должен угадывать результат своей команды по исходным параметрам: например,
после resize он использует размер из `RESIZED`. Пропуск определяется разрывом
`serial`. `WindowEventQueue<N>` не выделяет heap и при заполнении возвращает
backpressure вместо молчаливой потери lifecycle event.

Закрытие выполняется в два шага:

1. нажатие `X` создаёт `CLOSE_REQUESTED`;
2. приложение сохраняет данные или показывает подтверждение, затем отвечает
   `WindowCommand::close`; только после этого приходит `CLOSED`.

Встроенный terminal не имеет несохранённого документа, поэтому подтверждает
закрытие сразу. После `CLOSED` desktop удаляет registry entry, вызывает
destructor конкретного application instance и возвращает его физические кадры.
Повторный запуск создаёт новый `WindowId` и чистое состояние; закрытый shell не
может «воскреснуть» со старым cwd или экранным буфером. Редактор сможет задержать
второй шаг без блокировки desktop.

## Состояния

| Текущее | Команда | Новое | Что сохраняется |
|---|---|---|---|
| normal | maximize | maximized | прежний normal rect |
| normal/maximized | minimize | minimized | состояние до minimize |
| minimized | restore | normal или maximized | точное предыдущее состояние |
| maximized | restore | normal | прежний normal rect |
| любое видимое | close | closed | server policy решает: сохранить или уничтожить |
| closed | show | normal | валидированный restore rect |

После смены разрешения `reflow` пересчитывает normal geometry и рабочую область
maximized-окон. Свёрнутые и закрытые окна при этом не становятся видимыми.

## Проверки и следующий слой изоляции

Host-тесты покрывают переходы maximize/minimize/restore, создание, закрытие,
style policy, minimum/work-area constraints, corner resize и FIFO/backpressure.
GUI-тест дополнительно посылает реальные PS/2 mouse events в QEMU и проверяет
drag, resize, minimize и runtime смену видеорежима.

До полностью изолированного многопрограммного desktop остаётся вынести уже
работающие registry/focus/Z-order из bootstrap session в ring-3 `displayd` и
запустить сами GUI-клиенты как scheduler tasks. Каждому клиенту будет выдаваться
window capability, а пиксели будут передаваться не сообщениями, а через
shared-memory surface capability с `commit(damage, generation)`. Падение
клиента тогда автоматически отзовёт его capabilities и удалит только его окна,
не затронув compositor или остальные программы.

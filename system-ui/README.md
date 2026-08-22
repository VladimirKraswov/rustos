# SystemUI: компоненты приложений RustOS

`rustos-system-ui` — общая `no_std`-библиотека компонентов RustOS. Она строит
типизированное дерево UI, выполняет layout, обрабатывает мышь и клавиатуру и
формирует display list. Один и тот же код приложения может работать через
CPU-renderer, GPU backend или headless backend тестов. Основной GPU compiler
находится в [`libs/ui-gpu`](../libs/ui-gpu): SystemUI не зависит от
VirGL/Vulkan и сохраняет component API при смене видеодрайвера. CPU backend
остаётся эталоном pixel-diff тестов и аварийным recovery path.

Главное правило: приложение описывает **что** находится в окне, но не рисует
пиксели самостоятельно. Кнопка публикует `CommandId`, текст и изображения
адресуются через `ResourceId`, а оконный сервер предоставляет renderer и
доставляет `InputEvent`.

## Подключение

Для crate внутри репозитория добавьте зависимость в `Cargo.toml`:

```toml
[dependencies]
rustos-system-ui = { path = "../../system-ui" }
```

В коде удобно начать с prelude:

```rust
use rustos_system_ui::prelude::*;
use rustos_system_ui::{Color, FontSpec, Rect, RenderBackend};
```

`system-ui` не требует `std` и heap allocator. Его ёмкости задаются в типе:

```rust
type AppRuntime = Runtime<64, 256, 16>;
//                         │    │    └─ максимум damage-прямоугольников
//                         │    └────── максимум display-команд
//                         └─────────── максимум component nodes
```

При переполнении runtime возвращает ошибку и не показывает частично
построенный кадр. Для большого приложения увеличивайте budgets осознанно и
контролируйте `runtime.counters()`.

## Минимальное приложение

Полный компилируемый шаблон находится в
[`examples/minimal_components.rs`](examples/minimal_components.rs). Он создаёт
`Label`, `TextField`, `Button` и прокручиваемый `ListView`, принимает UTF-8 ввод,
обрабатывает команду кнопки и строит кадр через headless backend.

Запустить его на macOS или Linux можно так:

```sh
cargo run -p rustos-system-ui --example minimal_components
```

Минимальная часть, создающая дерево компонентов, выглядит так:

```rust
use rustos_system_ui::{
    Align, CommandId, ComponentKind, Content, Edges, LayoutSpec, Length,
    NodeSpec, Rect, ResourceId, Runtime, SelectionMode, SemanticRole, Theme,
};

const COMMAND_ADD: CommandId = CommandId(1);
type AppRuntime = Runtime<32, 128, 8>;

let mut runtime = AppRuntime::new(Rect::new(0, 0, 640, 420), Theme::light());
let list_node;

{
    let root = runtime.tree().root();
    let mut ui = runtime.builder();

    let page = ui.column(
        root,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            padding: Edges::all(24),
            gap: 12,
            align: Align::Stretch,
            ..LayoutSpec::default()
        },
    )?;

    // Label / обычный текст.
    ui.text(page, ResourceId(1), line(32))?;

    // Значение и accessible name — строки из resource table приложения.
    let input = ui.text_field(page, ResourceId(100), ResourceId(2), line(42))?;

    // Вместо callback компонент возвращает COMMAND_ADD.
    let button = ui.button(page, ResourceId(3), COMMAND_ADD, line(42))?;

    list_node = ui.list_view(
        page,
        LayoutSpec {
            width: Length::Fill(1),
            height: Length::Fill(1),
            min_height: 120,
            gap: 4,
            ..LayoutSpec::default()
        },
    )?;

    for text in [ResourceId(4), ResourceId(5), ResourceId(6)] {
        let mut item = NodeSpec::new(ComponentKind::Text);
        item.layout = line(36);
        item.content = Content::Text(text);
        item.role = SemanticRole::ListItem;
        item.accessible_name = text;
        ui.component(list_node, item)?;
    }
}

runtime.configure_list_view(list_node, 3, 36, SelectionMode::Single)?;

fn line(height: u16) -> LayoutSpec {
    LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(height),
        ..LayoutSpec::default()
    }
}
```

В настоящем конструкторе, возвращающем `Self`, вместо `?` можно вернуть свою
ошибку и преобразовать `TreeError`/`RuntimeError`. Не скрывайте переполнение
дерева через `unwrap_or(NodeId::NONE)`: приложение не должно продолжать работу
с неполным интерфейсом.

## Ресурсы и изменяемый текст

Дерево намеренно не хранит `String` и указатели процесса. Оно хранит стабильный
`ResourceId`, который backend разрешает через resource table RUNE-пакета:

```rust
fn text_resource(id: ResourceId, input: &str) -> &str {
    match id.0 {
        1 => "Новая заметка",
        2 => "Название",
        3 => "Сохранить",
        100 => input, // динамическая строка модели приложения
        _ => "",
    }
}
```

Для однострочного UTF-8 ввода используйте bounded-модель:

```rust
let mut value = TextInputBuffer::<128>::new();
value.set_text("Привет")?;
value.insert_char('!')?;

// ResourceId поля остался прежним, но backend должен перечитать строку.
runtime.invalidate_content(input_node)?;
```

`invalidate_content` повреждает только bounds компонента и не запускает полный
layout. Если новое содержимое меняет геометрию, обновите соответствующее
свойство дерева либо перестройте layout.

## События и команды

Все источники ввода нормализуются в `InputEvent` с логическими координатами:

```rust
let result = runtime.dispatch(event);

if result.command == COMMAND_ADD {
    model.add_item();
}

if result.target == input_node {
    // Передайте Character/navigation/IME в текстовую модель приложения.
}
```

Runtime сам выполняет hit-test, hover/pressed/focus, pointer capture,
Tab-навигацию, активацию через Enter/Space, выбор и прокрутку коллекций.
Приложение обрабатывает предметные действия через `CommandId`, поэтому
компоненты не содержат app-specific callbacks и могут сериализоваться в `.rui`.

После события вызывайте `render` только по запросу frame scheduler. Runtime
сам ограничит raster повреждёнными областями:

```rust
let frame = runtime.render(&mut window_backend)?;
compositor.present(frame.damage());
```

## Основные компоненты

- Layout: `Panel`, `Row`, `Column`, `Stack`, `Grid`, `SplitView`.
- Содержимое: `Text`/Label, `Image`, `Icon`, `Divider`.
- Ввод: `Button`, `CheckBox`, `RadioButton`, `Switch`, `TextField`,
  `TextArea`, `Slider`, `Select`.
- Коллекции: `ScrollView`, `ScrollBar`, `ListView`, `TreeView`, `TableView`,
  `GridView`.
- Структура приложения: `Toolbar`, `StatusBar`, `Tab`, `Menu`, `Dialog`,
  `ProgressBar`.
- Составной обозреватель файлов: `build_file_browser` и `FileBrowserSpec`.

Все стандартные компоненты создаются через `runtime.builder()`. Если нужен
особый state, style class или semantic role, создайте `NodeSpec` и передайте
его в `UiBuilder::component` — это всё ещё то же дерево, layout и event runtime.

## Списки и прокрутка

Небольшой список может иметь по одному дочернему узлу на элемент. Для десятков
тысяч строк не создавайте десятки тысяч nodes: настройте logical collection
через `configure_list_view`, а дочерними оставьте только переиспользуемые
видимые delegates. Актуальный диапазон доступен через `list_view_state`:

```rust
runtime.configure_list_view(list, 50_000, 32, SelectionMode::Extended)?;

if let Some(state) = runtime.list_view_state(list) {
    let visible = state.visible_range(runtime.tree().get(list).unwrap().scroll.vertical);
    // Обновить ResourceId/данные только visible delegates.
}
```

Колесо мыши передавайте как `PointerKind::Scroll`. `ScrollView`, `ListView`,
`TreeView` и `TableView` сами применяют scroll policy и рисуют scrollbar.

## Встраивание в приложение RustOS

Минимальный жизненный цикл окна:

1. Создать `Runtime` по логическому `Rect` клиентской области.
2. Один раз построить дерево через `UiBuilder`.
3. Получать от оконного сервера нормализованные `InputEvent`.
4. Менять только модель приложения и typed properties runtime.
5. На frame callback вызвать `render` с backend поверхности окна.
6. Передать `FrameResult::damage()` compositor-у.
7. При resize вызвать `runtime.resize(new_logical_rect)`; физический HiDPI
   scale применяет renderer перед растеризацией, а не layout приложения.

Пока нативный ring-3 facade оконного сервера продолжает оформляться, пример
показывает прямое встраивание runtime — тот же путь сейчас используют
системные приложения ядра. Публичный API компонентов и формат `ResourceId` от
выбранного CPU/GPU backend не зависят.

## Проверка приложения и библиотеки

```sh
cargo run -p rustos-system-ui --example minimal_components
cargo test -p rustos-system-ui -p rustos-rui -p rustos-abi
make test-gui
```

Для нового компонента обязательны: builder API, semantic role/action,
клавиатурное и pointer-поведение, состояния темы и тест локального damage.

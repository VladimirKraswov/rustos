//! Минимальное приложение на SystemUI.
//!
//! Пример использует headless backend, поэтому запускается на host и проверяет
//! тот же component tree, layout, ввод и damage, которые использует RustOS.

use rustos_system_ui::{
    Align, Color, CommandId, ComponentKind, Content, Edges, FontSpec, InputEvent, Key, KeyEvent,
    LayoutSpec, Length, NodeId, NodeSpec, PointerEvent, PointerKind, Rect, RenderBackend,
    ResourceId, Runtime, SelectionMode, SemanticRole, TextInputBuffer, Theme,
};

const TEXT_TITLE: ResourceId = ResourceId(1);
const TEXT_FIELD_NAME: ResourceId = ResourceId(2);
const TEXT_BUTTON: ResourceId = ResourceId(3);
const TEXT_FIRST: ResourceId = ResourceId(4);
const TEXT_SECOND: ResourceId = ResourceId(5);
const TEXT_THIRD: ResourceId = ResourceId(6);
const TEXT_INPUT_VALUE: ResourceId = ResourceId(100);

const COMMAND_ADD: CommandId = CommandId(1);

type AppRuntime = Runtime<32, 128, 8>;

struct App {
    ui: AppRuntime,
    input: TextInputBuffer<128>,
    input_node: NodeId,
    button_node: NodeId,
    submitted: bool,
}

impl App {
    fn new(viewport: Rect) -> Self {
        let mut ui = AppRuntime::new(viewport, Theme::light());
        let (input_node, button_node, list_node) = {
            let root = ui.tree().root();
            let mut builder = ui.builder();

            // Корневая колонка задаёт общие отступы и расстояние между
            // компонентами. Все размеры — логические пиксели.
            let page = builder
                .column(
                    root,
                    LayoutSpec {
                        width: Length::Fill(1),
                        height: Length::Fill(1),
                        padding: Edges::all(24),
                        gap: 12,
                        align: Align::Stretch,
                        ..LayoutSpec::default()
                    },
                )
                .expect("в дереве достаточно места для страницы");

            // Text — это Label: он показывает строку из resource table.
            builder
                .text(page, TEXT_TITLE, line(32))
                .expect("label создан");

            let input_node = builder
                .text_field(page, TEXT_INPUT_VALUE, TEXT_FIELD_NAME, line(42))
                .expect("text field создан");

            let button_node = builder
                .button(page, TEXT_BUTTON, COMMAND_ADD, line(42))
                .expect("button создан");

            let list_node = builder
                .list_view(
                    page,
                    LayoutSpec {
                        width: Length::Fill(1),
                        height: Length::Fill(1),
                        min_height: 120,
                        gap: 4,
                        ..LayoutSpec::default()
                    },
                )
                .expect("list view создан");

            for resource in [TEXT_FIRST, TEXT_SECOND, TEXT_THIRD] {
                let mut item = NodeSpec::new(ComponentKind::Text);
                item.layout = line(36);
                item.content = Content::Text(resource);
                item.role = SemanticRole::ListItem;
                item.accessible_name = resource;
                builder
                    .component(list_node, item)
                    .expect("элемент списка создан");
            }

            (input_node, button_node, list_node)
        };

        // ListView хранит только видимые delegates. Logical collection и
        // выбор принадлежат модели приложения.
        ui.configure_list_view(list_node, 3, 36, SelectionMode::Single)
            .expect("модель списка связана с компонентом");

        let mut input = TextInputBuffer::new();
        input
            .set_text("Привет, RustOS")
            .expect("начальная строка помещается в bounded buffer");

        Self {
            ui,
            input,
            input_node,
            button_node,
            submitted: false,
        }
    }

    fn handle_event(&mut self, event: InputEvent) {
        // Runtime выполняет hit-test, фокус, pointer capture, прокрутку и
        // возвращает CommandId вместо хранения callback внутри компонента.
        let result = self.ui.dispatch(event);

        // Текстовая модель остаётся данными приложения. Здесь показан
        // минимальный ввод символов; полноценный редактор использует
        // TextEditorController, selection, clipboard и IME-события.
        if result.target == self.input_node {
            if let InputEvent::Key(KeyEvent {
                key: Key::Character(character),
                pressed: true,
                ..
            }) = event
            {
                if self.input.insert_char(character).is_ok() {
                    self.ui
                        .invalidate_content(self.input_node)
                        .expect("input node существует");
                }
            }
        }

        if result.command == COMMAND_ADD {
            self.submitted = true;
        }
    }

    fn render(&mut self) -> u32 {
        // В настоящем приложении здесь используется backend оконного сервера.
        // Headless-версия проверяет ресурсный контракт и считает команды.
        let mut backend = HeadlessBackend::new(self.input.as_str());
        self.ui.render(&mut backend).expect("кадр построен");
        backend.commands
    }
}

fn line(height: u16) -> LayoutSpec {
    LayoutSpec {
        width: Length::Fill(1),
        height: Length::Px(height),
        ..LayoutSpec::default()
    }
}

struct HeadlessBackend<'a> {
    input: &'a str,
    commands: u32,
}

impl<'a> HeadlessBackend<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, commands: 0 }
    }

    fn text_resource(&self, id: ResourceId) -> &str {
        match id {
            TEXT_TITLE => "Минимальное приложение SystemUI",
            TEXT_FIELD_NAME => "Название элемента",
            TEXT_BUTTON => "Добавить",
            TEXT_FIRST => "Документы",
            TEXT_SECOND => "Изображения",
            TEXT_THIRD => "Исходный код",
            TEXT_INPUT_VALUE => self.input,
            _ => "",
        }
    }
}

impl RenderBackend for HeadlessBackend<'_> {
    fn fill(&mut self, _: Rect, _: Color, _: Rect) {
        self.commands += 1;
    }

    fn border(&mut self, _: Rect, _: Color, _: u8, _: Rect) {
        self.commands += 1;
    }

    fn text(&mut self, _: Rect, resource: ResourceId, _: Color, _: FontSpec, _: Rect) {
        let _resolved_utf8 = self.text_resource(resource);
        self.commands += 1;
    }

    fn image(&mut self, _: Rect, _: ResourceId, _: Color, _: Rect) {
        self.commands += 1;
    }
}

fn main() {
    let mut app = App::new(Rect::new(0, 0, 640, 420));
    let first_frame_commands = app.render();

    // Имитируем фокус поля и ввод Unicode-символа.
    let input_rect = app
        .ui
        .tree()
        .get(app.input_node)
        .expect("input node существует")
        .rect;
    app.handle_event(InputEvent::Pointer(PointerEvent::at(
        PointerKind::Down,
        input_rect.x + 4,
        input_rect.y + 4,
    )));
    app.handle_event(InputEvent::Pointer(PointerEvent::at(
        PointerKind::Up,
        input_rect.x + 4,
        input_rect.y + 4,
    )));
    app.handle_event(InputEvent::Key(KeyEvent {
        key: Key::Character('!'),
        pressed: true,
        modifiers: 0,
        shift: false,
    }));

    // Имитируем обычный click по Button и получаем COMMAND_ADD.
    let button_rect = app
        .ui
        .tree()
        .get(app.button_node)
        .expect("button node существует")
        .rect;
    for kind in [PointerKind::Down, PointerKind::Up] {
        app.handle_event(InputEvent::Pointer(PointerEvent::at(
            kind,
            button_rect.x + 4,
            button_rect.y + 4,
        )));
    }

    let update_commands = app.render();
    println!(
        "SystemUI: first_frame={first_frame_commands}, update={update_commands}, input={:?}, submitted={}",
        app.input.as_str(),
        app.submitted
    );
}

# Очередь малых задач для OpenCode

Задачи идут последовательно. Каждая строка — отдельная сессия и отдельный
review; соседние пункты нельзя объединять «заодно».

| Порядок | Статус | Малый модуль | Наблюдаемый результат | Почему подходит |
|---:|---|---|---|---|
| 1 | готово | UTF-8 `TextInputBuffer` в `system-ui` | атомарная вставка, удаление и движение курсора с тестами | чистый `no_std`, нет ABI и framebuffer |
| 2 | следующий | Selection для `TextInputBuffer` | anchor/range, replace selection и UTF-8 tests | расширяет готовый контракт одним состоянием |
| 3 | ожидает | TextField key behaviour | `KeyEvent` меняет model и повреждает только control | один control поверх пунктов 1–2 |
| 4 | ожидает | Clipboard ABI records | versioned copy/paste request/reply и validation tests | только wire contract, без service |
| 5 | ожидает | Clipboard client facade | safe bounded wrapper над готовым IPC | один DLL facade, service отдельно |
| 6 | ожидает | Calculator | независимый bootstrap GUI instance на System UI | маленькое приложение без device access |
| 7 | ожидает | Image viewer model | fit/zoom/pan state и unit tests | чистая логика до decoder/GUI integration |
| 8 | ожидает | System information panel | read-only presentation уже доступных metrics | небольшой consumer, без нового kernel ABI |
| 9 | ожидает | Terminal input migration | ввод Terminal через готовые TextField/clipboard API | допустимо только после пунктов 1–5 |
| 10 | ожидает | Terminal visual migration | retained tree и component styling, backend adapter отдельно | не смешивается с process isolation |

Отдельной сильной модели следует оставлять: новый syscall/IPC ABI, scheduler,
process isolation, display service, filesystem persistence, loader/relocations,
native Rust toolchain и GPU command path. OpenCode может реализовать их
последующие узкие adapters только после фиксации контракта и тестового fixture.

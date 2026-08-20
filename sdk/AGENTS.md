# Правила `sdk/`

SDK — публичный путь разработчика, поэтому пример должен быть меньше и понятнее
внутренней реализации системы.

Перед задачей прочитай `docs/SDK_DEVELOPMENT.md`.

- Обычное приложение начинается с `examples/hello`: `fn main`, `std` и safe
  facades. Оно не содержит `_start`, syscall numbers, disk structs или loader.
- Устанавливаемый файл — RUNE. Не учи пользователя запускать ELF intermediate.
- Новая DLL начинает с manifest в `sdk/abi`, затем C-compatible raw API, safe
  Rust wrapper, минимальный provider/consumer example и version mismatch test.
- Пример не получает больше capabilities, чем использует, и понятно сообщает
  об отсутствующей optional capability.
- Документация отделяет cross-hosted сборку от ещё не готового native
  self-hosting и bootstrap GUI от будущего ring-3 UI session.
- Код примера содержит русские пояснения границ, но не копирует internals service.

Проверка приложения:
`bash scripts/build-std.sh build -p rustos-sdk-hello --target targets/x86_64-unknown-rustos.json`.

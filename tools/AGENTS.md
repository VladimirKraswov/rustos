# Правила `tools/`

Tools запускаются на macOS/Linux и создают/проверяют артефакты RustOS. Они не
становятся зависимостями kernel или ring-3 target crates.

- Parse untrusted image/RUNE/RUI/ELF без unchecked offsets и unbounded allocation.
- Команда `verify` не изменяет файл. Запись использует temporary output и
  atomic replace только после полной успешной проверки.
- CLI стабилен для scripts/CI: breaking flag требует migration всех callers и
  документации.
- Повторная сборка из одинакового input детерминирована; timestamps/random IDs
  нормализуются или передаются явно.
- Host path separator/endian/metadata assumptions не попадают в on-disk format.
- Для нового option нужны help text, success test и malformed-input test.

Проверки: unit tests конкретного package, затем `make test-host`; image/layout
изменение дополнительно проверяет build/verify и соответствующий boot-test.

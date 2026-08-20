# Правила `libs/`

Здесь находятся loaders и client DLL facades, а не привилегированные services.

- Наружный DLL ABI описывается manifest из `sdk/abi`: `extern "C"`, `#[repr(C)]`,
  fixed-width values и явное владение. Safe Rust wrapper не экспортирует Rust ABI.
- DLL call после relocation является обычным локальным вызовом. IPC допустим
  внутри client stub системного service, но protocol детали скрываются wrapper.
- Client facade валидирует arguments, дробит streaming операции, корректно
  обрабатывает short read/write и не сохраняет чужой shared-memory pointer.
- Loader сначала проверяет весь container/dependency graph, затем map/relocate;
  при ошибке освобождает staging/mappings и не оставляет writable executable.
- Не дублируй record definitions из `rune-format`/`abi` и filesystem parser из
  `varaniafs`.
- Новая публичная функция требует manifest entry, ABI/version теста, negative
  path и примера вызова из SDK.

Проверки: тест изменённого crate и `make test-host`; loader/VFS facade — также
`make test-boot`.

# Правила `rune-format/`

RUNE — versioned on-disk executable format и привилегированная входная граница.

- Parser работает на bytes без allocator и проверяет multiplication/addition,
  alignment, overlap, architecture, W^X, reserved и record cross-references.
- Не map/execute данные до полной structural/hash/policy проверки.
- Unknown optional record пропускается по размеру; unknown required record даёт
  typed отказ. Старые значения kind/flag не получают новый смысл.
- Изменение layout сопровождается size/offset assertions, golden fixture,
  truncated/overflow/overlap/duplicate негативными fixtures и tool inspect.
- ELF/PE conventions не протекают в публичный RUNE ABI; converter нормализует их
  на host/toolchain boundary.
- Read-only shared regions остаются immutable после relocation и RELRO.

Проверки: crate/tool unit tests, `make test-host`; loader-visible изменение —
`make test-boot`.

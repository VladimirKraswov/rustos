# Правила `system-assets/`

- Assets — versioned immutable packs с bounded metadata, а не Rust-код,
  управляющий desktop/window policy.
- Cursor/icon lookup имеет обязательный fallback и не паникует на неизвестном
  ID, масштабе или теме.
- Hotspot, logical size, pixel dimensions, alpha и format валидируются до
  установки pack.
- Новая тема покрывает полный обязательный набор cursors/icons; частичная тема
  явно наследует base pack.
- Большие bitmap не дублируются в каждом приложении: идентичность ресурса
  включает package/pack/version/scale/theme.
- Generated assets воспроизводимы: исходник/лицензия/команда pack фиксируются.

Проверки: `cargo test -p rustos-system-assets -p rustos-abi`; видимые изменения
assets требуют `make test-gui` и screenshot regression.

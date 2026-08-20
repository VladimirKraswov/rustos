# Правила `system-ui/`

Это renderer-neutral `no_std` component runtime. Он не знает о framebuffer,
оконном сервере, terminal, конкретном package или процессе.

- Один источник истины — `NodeSpec`/`Tree`: Rust builder и `.rui` decoder не
  создают параллельные component models.
- Layout использует логические пиксели и constraints. Стандартный control не
  содержит app-specific координаты или callbacks; он публикует `CommandId`.
- Любая property mutation проходит API runtime и указывает правильную
  invalidation: layout, paint и/или semantics.
- Input сохраняет pointer capture, стабильный focus order и capture/target/
  bubble route. Disabled/hidden controls не активируются.
- Renderer получает display list + clip/damage. Не добавляй framebuffer/font
  dependency и не вызывай rasterizer из component logic.
- Resources адресуются `ResourceId`; runtime не хранит process pointer. Dynamic
  text предоставляет application resource adapter/shared buffer, а не новый
  component kind для каждого приложения.
- Capacity overflow и невалидный IR не публикуют partial tree/frame. NodeId
  остаётся generation-checked после remove/reuse.
- Новый control получает builder API, semantics role/action, keyboard и pointer
  tests, state/theme tests и damage test. Внешний вид без поведения не готов.

Проверки: `cargo test -p rustos-system-ui -p rustos-rui -p rustos-abi`. Если
control применён в desktop, дополнительно `make test-gui`.

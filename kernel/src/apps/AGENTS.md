# Правила bootstrap-приложений

Файлы здесь пока компилируются в kernel, но должны выглядеть как будущие
изолированные приложения.

- Каждый экземпляр окна владеет собственным состоянием. Запрещены mutable
  singleton/static для cwd, input, scrollback, selection или component tree.
- Закрытие означает уничтожение application object; повторный запуск получает
  чистое состояние. Общие только immutable assets и явно системные settings.
- Layout, focus, hit-test и controls идут через `rustos-system-ui`. App получает
  typed commands/events и не содержит таблиц координат для стандартных controls.
- Прямой framebuffer допустим только в маленьком `RenderBackend` adapter. Model,
  commands и lifecycle никогда не рисуют сами.
- App action не выполняет privilege напрямую: он возвращает typed request
  `DesktopSession`/будущему service.
- Не выдавай bootstrap object за ring-3 process. Перенос в user space — отдельная
  задача после стабильного UI/service ABI.

Для Terminal обязательно прочитай `docs/TERMINAL_SYSTEM_UI_MIGRATION.md`.
Образцы component apps: `ui_showcase.rs`, `desktop_settings.rs`, `shell_ui.rs`.
Проверки: kernel target build, `make test-arch`, `make test-gui`.

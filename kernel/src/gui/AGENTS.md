# Правила window/display session

- Window manager управляет геометрией, z-order, focus и lifecycle, но не хранит
  внутреннее состояние приложения.
- `WindowId` стабилен до close и не подменяется индексом slot. Все очереди и
  ссылки обязаны корректно отвергать закрытый/stale ID.
- Move/resize/minimize/maximize/close проходят `WindowCommand`/`WindowEvent`;
  приложение не меняет `ManagedWindow` напрямую.
- Input route выбирает одно верхнее окно, учитывает pointer capture и не должен
  активировать desktop под popup/window.
- Рисование и present используют damage. Полный framebuffer redraw на каждый
  mouse packet или key запрещён.
- Close освобождает application memory; tests должны проверять независимые
  несколько окон и чистое состояние нового экземпляра.

Изменения требуют `make test-gui`; lifecycle или memory ownership — также
`make test-boot`.

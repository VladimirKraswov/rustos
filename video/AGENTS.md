# Правила `video/`

`rustos-video` задаёт переносимые pixel surfaces, composition и damage; он не
управляет PCI/virtio, окнами или приложениями.

- Проверяй pitch, format, dimensions, buffer length и арифметику offsets до
  первого pixel access. Rect clipping обязателен для каждой primitive.
- Горячие циклы не делают division/format conversion на пиксель, если это можно
  вычислить один раз на row/span.
- Alpha/color conversion имеют точные тест-векторы для границ 0/255 и channel
  order. Не предполагай RGB32 для всех scanout modes.
- Damage ограничен surface и bounded; overflow безопасно схлопывается, а не
  теряет область.
- Driver mapping/page flip/fence живут в platform driver, component/UI policy —
  в `system-ui`/window service.
- Оптимизация сопровождается сравнительным correctness test; unsafe SIMD имеет
  scalar fallback и архитектурную границу.

Проверки: `cargo test -p rustos-video -p rustos-gui-check`; интеграция scanout —
`make test-gui`.

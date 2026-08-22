# GPU backend SystemUI

`rustos-ui-gpu` — граница между renderer-neutral деревом компонентов и
изолированным `renderd`. Приложение по-прежнему строит обычный
`rustos-system-ui::Runtime`, но передаёт в `Runtime::render` не framebuffer, а
`GpuRenderBackend`:

```rust
use rustos_system_ui::{Rect, Runtime, Theme, WindowMetrics};
use rustos_ui_gpu::GpuRenderBackend;

let metrics = WindowMetrics::from_physical(1920, 1200, 1500).unwrap();
let mut ui = Runtime::<128, 512, 16>::new(
    Rect::new(0, 0, metrics.logical_width(), metrics.logical_height()),
    Theme::light(),
);
// Построение дерева компонентов выполняется обычным UiBuilder.

let mut gpu = GpuRenderBackend::<512>::new(metrics).unwrap();
ui.render(&mut gpu).unwrap();
for batch in gpu.batches(64).unwrap() {
    // librender.dll передаёт batch renderd вместе с atlas resources.
    submit_to_renderd(batch.instances);
}
# fn submit_to_renderd(_: &[rustos_ui_gpu::GpuUiInstance]) {}
```

Backend переводит logical coordinates в physical до rasterization, сохраняет
clip, радиус, рамку, font flags и resource IDs. Он не содержит CPU pixel loop.
Переполнение budget отклоняет весь кадр: частичный интерфейс не публикуется.

Низкоуровневое преобразование instances в VirGL/Vulkan batches, glyph/image
atlases и отправка `GraphicsBuffer` принадлежат `renderd`; оконная политика и
композиция — `compositord`; modeset/vblank/scanout — `displayd`.


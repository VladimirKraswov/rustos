# Nature wallpaper sources

Фоны сгенерированы встроенным ImageGen для RustOS 20 августа 2026 года и
сохранены вместе с компактными runtime-копиями. В изображениях нет сторонних
логотипов, текста и UI.

Общая часть финального prompt:

```text
Use case: photorealistic-natural
Asset type: 16:9 desktop wallpaper for a modern educational operating system
Style/medium: refined photorealistic natural landscape, realistic textures,
gently cinematic but not fantasy
Composition/framing: wide landscape with calm readable areas for desktop icons
Lighting/mood: soft natural light, comfortable for long-term desktop use
Constraints: no people, buildings, roads, text, logos, watermark or UI;
avoid oversaturation, excessive contrast and busy detail
```

Сюжеты финального набора:

1. `spring-river.png` — весенняя речная долина на рассвете, зелёный луг,
   берёзы и далёкие низкие холмы;
2. `autumn-river.png` — тихая осенняя река, янтарная листва, лёгкий туман и
   далёкие холмы;
3. `winter-field.png` — снежное поле и замёрзшая река, редкие ели и мягкие
   горы под бледно-голубым небом.

`packed/*.rbc1` — 4×4 block-compressed RGB, 1280×720, ровно 460800 байт.
Каждый блок хранит два RGB565 endpoint и 2-bit индекс texel; широкоформатное
разрешение сохраняется до rasterization, а при нецелом масштабе
desktop использует bilinear sampling, а не растягивает низкое изображение
nearest-neighbor. Блочное хранение гарантирует загрузку GRUB/OVMF при
минимальных 128 MiB RAM; framebuffer по-прежнему 24-bit. Файлы
пересобираются `scripts/pack-wallpapers.sh` и непосредственно включаются в
`rustos-system-assets`; PNG остаются полноцветными master assets.

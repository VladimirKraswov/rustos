# Видеосистема RustOS

## Цель и границы

Видеосистема строится не вокруг конкретного firmware framebuffer, а вокруг трёх
разных контрактов:

1. **scanout driver** владеет физическим дисплеем, режимом и публикацией;
2. **surfaces/raster** хранят пиксели приложений и выполняют CPU-операции;
3. **compositor** собирает произвольное число окон и overlays только в damage.

Сейчас scanout предоставляет GRUB Multiboot2 framebuffer: firmware/GRUB
выбирает режим и передаёт linear 32-bit framebuffer, но не VSync, page flip
или аппаратное ускорение. Это bootstrap-драйвер, а не заявление о поддержке
конкретной GPU. В дальнейшем backend можно заменить virtio-gpu или
DRM-подобным драйвером, не меняя surfaces и оконный протокол.

## `rustos-video`

Новый platform-independent `no_std` crate содержит код без MMIO, allocator'а
и привязки к kernel:

- `Scanout`, `DisplayDriver`, `DisplayMode` и mode-set contract для
  firmware/virtio/GPU drivers;
- `Surface`/`SurfaceMut` с явными width, height, stride и pixel format;
- RGB, BGR, ARGB8888, RGB565 и grayscale8 с корректной конвертацией;
- span-based fill и opaque blit с clipping;
- source-over alpha composition с global opacity;
- clipped nearest-neighbour scaling как гарантированный baseline для icons и
  video frames (bilinear/SIMD добавится тем же API);
- `DamageRegion<N>`: bounded tracker без heap, clipping и безопасное
  схлопывание при переполнении;
- `Layer` и `composite`: background плюс любое число поверхностей в z-order.

Все unsafe-операции остаются на границе scanout/back-buffer mapping. Raster и
compositor получают обычные slices и тестируются на Linux/macOS как обычная
Rust-библиотека. Сейчас есть отдельные тесты geometry, pixel conversion,
clipped blit, alpha, damage overflow и многослойной композиции.

## Горячий путь

Framebuffer использует два кэшируемых RAM-слоя: composited back buffer и статический
desktop. `slice::fill` и построчный `copy_from_slice` позволяют LLVM применять
wide stores вместо проверки bounds на каждом пикселе. Scanout получает только
готовые damage rectangles. Полный кадр с одинаковым stride публикуется одним
последовательным copy, а не строками или отдельными volatile pixel stores.
Видимый framebuffer не используется как рабочая поверхность и не читается
обратно.

Физический pixel slot — XRGB/BGRX8888: 24 значащих бита цвета и один padding
byte. Packed RGB888 не используется из-за плохого выравнивания. Команды
`DISPLAY COLOR TRUECOLOR`, `RGB565` и `GRAY8` переключают логический формат
software surface; present конвертирует его в выровненный scanout.

Drag в TCG работает в preview-режиме. На первом движении старое окно один раз
удаляется из scanout, затем перемещаются только title bar и тонкий контур.
Полное содержимое публикуется один раз на mouse-up. PS/2-пакеты с неизменными
кнопками дополнительно объединяются, поэтому compositor не догоняет очередь
устаревших координат. При нехватке непрерывной RAM сохраняется корректный, но
медленный full-redraw fallback.

## Много окон и изоляция

`Layer` не ограничивает число окон константой compositor'а: caller передаёт
slice видимых layers. Bounded только список damage rectangles, а его
переполнение никогда не теряет картинку — области объединяются в более дорогой
bounding rectangle.

Следующий шаг изоляции — user-space `displayd`. Приложение получит shared-memory
surface capability и право отправлять commit/damage; только displayd получит
scanout capability. Падение клиента удалит его layers, но не остановит desktop.
Координаты, размеры и buffer generation должны проверяться до отображения.

## Software OpenGL и видео

Software OpenGL должен рендерить не в firmware framebuffer, а в ARGB/RGB surface приложения.
SwapBuffers станет surface commit: compositor заберёт последний полностью
готовый buffer и отбросит устаревшие кадры. Начальный практичный путь — API
совместимости OpenGL поверх software rasterizer/softpipe; аппаратный backend
позже реализует тот же контракт buffers/fences.

Для видео нужен тот же путь:

1. decoder пишет очередной RGB/ARGB surface (позже — YUV planes);
2. presentation queue хранит PTS и максимум несколько кадров;
3. compositor выбирает свежий готовый кадр по monotonic clock;
4. scale/color conversion выполняется CPU backend'ом, затем SIMD или GPU.

Это исключает копирование видеокадров через IPC messages: передаются handles,
damage и fence, а сами пиксели остаются в shared memory.

## Следующие аппаратные этапы

- frame pacing и monotonic presentation clock;
- front/back/triple-buffer protocol с generation и fences;
- SSE2/AVX2 dispatch для blend, scale и RGB/YUV conversion;
- virtio-gpu 2D scanout/page flip как первая независимая display-служба;
- native EDID/mode enumeration и несколько мониторов;
- перенос compositor/input/terminal из kernel bootstrap в ring 3;
- software OpenGL compatibility layer, затем порт Mesa по частям.

Текущих гарантий пока недостаточно для плавного видео: firmware framebuffer не имеет VSync, а
compositor синхронный. Но surfaces, alpha, damage и layer ABI уже отделены от
этого ограничения и не потребуют переписывания при появлении нового драйвера.

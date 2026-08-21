# Видеосистема RustOS

## Цель и границы

Видеосистема строится не вокруг конкретного firmware framebuffer, а вокруг трёх
разных контрактов:

1. **scanout driver** владеет физическим дисплеем, режимом и публикацией;
2. **surfaces/raster** хранят пиксели приложений и выполняют CPU-операции;
3. **compositor** собирает произвольное число окон и overlays только в damage.

Сейчас основной scanout — собственный драйвер **virtio-gpu 2D** поверх
modern virtio-pci. Он получает EDID, перечисляет широкоформатные
режимы, создаёт 32-bit BGRX resource и меняет scanout без
перезапуска. Закрытые GPU API, VirGL и host OpenGL не нужны:
вся отрисовка по-прежнему выполняется CPU.

GRUB Multiboot2 framebuffer остался надёжным fallback. Если virtio-gpu нет
или transport не прошёл negotiation, kernel продолжает работу с
firmware linear framebuffer. В этом режиме смена разрешения честно
возвращает `RequiresReboot`: уже переданный firmware framebuffer не
имеет runtime mode-set API.

Host display frontend не является частью scanout contract. На macOS
`scripts/run.sh` выбирает guest mode, который целое число раз помещается в
Cocoa backing surface, включает fullscreen и отключает zoom interpolation.
Так интерфейс остаётся крупным без дробной фильтрации готового framebuffer.
Policy `actual` сохраняет строгое окно 1:1 для pixel-level диагностики, а
обычный дробный `fit` доступен только как явный выбор. Настоящий HiDPI внутри ОС
выполняется до rasterization через `WindowMetrics`; scanout и compositor
по-прежнему получают physical surface и публикуют её `1:1`.

## Virtio-gpu 2D

Драйвер разделён на независимые уровни:

- `display/pci.rs` находит PCI function `1af4:1050`, BAR и vendor
  capabilities common/notify/ISR/device;
- `display/virtqueue.rs` выполняет Virtio 1.x feature negotiation и
  обслуживает bounded split control queue. На bootstrap-этапе в очереди
  не больше одной команды, completion ожидается polling'ом с
  timeout;
- `display/edid.rs` проверяет signature/checksum EDID 1.x и извлекает
  preferred/standard timings и физический размер монитора;
- `display/virtio_gpu.rs` реализует `GET_DISPLAY_INFO`, `GET_EDID`,
  `RESOURCE_CREATE_2D`, `ATTACH_BACKING`, `SET_SCANOUT`,
  `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`, `DETACH_BACKING` и `UNREF`.

При present драйвер копирует из software surface только damage rectangles,
после чего отправляет transfer + flush для тех же областей.
Гостевой backing сейчас физически непрерывен: это упрощает учебный DMA
путь, хотя protocol уже допускает scatter/gather entries.

### Атомарная смена режима

`DISPLAY MODE WxH` не меняет часть состояния. Операция идёт
как транзакция:

1. проверяется, что режим есть в bounded mode list;
2. выделяются новые back buffer и, если хватает памяти, desktop cache;
3. создаётся и привязывается новый virtio resource;
4. только после подтверждённого `SET_SCANOUT` старые backing и RAM-слои
   освобождаются;
5. window manager сбрасывает cursor snapshot, пересчитывает geometry и
   целиком перерисовывает сцену.

При ошибке памяти или устройства старый режим остаётся активным.
Минимальные 128 МиБ достаточны для тестовых 1280×800/1280×720; для 4K
нужен запас под несколько 32-МиБ буферов.

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
- IRQ-driven virtqueue, page flip/fences и hardware cursor virtio-gpu;
- PCI bridge enumeration, IOMMU/DMA isolation и несколько мониторов;
- native KMS-подобные драйверы для конкретного железа;
- перенос compositor/input/terminal из kernel bootstrap в ring 3;
- software OpenGL compatibility layer, затем порт Mesa по частям.

Текущих гарантий пока недостаточно для плавного видео: virtio-gpu backend
синхронно ожидает transfer/flush и не имеет VSync event. Но surfaces,
alpha, damage и layer ABI уже отделены от этого ограничения и не потребуют
переписывания при появлении asynchronous или native backend.

## Спецификации

- [OASIS Virtio 1.2, GPU Device](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-3720007)
  — normative transport, control queue, 2D resources, EDID и scanout commands;
- [QEMU virtio-gpu](https://www.qemu.org/docs/master/system/devices/virtio-gpu.html)
  — доступные QEMU backends и соотношение 2D/virgl/rutabaga.

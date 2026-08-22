# Полное GPU-ускорение RustOS

Этот документ задаёт не рекламную формулировку, а проверяемый маршрут от
bootstrap CPU desktop к постоянно работающему GPU desktop. Наличие
`virtio-gpu`, VirGL или одного аппаратно нарисованного треугольника само по
себе не означает, что SystemUI ускорен.

## Определение готовности

Обычный интерактивный сеанс считается GPU-ускоренным только одновременно при
выполнении всех условий:

- системные render providers публикуют отдельные `GraphicsBuffer` для desktop,
  chrome окон и content каждого приложения; Terminal, Проводник и сторонние
  программы при этом используют только публичный Window/SystemUI/Canvas API;
- геометрия компонентов растеризуется GPU, текст берётся из glyph atlas,
  изображения и иконки — из texture atlas;
- `compositord` смешивает client buffers непосредственно в один из трёх
  scanout buffers и не вызывает `TRANSFER_FROM_HOST_3D`/CPU readback;
- перемещение неизменившегося окна меняет только transform слоя и damage, не
  перерисовывает содержимое окна и не копирует его pixels CPU;
- cursor использует hardware plane, когда он доступен, и не инвалидирует
  desktop surface;
- одновременно могут быть подготовлены два-три кадра, но mailbox отбрасывает
  устаревший кадр до композиции;
- при потере GPU перезапускаются `renderd`/`compositord`/`displayd`, а
  ограниченный CPU recovery desktop остаётся доступен;
- serial-метрики и интеграционный тест подтверждают `readback=0`,
  `cpu-raster=0` и ненулевое число GPU batches именно для обычного desktop,
  а не только для специального demo image.

CPU разрешено использовать для layout, формирования вершин, shaping текста,
обновления atlas и software fallback. Это не rasterization кадра: CPU не
обходит каждый pixel видимого окна и не смешивает слои.

## Владение и границы процессов

```text
inputd ── input events ───────────────────────────────┐
                                                     v
application ──► window.dll / system-ui.dll / graphics.dll
    │              │ typed state, events, UI IR, Canvas/OpenGL calls
    │              v
    │        windowd / uid / renderd
    │              ├─ выбранный системой GPU или software backend
    │              ├─ glyph/icon atlases и GPU command batches
    │              └─ per-window GraphicsBuffer[3]
    │                              │ internal surface protocol
    │                              v
    └──────────────────────► compositord
                                      z-order / clip / transform / occlusion
                                                        │
                                      scanout GraphicsBuffer[3] + timeline
                                                        v
                                                  displayd
                                      modeset / planes / vblank / present
                                                        │
                                                        v
                                            virtio-gpu / native GPU
```

Ядро отвечает только за capabilities, память, DMA/IOMMU, IRQ, timeline,
изоляцию GPU context и reset. Оконная политика, rasterizer, shader cache,
atlas и fallback policy не входят в TCB.

## Единый renderer API

`rustos-system-ui` продолжает формировать renderer-neutral display list внутри
системного UI runtime. Это не публичный формат приложения и не часть SDK.
Реализации backend имеют одинаковую семантику:

- `GpuRenderBackend` — основной backend обычного сеанса;
- `SoftwareRenderBackend` — fallback и эталон для pixel-diff тестов;
- `HeadlessRenderBackend` — semantics/layout тесты без pixels.

Приложение не создаёт surface, не получает `GraphicsBuffer` и не строит GPU
display list. Оно создаёт `Window`, подключает дерево компонентов либо
инициализирует `Canvas2D`/`Canvas3D` в части окна. System UI runtime сам строит
display list, выбирает backend, владеет buffer queue и делает commit. Выбор
VirGL/Venus/V3D/software и восстановление принадлежат графическим сервисам.

`Canvas3D` позволяет выбрать программный стандарт (`OpenGL`, `OpenGL ES`, в
будущем Vulkan), но не устройство и не способ исполнения. Один и тот же RUNE
binary получает аппаратную Mesa при доступном GPU и software Mesa при fallback.

## Формат GPU-кадра SystemUI

Display list компилируется в bounded batches, а не в один безразмерный массив:

1. непрозрачные прямоугольники и простые границы объединяются по pipeline;
2. rounded rectangle, border и shadow используют аналитический fragment
   shader, поэтому радиус не превращается в сотни маленьких CPU-отрезков;
3. glyph quad ссылается на общий SDF atlas; bootstrap VirGL хранит
   premultiplied color в ключе tile, а следующий R8 shader backend отделит
   distance от tint без изменения wire primitive;
4. icon/image quad ссылается на immutable RGBA atlas;
5. clip превращается в scissor, одинаковые соседние scissor объединяются;
6. переполнение batch завершает его целиком и начинает следующий — частично
   записанная primitive никогда не публикуется;
7. damage отсекает primitives до tessellation, а не после rasterization.

Atlases обновляются редкими fenced uploads. В steady state hover, drag и
анимация не загружают полную текстуру и не пересоздают pipeline objects.

## Surface queues и композиция

Каждое окно владеет очередью из трёх buffers. Состояния одного slot:

```text
Free -> Rendering -> Ready(acquire) -> Submitted -> Displayed -> Free(release)
```

Клиент не пишет `Submitted`/`Displayed` buffer. Compositor не читает buffer до
acquire point. Resize создаёт новую generation очереди; старая generation
освобождается только после release всех её buffers. Close отзывает surface,
но не освобождает DMA-память до завершения последнего GPU fence.

`compositord` хранит только newest ready commit при `MAILBOX`. Перед кадром он
строит видимый region сверху вниз, полностью закрытые слои не рисует, а для
непрозрачных fullscreen слоёв может выбрать direct scanout. Обычный desktop
композируется в premultiplied-alpha B8G8R8A8/sRGB; преобразование color space
выполняется в финальном pass.

## Этапы перевода

### G1. Производственный render contract

- увеличить демонстрационный лимит команд до нескольких bounded batches;
- добавить sampled textures, scissor, premultiplied alpha и blit/copy;
- хранить resource ownership в GPU context, валидировать handles и размеры;
- поддержать не менее трёх submissions in-flight без синхронного flush.

Готовность: host-тесты malformed streams/resource rights и аппаратный тест,
который смешивает три GPU surfaces в четвёртый без readback.

### G2. GPU backend SystemUI

- geometry batcher и аналитические shaders;
- системный glyph atlas с Latin/Cyrillic, размерами и начертаниями;
- icon/image/wallpaper atlases;
- damage/scissor и renderer-independent pixel-diff tests.

Готовность: библиотека компонентов рисуется GPU backend и совпадает с
эталонным software screenshot в установленном допуске.

### G3. Внутренние оконные surfaces

- `surface_create/commit/destroy`, generation и release feedback в
  `compositord`;
- отдельный triple-buffer queue каждого окна;
- resize без растягивания старого bitmap и close без сохранения процесса;
- shared read-only страницы библиотек не заменяют владение pixels;
- surface API остаётся внутренним контрактом `uid/renderd/compositord` и не
  появляется в обычном application SDK.

Готовность: восемь независимых окон непрерывно обновляются; падение одного
клиента освобождает только его buffers.

### G4. GPU compositor

- z-order, clip, transforms, opacity, occlusion и damage;
- один atomic commit на refresh, triple buffering и mailbox;
- hardware cursor plane, direct scanout и overlay fast paths;
- полное удаление windowed `download_render_target`.

Готовность: drag неизменившегося окна публикует transform-only frame,
`readback=0`, CPU pixel counter не растёт.

### G5. Переключение системных providers без миграции приложений

Меняется реализация под публичным API, а не код Terminal/Проводника:

1. `windowd` переносит decorations, desktop, Start и taskbar из bootstrap;
2. `uid` начинает исполнять тот же component tree через GPU backend;
3. `graphics.dll` связывает Canvas2D/Canvas3D с renderd и Mesa;
4. Terminal, Проводник и Settings запускаются без изменения UI-кода;
5. тот же application binary проверяется с GPU и принудительным software
   backend;
6. kernel GUI остаётся только recovery console, а обычная загрузка больше не
   создаёт `DesktopSession` в kernel.

Если смена renderer требует исправлять приложение, этап считается
архитектурно проваленным.

### G6. Mesa и несколько GPU backend

- upstream Mesa VirGL + EGL/OpenGL/OpenGL ES;
- Venus/Vulkan как основной современный API виртуальной машины;
- `libdrm-rustos`/winsys поверх GraphicsBuffer и SyncTimeline;
- V3D для Raspberry Pi как первый native ARM backend;
- software renderer с тем же surface ABI при неизвестном устройстве.

Смена backend не меняет SystemUI и приложения.

## Производительность и наблюдаемость

Release-сборка на профиле UTM Apple Silicon должна держать refresh cadence без
накопления очереди при 1280×800 и 1920×1200. В serial раз в секунду, а не на
каждый кадр, публикуются:

```text
[graphics-perf] backend=virgl-metal fps=60 gpu-batches=... primitives=...
                uploads-kib=... readback=0 cpu-raster=0 dropped=...
```

Отдельно считаются время layout, encode, GPU fence, composition и present.
Среднее без p95/p99 не считается достаточной диагностикой рывков.

## Обязательные проверки

- unit: batch overflow atomicity, clipping, atlas eviction, surface state;
- property/fuzz: ABI records, command framing и damage arithmetic;
- pixel-diff: CPU reference против GPU для controls, text и icons;
- lifecycle: resize/close/crash/device reset без use-after-free и frame leak;
- stress: 16 окон, hover storm, быстрый drag, 10 000 resize/close cycles;
- UTM: HVF + VirGL/ANGLE/Metal, реальные input и screenshot;
- QEMU Linux: VirGL hardware path и отдельный llvmpipe fallback job;
- обе ISA: `make test-arch`, даже если host GPU transport различается.

Специальный triangle/Aurora test остаётся диагностикой 3D transport, но больше
не служит доказательством ускорения desktop.

## Текущее состояние реализации

- G1: capability-checked sampled textures, damage scissor, premultiplied alpha,
  bounded command validation, большие разбиваемые batches и несколько
  submissions in-flight реализованы;
- G2: обычный SystemUI компилируется в renderer-neutral GPU scene и передаётся
  постоянному ring-3 `renderd`. VirGL выполняет blending и запись geometry,
  icon/image и wallpaper pixels; CPU backend остаётся recovery fallback.
  Единый `rustos-system-fonts` формирует одинаковые Latin/Cyrillic metrics для
  обоих backend. GPU display list передаёт один semantic glyph primitive, а
  `renderd` лениво строит постоянный 2048×2048 SDF atlas для family, weight,
  italic, size и color без повторной загрузки glyph в steady state;
- G3: общая generation-checked `SurfaceQueue`, динамические IPC endpoint'ы и
  клиентская `surface.dll` реализованы; отдельный ring-3 процесс проходит
  `create/commit/direct-scanout/release/feedback/destroy`, а stale event
  capability после crash отзывается ядром. Bootstrap SystemUI stream теперь
  также содержит независимые surfaces desktop, каждого окна и popup overlay;
- `renderd` кэширует surfaces по устойчивому `layer.id + content_hash`,
  растеризует только изменившийся слой и одним GPU pass смешивает их в
  triple-buffered zero-copy scanout. При drag ядро повторно не обходит UI:
  меняются только `x/y` layer descriptor и checksum нового кадра;
- resize/close уничтожают VirGL surface object и device-local resource только
  после bounded fence drain. Аппаратный cursor plane остаётся независимым от
  damage. Проверочный marker содержит
  `raster=gpu composition=gpu readback=0 cpu-pixels=0 layers=N`.

Следующая граница — перенести ownership/policy слоёв из bootstrap kernel GUI в
постоянные `windowd`/`uid` и связать каждый ring-3 application commit с уже
готовым внутренним layer cache. Публичный Window/SystemUI API не меняется.

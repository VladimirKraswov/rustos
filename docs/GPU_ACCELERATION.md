# Полное GPU-ускорение RustOS

Этот документ задаёт не рекламную формулировку, а проверяемый маршрут от
bootstrap CPU desktop к постоянно работающему GPU desktop. Наличие
`virtio-gpu`, VirGL или одного аппаратно нарисованного треугольника само по
себе не означает, что SystemUI ускорен.

## Определение готовности

Обычный интерактивный сеанс считается GPU-ускоренным только одновременно при
выполнении всех условий:

- desktop, chrome окон, SystemUI, terminal и Проводник публикуют собственные
  `GraphicsBuffer`, а не рисуют в общий kernel framebuffer;
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
application ── SystemUI display list ──► librender.dll / renderd
    │                                      │
    │                                      ├─ GPU command batches
    │                                      ├─ glyph/icon atlases
    │                                      └─ per-window GraphicsBuffer[3]
    │                                                   │
    └─ surface commit + damage + acquire timeline ──────┤
                                                        v
                                                compositord
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

`rustos-system-ui` продолжает формировать renderer-neutral display list.
Реализации backend имеют одинаковую семантику:

- `GpuRenderBackend` — основной backend обычного сеанса;
- `SoftwareRenderBackend` — fallback и эталон для pixel-diff тестов;
- `HeadlessRenderBackend` — semantics/layout тесты без pixels.

Приложение не выбирает VirGL, Venus, V3D или software renderer. Оно создаёт
surface, получает metrics, строит display list и делает commit. Выбор device и
восстановление принадлежат графическим сервисам.

## Формат GPU-кадра SystemUI

Display list компилируется в bounded batches, а не в один безразмерный массив:

1. непрозрачные прямоугольники и простые границы объединяются по pipeline;
2. rounded rectangle, border и shadow используют аналитический fragment
   shader, поэтому радиус не превращается в сотни маленьких CPU-отрезков;
3. glyph quad ссылается на общий R8/SDF atlas и получает цвет отдельно;
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

### G3. Настоящие оконные surfaces

- `surface_create/commit/destroy`, generation и release feedback в
  `compositord`;
- отдельный triple-buffer queue каждого окна;
- resize без растягивания старого bitmap и close без сохранения процесса;
- shared read-only страницы библиотек не заменяют владение pixels.

Готовность: восемь независимых окон непрерывно обновляются; падение одного
клиента освобождает только его buffers.

### G4. GPU compositor

- z-order, clip, transforms, opacity, occlusion и damage;
- один atomic commit на refresh, triple buffering и mailbox;
- hardware cursor plane, direct scanout и overlay fast paths;
- полное удаление windowed `download_render_target`.

Готовность: drag неизменившегося окна публикует transform-only frame,
`readback=0`, CPU pixel counter не растёт.

### G5. Миграция программ

Порядок выбран так, чтобы каждый шаг оставлял рабочую систему:

1. desktop/chrome/start/taskbar;
2. библиотека компонентов;
3. terminal;
4. Проводник;
5. параметры рабочего стола;
6. Aurora 3D и последующие приложения.

После миграции kernel GUI остаётся только recovery console. Обычная загрузка
не создаёт `DesktopSession` в kernel.

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

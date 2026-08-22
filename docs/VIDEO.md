# Видеосистема RustOS

Архитектурные границы современного display/render/media stack зафиксированы в
[ADR-0001](adr/0001-modern-graphics-architecture.md). Текущий CPU/virtio 2D
путь остаётся только bootstrap и аварийным backend'ом; новые процессы не
получают его slices или физический framebuffer.

## Цель и границы

Видеосистема строится не вокруг конкретного firmware framebuffer, а вокруг трёх
разных контрактов:

1. **scanout driver** владеет физическим дисплеем, режимом и публикацией;
2. **surfaces/raster** хранят пиксели приложений и выполняют CPU-операции;
3. **compositor** собирает произвольное число окон и overlays только в damage.

Сейчас основной scanout — собственный драйвер **virtio-gpu** поверх
modern virtio-pci на AMD64 и modern virtio-mmio на AArch64. Он получает EDID,
перечисляет широкоформатные
режимы, создаёт 32-bit BGRX resource и меняет scanout без перезапуска.
Гарантированный fallback остаётся 2D/CPU, а при feature
`VIRTIO_GPU_F_VIRGL` изолированный ring-3 `renderd` выполняет 3D-команды и
передаёт GPU-only `GraphicsBuffer` на тот же scanout без CPU-копии.

GRUB Multiboot2 framebuffer остался надёжным fallback. Если virtio-gpu нет
или transport не прошёл negotiation, kernel продолжает работу с
firmware linear framebuffer. В этом режиме смена разрешения честно
возвращает `RequiresReboot`: уже переданный firmware framebuffer не
имеет runtime mode-set API.

Выбор выполняется автоматически и fail-closed. На AMD64 enumerator проверяет
все функции PCI bus 0, на AArch64 — все стандартные modern virtio-mmio slots
QEMU `virt`. Драйвер принимается только после Virtio 1.x reset, feature
negotiation, `FEATURES_OK`, настройки controlq и ответа display protocol.
Действительный EDID preferred timing становится рекомендованным режимом. Если
VirGL feature и capset не подтверждены одновременно, `renderd` не получает
GPU capability и SystemUI остаётся на CPU renderer — наличие одного
virtio-gpu 2D устройства не выдаётся за 3D-ускорение.

Boot-report фиксирует принятое решение одной машинно-проверяемой строкой:

```text
[hardware] display-driver=virtio-gpu transport=modern-mmio mode=1440x900 preferred=2880x1800 edid=valid outputs=1 renderer=virgl
```

При отсутствии устройства строка содержит причину и выбранный `uefi-gop`,
`grub-fb` либо честный headless fallback. Timeout/`DEVICE_NEEDS_RESET`/`FAILED`
навсегда закрывает текущую virtqueue до контролируемого reset: descriptor не
переиспользуется, пока устройство ещё может писать в старый DMA buffer.

Host display frontend не является частью scanout contract. На macOS
`scripts/run.sh` выбирает guest mode, который целое число раз помещается в
Cocoa backing surface, включает fullscreen и отключает zoom interpolation.
Так интерфейс остаётся крупным без дробной фильтрации готового framebuffer.
Policy `actual` сохраняет строгое окно 1:1 для pixel-level диагностики, а
обычный дробный `fit` доступен только как явный выбор. Настоящий HiDPI внутри ОС
выполняется до rasterization через `WindowMetrics`; scanout и compositor
по-прежнему получают physical surface и публикуют её `1:1`.

Pure policy `rustos-video::select_startup_mode` сначала сохраняет native mode,
если он укладывается в UI budget, затем ищет точную целую долю 2x…6x. Например,
2880×1800 выбирает 1440×900, а 2048×1280 — 1024×640. Если целая доля не
помещается, обе оси уменьшаются с сохранением пропорций; независимый clamp,
искажавший ultra-wide картинку, запрещён host-тестами. Ручной mode-set при
этом по-прежнему видит полный список до 4K.

## Virtio-gpu 2D и VirGL

Драйвер разделён на независимые уровни:

- `display/pci.rs` находит PCI function `1af4:1050`, BAR и vendor
  capabilities common/notify/ISR/device;
- `display/virtqueue.rs` выполняет Virtio 1.x feature negotiation и
  обслуживает bounded split control queue с четырьмя независимыми DMA slots;
  bootstrap-команды имеют bounded polling, 3D submit завершается асинхронно
  через fence/timeline;
- `display/virtqueue_mmio.rs` предоставляет тому же GPU protocol modern MMIO
  transport QEMU ARM `virt`; scanout/mode-set код не дублируется;
- `display/edid.rs` проверяет signature/checksum EDID 1.x и извлекает
  preferred/standard timings и физический размер монитора;
- `display/virtio_gpu.rs` реализует `GET_DISPLAY_INFO`, `GET_EDID`,
  `RESOURCE_CREATE_2D`, `ATTACH_BACKING`, `SET_SCANOUT`,
  `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`, `DETACH_BACKING`, `UNREF`, а также
  classic VirGL contexts, 3D resources и fenced `SUBMIT_3D`.

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
- локальные `CpuSurface`/`CpuSurfaceMut` с явными width, height, stride и
  `CpuPixelFormat` для software fallback;
- `GraphicsBufferDesc` с capability ownership, четырьмя planes, byte strides,
  offsets, format modifier, usage/memory domains и color metadata;
- surface buffer queue ABI: immutable commit, acquire/release timeline points,
  bounded shared-memory damage и presentation feedback;
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
startup mode policy, clipped blit, alpha, damage overflow и многослойной
композиции.

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

Во время drag окно продолжает отрисовываться полностью. На mouse-down его
готовый слой один раз сохраняется в отдельный bounded buffer; каждый пакет
движения восстанавливает только старый damage из desktop cache и копирует этот
слой в новое положение. Содержимое окна не растеризуется заново. Пакеты xHCI
USB HID mouse (а также PS/2/virtio fallback) с неизменными кнопками
объединяются, поэтому compositor не догоняет очередь устаревших координат. При
нехватке непрерывной RAM сохраняется корректный, но медленный full-redraw
fallback.

## Graphics buffers и явная синхронизация

`CpuSurface` никогда не пересекает границу процесса. Общий контракт находится
в `rustos-abi` и повторно экспортируется `rustos-video::buffer` и
`rustos-video::protocol`:

```text
client renderer
      | GraphicsBuffer capability + acquire SyncPoint
      v
SurfaceCommit --> compositord --> displayd
      ^                  |
      | BufferReleased   `--> PresentationFeedback
      +--- release SyncPoint
```

Версия descriptor'а проверяется до mapping. Неизвестные format/usage/domain
коды и ненулевые reserved fields отклоняются. Linear planes проверяются на
переполнение, недостаточный stride, выход за memory object и перекрытие.
Поддержаны packed RGB, 10-bit/float RGB и multi-plane NV12/P010/YUV420, поэтому
video decoder не обязан предварительно превращать каждый кадр в RGB.

Timeline point `NONE` означает уже завершённую зависимость; частично
заполненная пустая point запрещена. `SyncWaitMany` атомарно ждёт `ALL`/`ANY`
из bounded shared-memory массива, не заставляя CPU spin'иться. Surface commit
несёт monotonically increasing `frame_id`, режим
FIFO/mailbox/immediate/adaptive, logical/physical
size, fractional scale и target presentation time. ABI проверяет
`physical = ceil(logical × scale)`, поэтому растягивание уже растрированного
bitmap нельзя случайно выдать за HiDPI. Kernel objects уже реализуют
generation-safe lifetime, отдельные capability/mapping references и
блокирующий wait/wait-many. Постоянный ring-3 `displayd` один получает
непередаваемую scanout capability, а `renderd` — отдельную render capability;
они обмениваются только buffers/fences через `compositord`. Подробности и
граница оценочного vblank описаны в [GRAPHICS_ABI.md](GRAPHICS_ABI.md), а
3D-путь — в [GPU_RENDERING.md](GPU_RENDERING.md).

## Много окон и изоляция

`Layer` не ограничивает число окон константой compositor'а: caller передаёт
slice видимых layers. Bounded только список damage rectangles, а его
переполнение никогда не теряет картинку — области объединяются в более дорогой
bounding rectangle.

Эксклюзивная scanout capability, atomic full-frame commit, estimated vblank и
supervisor restart уже работают. Следующий шаг изоляции — подключить к
постоянному `compositord` независимые surface queues окон и заменить один
full-frame buffer на bounded multi-buffer queue с damage. Bootstrap kernel
desktop останется только аварийным fallback. Падение клиента должно удалять
только его buffer queue и layers, не останавливая desktop.

## OpenGL и видео

OpenGL должен рендерить не в firmware framebuffer, а в GraphicsBuffer
приложения. SwapBuffers станет surface commit: compositor заберёт последний
полностью готовый buffer и отбросит устаревшие кадры. Aurora 3D/Mesa milestone
уже проходит через ring-3 renderer без guest CPU rasterization. Следующий
практичный путь — Mesa Gallium VirGL winsys; software rasterizer остаётся
fallback для устройств без 3D.

Для видео нужен тот же путь:

1. decoder пишет очередной RGB/ARGB surface (позже — YUV planes);
2. presentation queue хранит PTS и максимум несколько кадров;
3. compositor выбирает свежий готовый кадр по monotonic clock;
4. scale/color conversion выполняется CPU backend'ом, затем SIMD или GPU.

Это исключает копирование видеокадров через IPC messages: передаются handles,
damage и fence, а сами пиксели остаются в shared memory.

## Следующие аппаратные этапы

### Физическая ARM-цель: Raspberry Pi 4

Первой поддерживаемой реальной платой выбрана Raspberry Pi 4 / BCM2711:
четыре Cortex-A72 и VideoCore VI (V3D 4.2). Важно не называть это одним
«видеодрайвером»: display controller и render GPU — разные устройства.

Порт строится четырьмя независимо проверяемыми слоями:

1. board support читает DT и выдаёт capabilities только на разрешённые MMIO,
   IRQ, clocks/resets и DMA ranges;
2. `vc4-displayd` управляет HVS/pixel valves/HDMI, EDID, modeset, planes и
   page flip; compositor не получает прямого MMIO;
3. `v3d-renderd` владеет V3D MMU, buffer objects, command queues, interrupts,
   fences, hang reset и проверкой submission;
4. user-space Mesa V3D/V3DV порт использует стабильный RustOS render ABI;
   OpenGL/EGL не встраиваются в kernel и падение renderer не валит desktop.

Начинать с абстрактной «Mali» хуже: Utgard, Midgard/Bifrost и Valhall требуют
разных Lima/Panfrost/Panthor стеков и конкретной SoC display pipeline. Pi 4
даёт одну воспроизводимую плату и зрелый открытый upstream VC4/V3D stack.

Ссылки для реализации: [официальные характеристики BCM2711](https://www.raspberrypi.com/documentation/computers/processors.html),
[Linux V3D driver](https://docs.kernel.org/gpu/v3d.html),
[Mesa V3D/V3DV](https://docs.mesa3d.org/drivers/v3d.html) и
[Linux VC4 display driver](https://docs.kernel.org/gpu/vc4.html).

- frame pacing и monotonic presentation clock;
- front/back/triple-buffer protocol с generation и fences;
- SSE2/AVX2 dispatch для blend, scale и RGB/YUV conversion;
- MSI-X/IOAPIC interrupt-domain для PCI, page flip и hardware cursor
  virtio-gpu; AArch64 MMIO controlq уже завершает fenced кадры через GICv3 SPI,
  сохраняя timer-poll как bounded аварийную страховку;
- PCI bridge enumeration, IOMMU/DMA isolation и несколько мониторов;
- native KMS-подобные драйверы для конкретного железа;
- перенос compositor/input/terminal из kernel bootstrap в ring 3;
- Mesa Gallium VirGL winsys, затем native V3D backend для Raspberry Pi 4.

Текущих гарантий пока недостаточно для плавного видео: CPU fallback
синхронно ожидает transfer/flush, а Virtio GPU не даёт точный VSync event.
Render submissions при этом уже допускают три независимых кадра in-flight;
на ARM completion приходит аппаратным IRQ и не ждёт следующего timer tick.
Но surfaces,
alpha, damage и layer ABI уже отделены от этого ограничения и не потребуют
переписывания при появлении asynchronous или native backend.

## Спецификации

- [OASIS Virtio 1.3, GPU Device](https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html)
  — normative transport, control queue, 2D resources, EDID и scanout commands;
- [QEMU virtio-gpu](https://www.qemu.org/docs/master/system/devices/virtio-gpu.html)
  — доступные QEMU backends и соотношение 2D/virgl/rutabaga.

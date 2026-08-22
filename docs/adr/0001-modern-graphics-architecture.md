# ADR-0001: современная графическая архитектура RustOS

- Статус: принято
- Дата: 2026-08-21
- Область: display, 2D/3D rendering, media и графические драйверы

## Контекст

Первоначальный графический путь RustOS был надёжным CPU bootstrap: ядро
собирало единый back buffer и публиковало damage через virtio-gpu 2D либо
firmware framebuffer. Сейчас штатный desktop уже использует изолированные
`renderd`/`compositord`/`displayd`, retained GPU surfaces, асинхронные fences и
zero-copy scanout; старый путь сохранён только для загрузки, диагностики и
аварийного режима без VirGL.

Расширять монолитный kernel `Framebuffer` до OpenGL/Vulkan нельзя: это смешает
политику окон, display modeset, выполнение недоверенных GPU-команд и аварийный
вывод в одном привилегированном компоненте.

## Решение

RustOS разделяет графику на три независимых системных направления:

1. `displayd` управляет outputs, EDID, режимами, planes, cursor, vblank и
   атомарной публикацией;
2. render services управляют GPU contexts, виртуальными адресами, очередями,
   buffers, submissions и восстановлением GPU;
3. `mediad` управляет decode/encode sessions, YUV buffers и временем показа.

Общей границей этих подсистем становятся capability handles на графические
буферы и явные timeline sync points. Пиксели не копируются через IPC. Клиент
публикует buffer handle, acquire point, damage и параметры кадра; compositor
возвращает release point и presentation feedback.

Дополнительно фиксируются следующие решения:

- Vulkan — основной стандартный низкоуровневый 3D/compute API;
- Mesa — основной userspace graphics stack;
- OpenGL, OpenGL ES и EGL предоставляются через Mesa как совместимый API;
- SystemUI остаётся renderer-independent нативным UI/2D API;
- premultiplied alpha становится каноническим режимом новых compositor
  buffers, но bootstrap CPU adapter сохраняет прежнюю семантику;
- software renderer и firmware framebuffer остаются обязательным fallback;
- драйверы по возможности работают в ring 3 и получают только выданные им
  MMIO, IRQ, DMA/IOMMU и device capabilities;
- display, render и media capabilities нельзя неявно заменять друг другом.

## Границы ядра

Ядро предоставляет discovery PCI/Device Tree, capability objects, IRQ/MSI-X,
DMA/IOMMU mappings, memory objects, sync timelines, планирование, revocation и
reset. Оно не реализует Vulkan, OpenGL, оконную политику, compositor или codec.

Обычное приложение никогда не получает raw MMIO, физический framebuffer,
произвольный DMA или display-master capability. При падении драйвера supervisor
отзывает его аппаратные capabilities, останавливает DMA, сбрасывает устройство
и возвращает display в software/fallback path.

## Порядок реализации

1. **Готово:** версионированные `GraphicsBufferDesc`, multi-plane metadata,
   `SyncPoint`/timeline, kernel objects и surface commit/release/feedback ABI.
2. **Готово:** непередаваемая ring-3 scanout capability, atomic full-frame
   present, estimated vblank feedback и постоянные supervisor-сервисы.
3. **Частично готово:** асинхронный virtio transport, несколько запросов
   in-flight, fences и cursor queue; впереди blob resources и несколько outputs.
4. **Готово для текущего desktop:** retained оконные surfaces, transform-only
   drag, multi-buffer damage и GPU backend SystemUI без изменения API компонентов.
5. **Готово как первый end-to-end backend:** Mesa/VirGL, EGL/OpenGL ES ABI и
   zero-copy composition; следующим стандартным backend станет Venus/Vulkan.
6. PCI bridges, ECAM, MSI/MSI-X, IRQ capabilities, scatter/gather DMA и IOMMU.
7. Полные client-owned surface queues и выделенный `inputd`.
8. `libdrm-rustos`, Vulkan WSI, blob resources и несколько outputs.
9. Raspberry Pi VC4/V3D как первая фиксированная физическая ARM-цель.

Первый пакет сохраняет рабочее поведение desktop, но не старые имена API. Весь
workspace сразу переводится с неоднозначных `Surface`/`SurfaceMut` на точные
`CpuSurface`/`CpuSurfaceMut`. Новые процессы используют только capability
buffers и surface commit protocol.

## Последствия

Положительные:

- падение клиента или userspace-драйвера не обязано останавливать desktop;
- UI, видео и 3D используют один zero-copy buffer/sync contract;
- software, virtio и физические GPU backend'ы не меняют API приложений;
- multi-plane formats, HDR metadata и несколько кадров in-flight не требуют
  очередного изменения базового ABI.

Цена решения:

- до аппаратного ускорения нужны DMA/IOMMU, async queues и userspace services;
- lifetime buffers и timelines сложнее синхронного framebuffer copy;
- Mesa port требует зрелых threads, TLS, dynamic loader, VFS и build tools.

## Не является частью решения

Этот ADR не объявляет Vulkan, VirGL, Venus, VC4/V3D, аппаратный decode или
ring-3 compositor уже реализованными. Он фиксирует границы, по которым эти
возможности добавляются отдельными проверяемыми этапами.

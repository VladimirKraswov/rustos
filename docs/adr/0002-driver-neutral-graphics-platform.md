# ADR-0002: driver-neutral графическая платформа RustOS

- Статус: принято
- Дата: 2026-08-22
- Заменяет: части ADR-0001 о границе compositor/driver
- Область: SystemUI, surfaces, compositor, display, GPU providers

## Цель

RustOS должна получить графическую подсистему того же архитектурного класса,
что современные Linux/FreeBSD desktop stacks: приложение не знает модель GPU,
не управляет scanout и не меняется при замене software renderer, VirGL, AMD,
Intel, NVIDIA, Mali, VC4/V3D или другого backend.

Это не утверждение о текущем паритете. Паритет считается достигнутым только по
измеримым критериям из этого документа. До их выполнения kernel SystemUI bridge
является bootstrap/recovery механизмом, а не окончательной архитектурой.

## Проверенные идеи, которые мы используем

RustOS не копирует чужой ABI, но опирается на проверенные свойства:

- Wayland разделяет global geometry и принадлежащую клиенту surface: клиент
  прикрепляет buffer и атомарно публикует состояние через commit, compositor
  возвращает buffer после использования. Это соответствует нашей паре
  `SurfaceCommit` / `BufferReleased`.
- Linux DRM/KMS сначала целиком проверяет независимое atomic state, а затем
  передаёт его driver; неудачный check не меняет hardware state. Plane update
  может быть nonblocking и связан explicit fences.
- Mesa/Gallium разделяет API frontend, общий state tracker и driver screen /
  context. Возможности форматов запрашиваются у provider, а не угадываются
  приложением.

Первичные материалы:

- <https://wayland.freedesktop.org/docs/book/Protocol.html>
- <https://wayland.freedesktop.org/docs/book/Content_Updates.html>
- <https://www.kernel.org/doc/html/latest/gpu/drm-kms.html>
- <https://docs.mesa3d.org/gallium/screen.html>

## Неподвижная граница

```text
application
   | Window/SystemUI/Canvas3D DLL
   v
surface.dll ── atomic surface commit ───────────────┐
                                                    v
inputd ── normalized events ───────────────> compositord
                                                    |
                                  scene + damage + frame clock
                                                    |
                                  GraphicsBuffer + SyncTimeline
                                                    v
                                           render provider
                                      (software/VirGL/native)
                                                    |
                                      ready scanout buffer + fence
                                                    v
                                                displayd
                                      atomic output/plane commit
                                                    |
                                                    v
                                                  display
```

Приложению видны только Window/SystemUI/Canvas3D и surface lifecycle. Оно не
импортирует `rustos-virgl`, raw Mesa/Gallium driver, PCI IDs, MMIO, framebuffer
или display capability. Выбор provider не меняет исходники, RUIDL schema или
RUNE package приложения.

## Kernel objects и ring-3 policy

Kernel предоставляет только механизм:

- generation-safe `GraphicsBuffer` с descriptor, usage и memory domain;
- `SyncTimeline` с монотонными acquire/release points;
- IPC/capability transfer и revocation;
- IRQ/MSI-X, DMA/IOMMU mapping, bounded command queues и device reset;
- `DisplayOutput` authority для одного владельца;
- scheduler primitives без ожидания GPU в syscall/idle hot path.

В kernel запрещены scene graph, z-order, focus policy, shader compiler, OpenGL,
Vulkan, оконные декорации и vendor command language. Временный VirGL validator
и kernel SystemUI recorder удаляются из штатного пути после переноса driver
provider и compositord в ring 3; они остаются только в recovery image.

## Два независимых ABI

### Surface ABI — стабильный для приложений

`SurfaceCommit` атомарно публикует:

- buffer slot и generation;
- physical/logical metrics и fractional scale;
- damage;
- transform;
- acquire timeline point;
- present mode и target time;
- запрос presentation feedback.

До commit pending state невидим. Compositor хранит не очередь событий движения,
а последнее согласованное состояние surface. Для `MAILBOX` новый готовый commit
заменяет старый до начала composition; заменённый buffer обязательно получает
`REPLACED` feedback и release fence.

Положение toplevel окна, z-order, focus и decoration принадлежат compositor,
а не клиенту. Клиент получает configure serial и не может подменить geometry
другого процесса.

### Provider ABI — системный, заменяемый

Supervisor выбирает display/render provider по hardware descriptor и
подписанному manifest. Provider публикует renderer-neutral snapshot:

```text
GpuAdapterInfo
  adapter_id, device_generation, driver_uuid
  vendor/device/subsystem identifiers
  queue classes, memory heaps, reset domains
  supported formats + usage + modifiers
  max dimensions, samples, timelines, sparse/protected flags

DisplayProviderInfo
  outputs, modes, planes, cursor limits
  formats + modifiers, scaling/rotation/color capabilities
  vblank source and variable-refresh range
```

Системные операции используют opaque handles и стандартные records:

```text
adapter_open / context_create / queue_submit
buffer_allocate / buffer_import / buffer_export
timeline_wait / timeline_signal
output_query / atomic_check / atomic_commit
device_status / device_reset
```

Vendor payload допустим только между конкретным userspace provider и выданным
ему device capability. Он не выходит в application, SystemUI или compositor
ABI. Поэтому VirGL, native-context virtio, AMDGPU, Intel, Nouveau/NVK,
Panfrost, VC4/V3D и software providers могут сосуществовать.

## Buffer и synchronization contract

- Buffer descriptor неизменяем весь lifetime capability.
- Format всегда включает channel layout; color space/transfer/alpha задаются
  отдельно. Compositor использует premultiplied alpha.
- Format modifier является opaque `u64`; `LINEAR` — только один из вариантов.
- Import успешен лишь при совпадении format, modifier, planes, usage и security
  domain. Нельзя тихо копировать GPU-only buffer в CPU memory.
- Каждый producer передаёт acquire point; каждый consumer возвращает release
  point. Device loss сигналит timeline ошибкой и не оставляет вечного waiter.
- Buffer нельзя переиспользовать до release. Закрытие последнего process handle
  не освобождает DMA memory, пока provider/device ещё владеет ссылкой.
- Readback в desktop composition запрещён. Screenshot — отдельная capability и
  явная async copy operation, а не скрытая часть present.

## Compositor frame loop

Один цикл имеет только bounded этапы:

1. осушить input до последнего motion, не теряя button/key transitions;
2. принять и проверить surface commits;
3. применить одну atomic scene transaction;
4. удалить occluded layers и объединить damage;
5. выбрать direct scanout / hardware planes / GPU composition;
6. отправить один render graph batch;
7. отправить nonblocking atomic display commit;
8. выдать frame callbacks, release и presentation feedback по completion IRQ.

Ни один этап не ждёт device fence busy-loop. Одновременно разрешено два-три
кадра; более старый непоказанный mailbox frame отбрасывается. Окно при drag
меняет transform, а его content buffer не растеризуется повторно.

Cursor по возможности использует hardware plane. Software cursor является
отдельной маленькой surface и повреждает только старый/новый bounds, а не весь
desktop.

## Rendering SystemUI и Canvas3D

SystemUI строит renderer-neutral display list. Выбранный provider компилирует
его в cached pipelines, vertex/index buffers, texture/glyph atlases и draw
batches. Layout и shaping могут выполняться CPU; обход каждого screen pixel на
CPU в ускоренном сеансе запрещён.

Canvas3D создаёт дочернюю surface/context через Graphics DLL. 2D compositor
видит только готовый buffer и timeline, поэтому одновременно работающие 3D
приложения не делят mutable global render target или frame counter.

## Driver selection и Mac M1

Выбор выполняется по capability, а не по архитектуре CPU:

1. native provider с пройденными probe/reset tests;
2. paravirtual provider (`virtio-gpu` VirGL/Venus/native-context);
3. software provider с display scanout;
4. firmware framebuffer recovery.

Для текущей UTM VM на Apple Silicon первым рабочим provider остаётся
`virtio-gpu + VirGL`, который UTM переводит через ANGLE в Metal. Это host GPU
acceleration, но не Apple GPU driver внутри RustOS. Приложение и compositor не
должны знать об ANGLE/Metal; переход на Venus или другой virtio backend меняет
только provider manifest и negotiated capabilities.

## Failure containment

- Ошибка приложения уничтожает только его surfaces и освобождает buffers после
  fences.
- Ошибка compositor перезапускает compositord; displayd удерживает последний
  корректный scanout либо показывает recovery surface.
- Ошибка render provider запрещает новые submits, завершает timelines как
  `DEVICE_LOST`, отзывает DMA mappings, выполняет bounded reset и перезапуск.
- Ошибка display provider откатывается на последний проверенный mode либо
  recovery framebuffer.
- Повторяющийся hang изолируется supervisor backoff; kernel продолжает input,
  scheduler, VFS и serial diagnostics.

## Критерии зрелости

Формулировка «не хуже Linux/FreeBSD по архитектурному классу» допустима только
после выполнения всех gates:

- штатный desktop, Terminal и два независимых Aurora используют только public
  surface API; `kernel/src/gui` не участвует в обычном кадре;
- `readback=0`, `cpu-raster-pixels=0`, один composition submit и не более одного
  display commit на refresh;
- 60 Hz mode: p95 input-to-present не больше двух refresh intervals, p99 frame
  time не больше 25 ms при drag одного окна;
- два Aurora сохраняют независимые contexts/surfaces и не опускают desktop
  input p95 ниже заданного gate; FPS сцены измеряется presentation feedback, а
  не числом вызовов render loop;
- 30 минут drag/resize/menu/launch stress без роста live buffers, capabilities,
  pending fences или input backlog;
- fault injection отдельно убивает app, compositor, renderd и displayd;
  desktop восстанавливается без kernel panic;
- atomic-check, invalid modifier, stale generation, malformed command,
  timeline timeout, device loss и hot-unplug имеют негативные tests;
- одинаковый surface conformance suite проходит software и VirGL providers;
- добавление тестового второго provider не требует изменения app/SystemUI ABI.

## Этапы перехода

1. Удалить blocking GPU waits и backlog input в bootstrap path.
2. Завершить корректный premultiplied-alpha compositor и frame metrics.
3. Добавить provider discovery/capability snapshot и atomic-check ABI.
4. Перенести latest-commit mailbox, scene graph и input focus в compositord.
5. Перевести desktop/Terminal/Aurora на `surface.dll`; удалить kernel UI из
   normal boot profile.
6. Выделить VirGL transport в первый userspace provider.
7. Подключить Mesa state tracker к provider ABI; затем Venus/Vulkan WSI.
8. Реализовать второй conformance backend (software), потом физический
   VC4/V3D/Panfrost или PCI GPU provider.

Обратная совместимость с текущим bootstrap GUI ABI не является целью: проект
ещё не выпущен, поэтому приложения переводятся один раз на окончательную
surface boundary. После объявления public SDK 1.0 wire ABI становится
версионированным и совместимым по обычным правилам RustOS.

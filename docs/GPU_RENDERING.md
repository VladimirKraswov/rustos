# Ускоренный 3D-путь: renderd, VirGL и GraphicsBuffer

RustOS отделяет рендеринг от вывода на монитор. Наличие Virtio GPU само по
себе не даёт приложению право на PCI/MMIO, scanout или чужую графическую
память. Каждый уровень получает минимальную capability:

```text
renderd (ring 3)              compositord (ring 3)          displayd (ring 3)
GpuRender                     IPC endpoints                 DisplayScanout
    |                               |                            |
    | VirGL commands                |                            |
    v                               |                            |
virtio-gpu control queue            |                            |
    | host 3D renderer              |                            |
    v                               |                            |
GraphicsBuffer + acquire timeline -> validate/forward ----------> atomic present
                                                                  |
                                                                  v
                                                               scanout
```

Рабочий вертикальный срез запускает системное ring-3 приложение **Aurora 3D**.
`rustos-mesa` строит perspective mesh, lighting state и TGSI shader pipeline;
guest CPU передаёт только вершины и команды. Очистку, rasterization,
интерполяцию и запись пикселей выполняет VirGL renderer. Render target не имеет
`CPU_READ`, `CPU_WRITE` или `MAP`, поэтому `renderd` физически не может
подменить тест программной растеризацией.

## Асинхронный command engine

PCI и MMIO transports используют одинаковую split Virtqueue:

- четыре независимых DMA command slot;
- отдельная request/response page для каждого slot;
- generation token запрещает принять старое завершение за новую команду;
- `submit_bytes` копирует bounded stream в kernel-owned DMA page и сразу
  возвращает fence;
- timer bottom half читает used ring и продвигает `SyncTimeline`;
- обычный процесс ждёт timeline и не spin-ит CPU. Пока существуют runnable
  threads, они исполняются параллельно GPU. В bootstrap scheduler ещё нет
  отдельного kernel idle thread: если все процессы ждут именно последний GPU
  fence, kernel выполняет bounded idle-drain и сразу будит timeline waiter.

Bootstrap process ABI допускает один незавершённый submit на единственный
контекст. Это осознанно более узкая граница, чем transport: сначала нужен
простой проверяемый lifetime, затем будут добавлены несколько очередей и
submission records для Mesa. При аварийном завершении `renderd` kernel
осушает очередь до fence и только после этого освобождает capability-backed
кадры — устройство не продолжает DMA в возвращённую память.

## GPU ABI

Syscall ABI v7 добавляет:

- `gpu_get_info` — negotiated VirGL/capset и точные лимиты;
- `gpu_context_create` — изолированный classic VirGL context;
- `gpu_resource_import` — импорт `GraphicsBuffer` как render/scanout target;
- `gpu_resource_create` — context-local resource без CPU mapping;
- `gpu_submit` — bounded асинхронный command buffer и completion timeline;
- `gpu_completion_status` — результат конкретного device fence.

`GpuRender` выдаётся только supervisor-сервису `renderd` и не передаётся.
`GpuContext` также непередаваем. Kernel проверяет ABI records, размеры,
зарезервированные поля, command framing и разрешённый bootstrap subset VirGL.
Host renderer дополнительно разрешает resource handles только в контексте, к
которому kernel уже привязал эти объекты.

`GraphicsBuffer` передаётся compositor'у только с `READ`, а acquire timeline —
только с `WAIT`. `compositord` проверяет неизменяемый descriptor и передаёт
готовый кадр `displayd`. Драйвер делает `SET_SCANOUT` и `RESOURCE_FLUSH` того
же VirGL resource: между 3D render target и экраном нет guest CPU copy.

## Проверка Aurora 3D

Нужен QEMU, собранный с `virglrenderer` и `virtio-vga-gl`:

```sh
make run-virgl
make test-virgl
```

`test-virgl` собирает специальный образ, запускает QEMU с OpenGL display,
ждёт 48 device-fenced/vblank-paced кадров, снимает scanout и проверяет
реальный цветной 3D stage, а не только serial marker. Результаты сохраняются в
`build/test-results/virgl/`.

Обычный Homebrew QEMU 11.1.0 на macOS не содержит `virtio-vga-gl`. Скрипты
в этом случае завершаются явной ошибкой и не называют CPU fallback
ускорением. Совместимую сборку можно указать так:

```sh
RUSTOS_VIRGL_QEMU=/path/to/qemu-system-x86_64 make test-virgl
```

Linux CI устанавливает QEMU/VirGL и Mesa. `LIBGL_ALWAYS_SOFTWARE=1` в
headless CI означает host-side llvmpipe: guest по-прежнему не растеризует
пиксели, но это не доказательство физического GPU acceleration. На машине с
аппаратным OpenGL тот же guest protocol использует host GPU.

На Apple Silicon аппаратный маршрут проверяется отдельно:

```sh
make run-utm-gpu
make test-utm-gpu
```

UTM запускает AArch64 через HVF, принимает VirGL команды
`virtio-gpu-gl-device`, а host renderer UTM переводит их через ANGLE в Metal.
Тест требует маркеры от ring-3 `renderd`, `GraphicsBuffer` scanout и
`cpu-raster=no`; лог лежит в `build/test-results/utm-gpu/serial.log`.

Успешный тест требует сразу три свидетельства:

```text
[gpu-demo] AURORA_3D_READY frames=48 renderer=mesa-virgl cpu-raster=no
[virgl-test] MESA_SHOWCASE_READY scanout=graphics-buffer cpu-raster=no
rustos-gui-check: stage и освещённый объект присутствуют в screenshot
```

## Текущая честная граница

Этот milestone доказывает весь путь Mesa state → 3D-команда → fence →
GraphicsBuffer → compositor → scanout, но ещё не является полной upstream Mesa:

- platform seed поддерживает одну bounded OpenGL-core-подобную сцену;
- один context и один in-flight submission на process ABI;
- первый DMA import требует один физически непрерывный extent;
- нет device-local allocator, eviction, reset/replay и приоритетных engines;
- Virtio/VirGL — виртуальная GPU-модель, а не native V3D/Mali/Intel/AMD driver.

Точный статус upstream-порта, закреплённая версия и C/POSIX зависимости
описаны в [MESA.md](MESA.md). Следующий этап — Meson cross-build
`gallium/virgl` поверх нынешнего winsys. После этого `compositord` сможет
собирать SystemUI GPU-командами, сохраняя `GraphicsBuffer` и `SyncTimeline`.

Протокол сверяется с
[Virtio 1.2 GPU Device](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-3720007),
[Linux virtio_gpu UAPI](https://github.com/torvalds/linux/blob/master/include/uapi/linux/virtio_gpu.h)
и [virglrenderer](https://gitlab.freedesktop.org/virgl/virglrenderer).

# GraphicsBuffer, SyncTimeline и ring-3 display path

Syscall ABI v8 разделяет пиксельную память, синхронизацию, право рендеринга и право управлять
экраном на три разных kernel object. Ядро проверяет память, lifetime, права,
очереди ожидания и доступ к устройству. Политика окон и композиция остаются в
ring 3.

## GraphicsBuffer

`graphics_buffer_create` принимает проверенный `GraphicsBufferDesc` и
возвращает process-local capability. Descriptor неизменяем весь lifetime
объекта: импортёр сначала вызывает `graphics_buffer_get_info`, затем отображает
разрешённый диапазон через `graphics_buffer_map`.

GraphicsBuffer не является псевдонимом shared memory. Kernel различает object
kinds, поэтому syscall для объекта другого типа возвращает `ACCESS_DENIED`.
Права выводятся из descriptor:

- `CPU_READ` выдаёт `READ`;
- `CPU_WRITE` выдаёт `WRITE`;
- CPU mapping требует `MAP`;
- только domain `SHARED` разрешает `TRANSFER` другому процессу.

Текущий allocator предоставляет linear system memory. Обычные CPU surfaces
получают host mapping, а GPU-only render targets создаются без `CPU_READ`,
`CPU_WRITE` и `MAP`: приложение не может растрировать в них пиксели CPU.
Device-local, protected и vendor-modifier buffers пока возвращают
`NOT_SUPPORTED`. Один buffer ограничен 32 МиБ, общий bootstrap budget —
96 МиБ и шесть объектов. Кадры хранятся bounded scatter/gather extents;
первый VirGL import временно требует один непрерывный extent.

Capability references и mapping references считаются отдельно. Закрытие
последнего handle не уничтожает ещё отображённый buffer; завершение процесса
снимает забытые mappings. Последняя ссылка возвращает физические кадры и
увеличивает generation object ID.

## SyncTimeline

Timeline хранит монотонное `u64`. Создатель получает `WAIT | WRITE | TRANSFER`;
при передаче права можно ослабить.

- `sync_timeline_signal` запрещает движение значения назад;
- `sync_timeline_wait` блокирует thread до заданного значения;
- `sync_timeline_wait_many` копирует до 64 points и ждёт `ALL` или `ANY`;
- нулевой timeout означает poll, `u64::MAX` — ожидание без timeout.

Kernel не сохраняет user pointer после входа. Wait-many сначала копирует
records, превращает handles во внутренние generation-safe IDs и удерживает
references. Закрытие исходного handle другим потоком не создаёт dangling wait.

## Эксклюзивная scanout capability

Supervisor выдаёт `DisplayScanout(0)` только `displayd`, с правами
`READ | WRITE | WAIT` и без `TRANSFER`. Ни compositor, ни приложение не
получают MMIO, virtqueue, физический framebuffer или DMA authority.

Display controller ABI содержит три операции:

- `display_get_info` возвращает output, физический режим, format, refresh,
  `mode_generation` и capabilities;
- `display_atomic_present` принимает полностью готовый GraphicsBuffer и
  атомарно делает один `TRANSFER_TO_HOST_2D + RESOURCE_FLUSH`, возвращая
  монотонный sequence;
- `display_wait_vblank` блокирует thread до presentation boundary или timeout.

Commit отвергается, если format, размер или `mode_generation` изменились. В
один момент допускается только один незавершённый commit, поэтому два кадра не
могут частично перемешаться. Текущий ring-3 путь публикует полный кадр; damage
будет добавлен аддитивно после очереди нескольких buffers.

Virtio-gpu 2D подтверждает завершение fenced FLUSH, но не экспортирует отдельный
vblank IRQ. Поэтому `DisplayScanoutInfo` и feedback явно содержат флаг
`ESTIMATED_VBLANK`: kernel вычисляет refresh boundary по монотонным часам. Если
все пользовательские threads уже заблокированы и ожидание таймера создало бы
deadlock bootstrap scheduler, boundary завершается на подтверждённом FLUSH.
Это не заявляется как точный аппаратный vblank.

## Эксклюзивная render capability

При negotiated `VIRTIO_GPU_F_VIRGL` supervisor добавляет третий постоянный
сервис — `renderd`. Только он получает непередаваемую `GpuRender` capability.
Kernel создаёт изолированный context, импортирует GPU-only `GraphicsBuffer`,
принимает bounded VirGL stream и связывает device fence с `SyncTimeline`.

`renderd` не владеет scanout; `displayd` не может отправлять 3D-команды;
`compositord` не получает ни одно из этих аппаратных прав. Готовый buffer и
acquire timeline переходят по обычному capability IPC. Подробный контракт,
ограничения и end-to-end triangle test описаны в
[GPU_RENDERING.md](GPU_RENDERING.md).

## Постоянные renderd, displayd и compositord

При доступном native scanout интерактивная сессия запускает оба процесса как
supervisor services:

```text
renderd                 compositord                   displayd          virtio-gpu
   | async VirGL submit       |                           |                 |
   |--------------------------+---------------------------+---------------->|
   | wait device timeline     |                           |                 |
   | buffer + acquire ------->| validate/forward          |                 |
   |                          | buffer + timelines ------>| atomic present  |
   |                          |                           |---------------->|
   |                          |<----- release + feedback --|                 |
```

Все сервисы остаются живы после первого кадра. `displayd` и `renderd` работают
в классе `Driver`, compositor — `System`. При падении одного процесса supervisor
отзывает capabilities/mappings, сбрасывает private endpoints и перезапускает
пару с новыми PID и handles. Boot-test принудительно завершает оба сервиса,
перезапускает их и требует прохождения второго atomic present: stale
capability не должна пережить restart.

Firmware framebuffer не имеет runtime authority, которую можно безопасно
выдать процессу. На такой машине supervisor оставляет display services
выключенными, но продолжает `vfsd` и kernel recovery desktop; отсутствие
virtio-gpu поэтому не превращается в ошибку загрузки.

Payload `DisplayPresentRequest` занимает 64 байта. Четыре capabilities идут
через обычный IPC attenuation path. Отдельный типизированный
`DisplayPresentFeedback` связывает `frame_id`, display sequence, actual time,
refresh interval и output.

Интерактивный desktop пока рисует kernel bootstrap compositor через тот же
глобальный scanout broker. Это аварийный клиент, а не окончательная граница:
следующий графический рубеж — подключить окна к surface queues постоянного
ring-3 compositor и оставить kernel renderer только для panic/recovery screen.

Критерий integration test:

```text
[graphics-abi-v7] graphics-buffer sync-timeline atomic-present supervisor-restart verified
[supervisor] persistent renderd/compositord/displayd services ready
```

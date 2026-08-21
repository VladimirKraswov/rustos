# GraphicsBuffer, SyncTimeline и ring-3 display path

Syscall ABI v5 добавляет два kernel object, необходимых графике, видео и
будущему GPU API. Ядро управляет только памятью, lifetime, правами и ожиданием;
оконная политика, композиция и выбор display mode остаются в ring 3.

## GraphicsBuffer

`graphics_buffer_create` принимает полностью проверенный
`GraphicsBufferDesc` и возвращает process-local capability. Descriptor
неизменяем весь lifetime объекта, поэтому импортёр сначала вызывает
`graphics_buffer_get_info`, а затем отображает разрешённый диапазон через
`graphics_buffer_map`.

Это не alias обычной shared memory. Kernel различает object kinds, поэтому
`shared_memory_map(graphics_handle)` и
`graphics_buffer_get_info(timeline_handle)` возвращают `ACCESS_DENIED`.
Права вычисляются из descriptor:

- `CPU_READ` выдаёт `READ`;
- `CPU_WRITE` выдаёт `WRITE`;
- CPU mapping требует `MAP`;
- только domain `SHARED` разрешает `TRANSFER` другому процессу.

Текущий allocator реализует linear, host-visible system memory. Device-local,
protected и vendor-modifier buffers честно возвращают `NOT_SUPPORTED`; их
добавит driver allocator, не подменяя обычной RAM. Один buffer ограничен
32 MiB, суммарный bootstrap budget — 96 MiB и шесть объектов. Физическая
память хранится bounded scatter/gather extents: сначала allocator пытается
получить крупный extent, при фрагментации уменьшает запрос до страницы.

Capability references и mapping references считаются независимо. Закрытие
последнего handle не разрушает ещё отображённый buffer; завершение процесса
снимает забытые mappings. Последние references возвращают все extents
физическому allocator'у и меняют generation object ID.

## SyncTimeline

Timeline содержит одно `u64`, которое может только расти. Создатель получает
`WAIT | WRITE | TRANSFER`; при передаче compositor обычно оставляет клиенту
`WAIT`, а producer/display service получает только `WRITE`.

- `sync_timeline_signal` разрешает повтор текущего значения, но отвергает
  движение назад;
- `sync_timeline_wait` блокирует вызывающий thread до `value >= requested`;
- `sync_timeline_wait_many` копирует до 64 points из shared memory и ждёт
  условие `ALL` или `ANY`;
- timeout равен длительности в наносекундах от момента syscall; ноль означает
  poll с результатом `TIMED_OUT`, `u64::MAX` — ожидание без timeout.

Kernel не сохраняет user pointer после входа. Wait-many сначала копирует и
валидирует все records, преобразует process-local handles во внутренние
generation-safe IDs и удерживает references объектов. Поэтому другой thread
может закрыть исходный handle или изменить shared array, не создавая dangling
wait. Signal и timer переводят waiter `Blocked -> Ready`; busy-spin отсутствует.

## Первый ring-3 вертикальный срез

Boot-test запускает два независимых RUNE-процесса:

```text
compositord                         displayd
    | create/map GraphicsBuffer        | receive blocks
    | CPU raster + unmap               |
    | signal acquire=1                 |
    | IPC: buffer + acquire + release  |
    |--------------------------------->| wait acquire
    | wait-many(ALL) blocks            | map RO + consume
    |                                  | signal release=1
    |<------------ scheduler wake -----|
```

Payload `DisplayPresentRequest` имеет ровно 64 байта и передаётся inline; три
capabilities идут через стандартный attenuation path IPC. `SurfaceCreate` и
`SurfaceCommit` проверяются compositor'ом до публикации. Тест доказывает
передачу buffer ownership, explicit acquire/release, настоящее блокирующее
ожидание, capability isolation и возврат кадров после завершения обоих
процессов на AMD64 и AArch64.

Этот `displayd` пока использует headless present backend и не объявляет
перенос рабочего стола завершённым. Kernel bootstrap compositor продолжает
показывать интерактивный desktop. Следующий этап — выдать только `displayd`
scanout/virtio-gpu capability, перенести output enumeration, atomic modeset и
vblank feedback, затем сделать `compositord` постоянным supervisor service.

Критерий integration test:

```text
[graphics-abi-v5] GraphicsBuffer SyncTimeline ring3 displayd/compositord verified
```

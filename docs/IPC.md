# Capability IPC

## Исполняемый контракт

IPC уже проходит через настоящие CPL3-процессы. Endpoint является kernel
object, доступ к которому задаётся отдельными process-local handles:

- `SEND` разрешает `IPC_SEND`;
- `RECEIVE` разрешает `IPC_RECEIVE`;
- само числовое значение handle в другом процессе ничего не означает.

Сообщение имеет фиксированный размер 160 байт: 32-байтный header, 64 байта
inline payload и до четырёх transferred handles. Kernel проверяет ABI version,
length/count/reserved fields и mapped user range. `sender_pid` из user buffer
обязан быть нулём; kernel заменяет его настоящим generation-safe PID.

Endpoint содержит bounded FIFO на восемь сообщений. Если очередь пуста,
receiver переходит `Running -> Blocked`, а scheduler запускает другой TID.
Send либо кладёт сообщение в FIFO, либо сразу копирует его в проверенный
writable buffer ожидающего receiver, устанавливает syscall result и выполняет
`Blocked -> Ready`. Busy polling не используется. Полная очередь возвращает
`QUEUE_FULL` до создания производных capabilities.

`endpoint_create()` создаёт process-owned reply/event channel. Владелец
получает `SEND | RECEIVE | TRANSFER`, но передаёт сервису только производный
`SEND` handle. Право `RECEIVE` у динамического endpoint нельзя копировать,
передавать через IPC или наследовать при spawn: очередь всегда имеет ровно
одного владельца. Закрытие owning handle или завершение процесса атомарно
удаляет очередь и отзывает все её `SEND` capabilities. Generation в object ID
защищает новый endpoint от оставшихся stale handles старого объекта.

## Передача capability

Передаваемая запись содержит source handle и запрошенные права. Kernel:

1. ищет source только в таблице отправителя;
2. требует у него право `TRANSFER`;
3. запрещает пустой набор и любое право, отсутствующее у source;
4. заранее резервирует свободные slots получателя для всего сообщения;
5. создаёт новые entries и заменяет handles внутри доставляемого сообщения.

Передача транзакционна относительно валидации: amplification или нехватка
slots отклоняет всё сообщение. Bootstrap sender сначала намеренно просит
`READ|WRITE` у source `READ|TRANSFER` и получает `ACCESS_DENIED`; допустимая
передача `READ` затем будит receiver. Полученный VFS handle успешно выполняет
`stat /boot/README.txt` и уже не содержит `TRANSFER`.

FIFO и attenuation находятся в platform-independent crate
`rustos-microkernel`, поэтому проходят host unit tests. Pointer validation,
CR3, block/wake и физическая доставка проверяются QEMU boot-тестом.

## Shared-memory data plane

Inline IPC предназначен для control plane. Shared-memory object capabilities
уже используются VFS: `vfs-1.dll` создаёт и отображает reusable окно 64 КиБ,
а в сообщении передаёт производный `READ | WRITE | MAP` handle, offset и
length. `vfsd` отображает окно только на время запроса, выполняет потоковый
I/O и закрывает переданную capability на всех путях, включая ошибки. Файл
70 000 байт в boot-test гарантированно проходит несколькими chunks.

Та же схема предназначена для framebuffer damage regions и compiler
artifacts:

```text
sender maps RW -> seals/attenuates -> sends MAP|READ handle
receiver maps RO -> processes bytes -> reply/release
```

Следующие расширения IPC — multi-wait, cancellation/timeouts и priority
inheritance для synchronous call/reply. Bootstrap endpoints постоянных
сервисов пока создаёт process manager, а приложения уже создают независимые
динамические reply/event channels. Supervisor должен будет переиздавать
service capabilities после restart сервиса.

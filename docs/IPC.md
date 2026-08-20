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

Следующие расширения IPC — user-facing endpoint factory, multi-wait,
cancellation/timeouts, priority inheritance для synchronous call/reply и
полное revoke tree. Текущие bootstrap endpoints для `vfsd` создаёт process
manager; supervisor должен будет переиздавать их после restart сервиса.

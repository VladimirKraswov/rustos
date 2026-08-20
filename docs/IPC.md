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

## Следующее расширение

Inline IPC предназначен для control plane. Файлы, framebuffer damage regions
и compiler artifacts не должны копироваться по 64 байта. Следующая версия
добавит shared-memory object capabilities:

```text
sender maps RW -> seals/attenuates -> sends MAP|READ handle
receiver maps RO -> processes bytes -> reply/release
```

Также нужны endpoint create/destroy syscalls, multi-wait, cancellation,
timeouts, priority inheritance для synchronous call/reply и revoke tree.
Текущий bootstrap endpoint создаёт process manager; user-facing factory будет
capability-защищённым API supervisor'а.

# Процессы, потоки и память: syscall ABI v4

Этот документ описывает первый исполняемый ABI RustOS, достаточный для
построения `std::process`, `std::thread`, allocator и zero-copy IPC. Общие
`#[repr(C)]` структуры находятся в `rustos-abi`, обёртки — в
`rustos-runtime`, а проверка выполняется настоящими RUNE-процессами ring 3.

## Capability-модель

PID и TID нужны для диагностики, но не дают права управлять объектом. Любая
операция над чужим процессом, потоком или shared memory требует локальный
capability handle:

- `process_spawn` возвращает process capability `WAIT | DESTROY | TRANSFER`;
- `thread_create` возвращает thread capability `WAIT | TRANSFER`;
- `shared_memory_create` возвращает `MAP | TRANSFER` и права данных;
- IPC и spawn могут только уменьшить исходные права;
- после reap generation PID/TID меняется, stale handles инвалидируются.

Это позволяет позже подключить supervisor, `vfsd` и user-space драйверы без
несовместимого перехода от глобальных Unix PID к capabilities.

## Процессы, argv и environment

`process_spawn(request, result)` создаёт отдельные таблицы страниц, загружает
RUNE image и создаёт начальный поток. Сейчас image читается из read-only
initramfs namespace. После запуска `vfsd` тот же request будет обслуживаться
VFS capability без изменения структуры ABI.

`ProcessSpawnRequest` содержит VFS capability `READ | EXECUTE`, UTF-8 путь,
NUL-разделённые таблицы argv/environment, пользовательский priority class и
список производных capabilities с заранее выбранными slots ребёнка.

Kernel отображает ребёнку read-only `ProcessStartInfo` по адресу
`PROCESS_START_INFO_ADDRESS`. Первый аргумент entry point указывает на этот
блок, второй содержит `syscall::ABI_VERSION`. Заголовок сообщает PID, TID,
page size, частоту монотонного counter и адреса скопированных argv/env.

`process_wait` блокирует только вызывающий поток. После завершения он получает
полный `ExitReason`, а zombie, address space и физические кадры уничтожаются.
`process_kill` требует `DESTROY`; fault или kill процесса не прекращает работу
остальных процессов и микроядра.

## Потоки и TLS

`thread_create` создаёт независимо планируемый контекст внутри текущего
address space. Пользователь заранее отображает стек и передаёт исполняемый
entry address, ABI-совместимый stack pointer, первый аргумент, thread pointer
и priority class.

На AMD64 thread pointer хранится в `IA32_FS_BASE`, на AArch64 — в
`TPIDR_EL0`. Process manager восстанавливает его при каждом context switch.
`thread_exit` завершает только текущий поток, `thread_join` блокирует caller и
возвращает `ExitReason`. `process_exit` атомарно завершает все потоки процесса.

## Виртуальная память

- `vm_map` отображает private anonymous zero-filled pages;
- `vm_unmap` удаляет PTE и немедленно освобождает private frames;
- `vm_protect` меняет права существующего диапазона.

Адрес и размер кратны 4 КиБ. Нулевой адрес включает автоматический выбор в
отдельной VM arena. Kernel проверяет canonical user range и выполняет TLB
flush до возврата. Одновременные `WRITE | EXECUTE` запрещены политикой W^X.

Текущая bootstrap-сборка ограничивает одну операцию 256 страницами, один
address space — 1024 описателями страниц. Это защитные лимиты статических
таблиц раннего ядра, а не ограничение ABI или 64-битного адреса. Перед портом
`rustc` metadata будет перенесена в pageable kernel slabs/radix tree.

## Shared memory

Shared memory — отдельный kernel object с собственными физическими кадрами.
`shared_memory_map` одновременно проверяет права capability, максимальные
права объекта, offset/length, свободный user range и W^X.

`shared_memory_seal(handle, READ [| EXECUTE])` поддерживает безопасный DLL
page cache. Object создаётся RW, заполняется ровно одним владельцем, его RW
mapping снимается, после чего seal необратимо убирает WRITE. Операция требует
один capability reference и ноль mappings; kernel заменяет authority handle
на R/RX. Добавить WRITE обратно или одновременно держать W и X невозможно.

Capability references и mapping references считаются раздельно. Поэтому
`handle_close` не освобождает кадр, пока он отображён, а завершение процесса
снимает mapping references даже у приложения, забывшего `vm_unmap`. Последние
`unmap` и `close` возвращают кадры allocator.

Обычный capability IPC уже умеет передавать shared-memory handle: маленький
control message остаётся inline, а исходники, object-файлы и VFS-буферы можно
передавать без копирования payload через kernel.

## Монотонные часы

`clock_monotonic` возвращает наносекунды от аппаратной монотонной эпохи. AMD64
использует откалиброванный invariant TSC, AArch64 — architected counter.
Значение не является календарным временем и не должно уменьшаться.

## Сквозная проверка

`rustos-abi-lifecycle` и `rustos-abi-child` внутри QEMU проверяют:

1. anonymous map/protect/unmap и W^X;
2. shared mapping двух процессов и capability transfer при spawn;
3. argv/environment в `ProcessStartInfo`;
4. блокирующие process wait и thread join;
5. внешний process kill;
6. отдельный FS-base/TPIDR TLS потока;
7. монотонные часы;
8. возврат private/shared/page-table frames.

```sh
make test-boot
```

Критерий прохождения в serial log:

```text
[abi-v4] spawn/wait/kill threads VM shared-memory TLS clock verified
[process-manager] ABI v4 VM/shared-memory frames reclaimed
```

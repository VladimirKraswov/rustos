# Микроядро: рабочая граница и следующий этап

Этот документ отделяет уже исполняемый механизм от спроектированного API.
Главная проверка не сводится к наличию структур в исходниках: обычный boot и
CI действительно переходят в CPL3, вызывают kernel и переживают user fault.

## Что работает сейчас

```text
RIFS initramfs
      |
      v
RUNE loader --------- создаёт отдельный address-space root
      |               RX code, RW+NX data/stack/TLS, W^X + RELRO
      v
iretq -> CPL3 init.rune -> int 0x80 -> VFS handle check -> RIFS stat
      |                                   |
      +---- process_exit / exception -----+
                         |
                         v
              kernel address space + privileged stack
                         |
                         v
          Drop AddressSpace -> free всех кадров
```

Kernel устанавливает собственные GDT, TSS и IDT. TSS содержит отдельный
ring-0 stack, double fault использует IST. До входа пользователя включаются
`EFER.NXE` и `CR0.WP`. Рабочий AMD64 loader проверяет RUNE SHA-256 и records
до отображения, запрещает writable+executable regions, применяет
`RELATIVE64`, создаёт TLS и user stack, закрывает RELRO и проверяет entry.
ELF64 остаётся явным migration fallback и build intermediate.

Тот же container выбирает AArch64 slice; converter нормализует
`R_AARCH64_RELATIVE`, а register context и syscall ABI выбираются `arch` HAL.
Исполняемый ARM boot path уже проходит AAVMF → EL1 → изолированный EL0,
GICv3 preemption, PSCI SMP bring-up и тот же RUNE/VFS/loader contract.

`init.rune` получает в bootstrap-регистрах ABI version и handle. Handle является
индексом только в capability table этого процесса; число само по себе не даёт
прав. Kernel проверяет kind/rights, canonical mapped range и UTF-8, копирует
короткий путь из user memory и только затем вызывает bootstrap RIFS backend.
Тест сначала предъявляет заведомо чужой handle и ожидает `BAD_HANDLE`, затем
делает успешный read-only `vfs_stat`.

Второй процесс выполняет `UD2`. Trap handler определяет CPL по сохранённому
`CS`, записывает `ExitReason`, возвращает kernel CR3 и не продолжает
неисправный instruction stream. После обычного exit и fault boot-тест
сравнивает `free_frames` до создания address space и после `Drop`: код, данные,
stack и все уровни page tables обязаны быть возвращены allocator'у.

## Жизненный цикл и scheduler policy

Crate `rustos-microkernel` не зависит от x86 и heap. Это позволяет проверять
одну и ту же state machine unit-тестами на macOS/Linux и использовать её из
kernel.

- PID и TID состоят из `slot + generation`; stale reference после reap
  отклоняется.
- Process проходит `Free -> Alive -> Zombie -> Free`; exit status доступен
  supervisor'у до reap.
- Thread проходит `Free -> Ready -> Running/Blocked -> Exited -> Free`.
- Fault завершает все потоки только соответствующего PID; unrelated process
  остаётся runnable.
- Affinity — 64-битная маска логических CPU. Ноль и биты вне конфигурации
  запрещены.
- Внутри одного класса используется round-robin по `last_run`.

Классы, от более срочного к менее срочному:

1. `Kernel` — только короткая доверенная работа ядра;
2. `Driver` — IRQ/DMA workers пользовательских драйверов;
3. `System` — supervisor, VFS, display и другие серверы;
4. `Interactive` — terminal, editor и активное GUI;
5. `Batch` — compiler, linker, indexer;
6. `Idle`.

Driver имеет приоритет, но не бесконечный realtime. После восьми driver
quanta готовый lower-class поток получает квант, затем budget начинается
снова. Это защищает input latency и одновременно не позволяет сломанному
драйверу навсегда остановить supervisor. В будущем deadline/real-time policy
будет отдельной capability, а не правом любого процесса поднять себе priority.

Supervisor policy перезапускает только аварийно завершившийся сервис,
ограничивает число попыток и использует capped exponential backoff. Успешная
работа сбрасывает серию ошибок. На текущем этапе это протестированная state
machine; привязка к service manifests появится вместе с асинхронным process
manager.

## Драйверы и аппаратные прерывания

Финальная IRQ-модель сохраняет минимум кода в kernel:

1. interrupt stub подтверждает local APIC/IOAPIC и помечает IRQ pending;
2. kernel будит driver thread, владеющий IRQ capability;
3. driver читает device queue в своём address space;
4. DMA использует только кадры, выданные процессу через DMA/IOMMU capability;
5. падение driver process отбирает MMIO/port/IRQ/DMA handles, supervisor
   выполняет reset/backoff и перезапускает его.

Обычный код драйвера не исполняется в interrupt context. Даже высокий класс
`Driver` не разрешает обращаться к памяти kernel или другого процесса.

## Рабочее вытеснение и граница SMP

CPU0 уже использует local APIC timer: x2APIC/MSR на современных CPU и
xAPIC/MMIO fallback на старом TCG. На CPU с TSC-deadline применяется deadline
mode; без этой возможности калибруется periodic decrement counter по TSC.
Каждый IRQ сохраняет все GPR, user RIP/RFLAGS/RSP, scheduler переводит текущий
TID обратно в Ready, выбирает следующий, kernel загружает его CR3 и меняет
trap frame. Два CPU-bound ELF не вызывают `yield`, поэтому ненулевой счётчик
переключений доказывает именно аппаратное вытеснение.

Process manager динамически создаёт address spaces и generation-safe PID/TID,
обрабатывает Ready/Running/Blocked/Exited, сохраняет zombie status до reap и
после каждой фазы проверяет возврат всех физических кадров. Конкурентный тест
запускает `UD2` и survivor одновременно: fault завершает первый PID, второй
продолжает получать timer quanta и выходит сам.

ACPI parser проверяет RSDP/XSDT/MADT checksums и enabled flags. BSP по одному
посылает AP INIT–SIPI–SIPI. Копируемый код ниже 1 MiB проходит real mode,
protected mode и long mode, устанавливает kernel CR3/отдельный stack, затем AP
включает свой local APIC и подтверждает ID. В штатном тесте
`discovered=2 online=2`.

При этом AP пока **parked**, а не является scheduler CPU: у него ещё нет
per-CPU GDT/TSS/IDT, ring-0 interrupt stack, run queue и TLB shootdown inbox.
Следующий аппаратный milestone:

1. per-CPU descriptors, current thread, preemption/IRQ nesting counters;
2. local timer и scheduler loop на каждом AP;
3. per-CPU ready queues, affinity migration и bounded work stealing;
4. IOAPIC routing в kernel IRQ endpoints пользовательских драйверов;
5. TLB shootdown и PCID optimization;
6. тест параллельной записи разных user pages двумя CPU с fault/GUI heartbeat.

Process ABI v7 предоставляет spawn/kill/wait, несколько потоков, anonymous
VM, sealed shared-memory capabilities, args/env, TLS и monotonic clock.
Streaming VFS IPC, persistent VaraniaFS и user-space RUNE DLL loader уже
проходят boot-test. `std::process::Command` умеет выполнять RUNE с VFS через
ring-3 runner. Постоянные `renderd`/`displayd`/`compositord` уже получают
раздельные render/scanout capabilities, priorities, private endpoints и
bounded restart policy. Следующий программный
milestone — ring-3 `init`, который запускает filesystem, display/input services
и desktop по manifests вместо bootstrap-таблиц kernel. Контракт описан в
[PROCESS_MEMORY_ABI.md](PROCESS_MEMORY_ABI.md) и
[ELF_LOADER.md](ELF_LOADER.md).

## Почему это база для self-hosting

Upstream `std`, dynamic loader и будущий native seed `rustc` опираются на процессы и VFS, а не
на прямые kernel shortcuts. Последовательность зависимостей следующая:

```text
preemptive threads + IPC
        -> vfsd/blockd/filesystem + persistent files
        -> RUNE loader + system/vfs client ABI
        -> target std (allocator/fs/time/TLS/thread/process/pipe готовы)
        -> rust-lld + native seed rustc/cargo
        -> сборка RustOS внутри RustOS
```

Новый VFS path уже использует тонкий `vfs-1.dll` client stub и capability IPC
к `vfsd`; старый `vfs_stat` остаётся только bootstrap proof для раннего
`init.rune` и будет удалён вместе с kernel-side initramfs spawn.

## Автоматические критерии

`make test-host` проверяет starvation budget, round-robin, affinity, stale
PID/TID, изоляцию process fault и supervisor backoff. `make test-boot` в QEMU
проверяет реальные privilege/page-table/trap paths и требует markers:

```text
[process] init.rune exited cleanly; VFS capability verified
[isolation] user #UD contained; kernel and GUI continue
[memory] user address spaces reclaimed
[smp] discovery=ACPI MADT discovered=2 online=2 APs parked safely
[preempt] timer ticks=... context-switches=...
[isolation] concurrent #UD terminated one process; survivor exited=22
[ipc] queued block/wake and attenuated VFS capability verified
[abi-v4] spawn/wait/kill threads VM shared-memory TLS clock verified
[graphics-abi-v7] graphics-buffer sync-timeline atomic-present supervisor-restart verified
[supervisor] persistent renderd/compositord/displayd services ready
[std] allocator fs threads futex process pipes stdio native SDK and VFS executable verified in ring3 RUNE
[vfsd] restart recovered committed VaraniaFS metadata and file data
[loader] RUNE interfaces imports ABI TLS RELRO and cross-process shared RX verified
[process-manager] dynamic create/exit/reap reclaimed all frames
[microkernel] RING3_MILESTONE_OK
```

# Архитектура RustOS

## Загрузка

UEFI-приложение `rustos-boot` находит GOP и ACPI, закрепляет единый bootstrap
region через `AllocatePages(LOADER_DATA)`, загружает ELF64 PIE kernel и
initramfs, строит разрежённую identity-карту и передаёт `BootInfo v2`.

Page tables отображают реальные RAM descriptors, kernel reservation, RSDP и
точный диапазон framebuffer. Большие MMIO-дыры не выдаются за RAM.

## Текущие подсистемы

```text
UEFI/OVMF
   |
   v
rustos-boot --> BootInfo v2 --> kernel
                                  |-- serial diagnostics
                                  |-- GDT/TSS/IDT + exception containment
                                  |-- physical frame allocator
                                  |-- ELF64 CPL3 bootstrap runner
                                  |-- process-local VFS capability
                                  |-- GOP renderer
                                  |-- PS/2 input
                                  |-- bootstrap RIFS + RAM overlay
                                  |-- UI components
                                  |-- window manager
                                  `-- desktop + terminal
```

Ранний GUI-сеанс работает на CPU0 и не использует heap. Unsafe MMIO и port I/O
сосредоточены в `graphics`, `input` и `arch`; widget/window/application logic
написана без unsafe. Первый kernel object уже существует: process-local VFS
handle разрешает ring-3 `init.elf` выполнить ограниченный `vfs_stat`. Это
bootstrap backend initramfs, не финальный `vfsd`.

Platform-independent crate `rustos-microkernel` содержит process/thread
lifecycle, generation-safe PID/TID, CPU affinity, scheduler priority policy и
supervisor backoff. Он `no_std`, но тестируется на host без QEMU. x86 runner
пока выполняет один пользовательский процесс синхронно; local APIC timer и
переключение нескольких сохранённых контекстов ещё не подключены.

## Путь к микроядру

Текущий desktop — проверочный вертикальный срез, а не финальная граница
привилегий. Этапы 1–3a уже исполняются и проверяются при boot:

1. **готово:** собственные GDT/TSS/IDT, CPL3 traps и возврат в kernel;
2. **готово:** physical allocator, отдельные CR3, W^X/NX и полный reclaim;
3. **готово:** ELF64 PIE loader, VFS capability и изоляционный fault test;
4. **база готова:** lifecycle/scheduler policy; дальше APIC preemption и SMP;
5. capability transfer, очереди IPC и shared memory;
6. process manager, `vfsd`, persistent filesystem и dynamic loader;
7. target `std`, native Rust toolchain и package/build services;
8. `inputd`, `displayd`, compositor и terminal как отдельные процессы;
9. supervisor применяет restart policy к реальным service manifests.

UI API уже отделён от framebuffer ownership, поэтому widget logic не должна
переписываться при замене прямых вызовов IPC-сообщениями.

## Масштаб памяти

On-disk offsets и физические адреса используют `u64`. Frame allocator уже
хранит ограниченный набор свободных extent'ов из usable UEFI descriptors, а
не bitmap до максимального MMIO-адреса. Поэтому терабайты RAM не требуют
терабайтно-пропорциональной служебной таблицы. Будущий NUMA allocator разделит
эти extent'ы по node/zone, не меняя 64-битный ABI.

Детальный контракт и честная карта следующего этапа находятся в
[MICROKERNEL.md](MICROKERNEL.md).

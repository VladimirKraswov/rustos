# Архитектура RustOS

## Загрузка

UEFI-приложение `rustos-boot` находит GOP и ACPI, закрепляет единый bootstrap
region через `AllocatePages(LOADER_DATA)`, загружает ELF64 PIE kernel и
initramfs, строит разрежённую identity-карту и передаёт `BootInfo v3`.

Page tables отображают реальные RAM descriptors, kernel reservation, RSDP и
точный диапазон framebuffer. Большие MMIO-дыры не выдаются за RAM.

## Текущие подсистемы

```text
UEFI/OVMF
   |
   v
rustos-boot --> BootInfo v3 --> kernel
                                  |-- serial diagnostics
                                  |-- GDT/TSS/IDT + exception containment
                                  |-- physical frame allocator
                                  |-- ELF64 CPL3 bootstrap runner
                                  |-- process-local VFS capability
                                  |-- local-APIC preemptive process manager
                                  |-- endpoint IPC + capability transfer
                                  |-- MADT + AP long-mode trampoline
                                  |-- GOP scanout + rustos-video surfaces
                                  |-- damage/layer CPU compositor
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
lifecycle, generation-safe PID/TID, CPU affinity, scheduler priority policy,
bounded endpoint queue, capability attenuation и supervisor backoff. Он
`no_std`, но тестируется на host без QEMU. Переносимый process manager
подключён к `arch` HAL; рабочий AMD64 backend использует local APIC:
timer trap сохраняет регистры/RSP, выбирает TID, меняет CR3 и возвращается
через `iretq` в другой CPL3 context. AArch64 backend определяет эквивалентные
EL0/EL1 context, TTBR и `svc` ABI и собирается в CI; GIC/PSCI boot ещё не
объявляются работающими. Полный контракт описан в [ARCHITECTURES.md](ARCHITECTURES.md).

Platform-independent `rustos-video` задаёт безопасные pixel surfaces,
RGB/BGR/ARGB formats, span fill, blit, alpha composition, bounded damage и
неограниченный slice layers. Kernel desktop уже использует его raster/damage
часть поверх GOP; тот же контракт предназначен будущему ring-3 `displayd`.

MADT перечисляет CPU, BSP последовательно выполняет INIT–SIPI–SIPI. AP
проходит 16 -> 32 -> 64 bit trampoline, получает отдельный stack, включает
свой local APIC и публикует ID. Предпочтителен x2APIC/MSR, а старый TCG
использует совместимый xAPIC/MMIO backend. До per-CPU TSS/IDT AP parked с
выключенными interrupts и не участвует в scheduling.

## Путь к микроядру

Текущий desktop — проверочный вертикальный срез, а не финальная граница
привилегий. Этапы 1–3a уже исполняются и проверяются при boot:

1. **готово:** собственные GDT/TSS/IDT, CPL3 traps и возврат в kernel;
2. **готово:** physical allocator, отдельные CR3, W^X/NX и полный reclaim;
3. **готово:** ELF64 PIE loader, VFS capability и изоляционный fault test;
4. **готово на CPU0:** APIC preemption, dynamic lifecycle, endpoint IPC и
   capability transfer;
5. **bootstrap готов:** AP startup; дальше per-CPU TSS/IDT, timer queues,
   TLB shootdown и work stealing;
6. **готово:** shared-memory IPC и process create/kill/wait syscalls;
7. **готово:** изолированный `vfsd`, persistent VaraniaFS и user-space
   dynamic loader (`DT_NEEDED`, symbols, TLS, RELRO, shared RX);
8. полный `exec` через VFS, target `std`, native Rust toolchain и
   package/build services;
9. `inputd`, `displayd`, compositor и terminal как отдельные процессы;
10. supervisor применяет restart policy к реальным service manifests.

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

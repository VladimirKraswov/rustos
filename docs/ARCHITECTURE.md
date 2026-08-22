# Архитектура RustOS

## Загрузка

Standalone GRUB 2 загружает фиксированный ELF64 kernel по Multiboot2 и
initramfs как module. Архитектурный bootstrap переводит memory map,
framebuffer и ACPI в переносимый `BootInfo v3`, строит identity map и входит
в обычную Rust-точку `_start`. Подробности находятся в [GRUB.md](GRUB.md).

Kernel, module и Multiboot information вырезаются из usable descriptors.
Большие MMIO-дыры отображаются для раннего доступа, но не выдаются за RAM.

## Текущие подсистемы

```text
UEFI/OVMF
   |
   v
GRUB 2 / Multiboot2 --> BootInfo v3 --> kernel
                                  |-- serial diagnostics
                                  |-- GDT/TSS/IDT + exception containment
                                  |-- physical frame allocator
                                  |-- RUNE CPL3 loader (ELF migration fallback)
                                  |-- process-local VFS capability
                                  |-- local-APIC preemptive process manager
                                  |-- endpoint IPC + capability transfer
                                  |-- MADT + AP long-mode trampoline
                                  |-- GRUB/virtio scanout + CPU fallback
                                  |-- graphics-buffer/surface/sync ABI
                                  |-- async virtio-gpu/VirGL render ABI
                                  |-- damage/layer CPU compositor
                                  |-- xHCI USB HID input + PS/2/virtio fallback
                                  |-- bootstrap RIFS + RAM overlay
                                  |-- UI components
                                  |-- window manager
                                  `-- desktop + terminal
```

Ранний GUI-сеанс работает на CPU0 и не использует heap. Unsafe MMIO и port I/O
сосредоточены в `graphics`, `input` и `arch`; widget/window/application logic
написана без unsafe. Первый kernel object уже существует: process-local VFS
handle разрешает ring-3 `init.rune` выполнить ограниченный `vfs_stat`. Это
bootstrap backend initramfs, не финальный `vfsd`.

Platform-independent crate `rustos-microkernel` содержит process/thread
lifecycle, generation-safe PID/TID, CPU affinity, scheduler priority policy,
bounded endpoint queue, capability attenuation и supervisor backoff. Он
`no_std`, но тестируется на host без QEMU. Переносимый process manager
подключён к `arch` HAL. AMD64 backend использует local APIC: timer trap
сохраняет регистры/RSP, выбирает TID, меняет CR3 и возвращается через `iretq`
в другой CPL3 context. AArch64 выполняет тот же контракт через полный
EL0/EL1 `TrapFrame`, TTBR0 и `eret`: GICv3/Generic Timer вытесняют процессы,
а Device Tree + PSCI запускают и безопасно паркуют AP. Оба маршрута проходят
настоящие QEMU boot integration tests. Полный контракт описан в
[ARCHITECTURES.md](ARCHITECTURES.md).

Основной user stack начинает с 64 КиБ и растёт вниз страницами по translation
fault до 8 МиБ. Ядро заполняет только короткий непрерывный промежуток (не более
64 КиБ) до уже отображённой части стека — этого достаточно для stack probing
AMD64/AArch64. Дальний произвольный fault, protection fault или выход за лимит
по-прежнему завершает только виновный процесс. Поэтому крупные scratch frames
сервисов не требуют заранее закреплять мегабайты RAM за каждым процессом.

Platform-independent `rustos-video` разделяет локальные
`CpuSurface`/`CpuPixelFormat` для software fallback и межпроцессный graphics
contract. `GraphicsBufferDesc` описывает packed/multi-plane память, color,
usage/domain и modifier; `SyncPoint` и `SurfaceCommit` задают явное владение
кадром, damage, buffer release и presentation feedback. Kernel objects уже
предоставляют generation-safe GraphicsBuffer/SyncTimeline handles, mapping
lifetime и блокирующий wait-many. Постоянный RUNE `displayd` эксклюзивно
владеет непередаваемой scanout capability. При VirGL постоянный `renderd`
отдельно владеет render capability, отправляет асинхронные 3D-команды и
передаёт GPU-only buffer compositor'у; `compositord` выполняет atomic present,
ждёт оценочный vblank и получает feedback. Клиентская `surface.dll` уже
реализует process-owned event endpoint и полный lifecycle полноэкранной
zero-copy surface. Интерактивный desktop пока использует CPU raster/damage
bootstrap через тот же broker; оконная multi-layer композиция остаётся
следующим переключением. Контракт описан в
[GRAPHICS_ABI.md](GRAPHICS_ABI.md), [GPU_RENDERING.md](GPU_RENDERING.md) и
[ADR-0001](adr/0001-modern-graphics-architecture.md). Проверяемые критерии
перехода всего desktop на GPU и порядок удаления bootstrap readback закреплены
в [GPU_ACCELERATION.md](GPU_ACCELERATION.md).

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
3. **готово:** RUNE loader, VFS capability и изоляционный fault test;
4. **готово на CPU0:** APIC preemption, dynamic lifecycle, endpoint IPC и
   capability transfer;
5. **bootstrap готов:** AP startup; дальше per-CPU TSS/IDT, timer queues,
   TLB shootdown и work stealing;
6. **готово:** shared-memory IPC и process create/kill/wait syscalls;
7. **готово:** изолированный `vfsd`, persistent VaraniaFS и native RUNE
   resolver (interface ABI, imports, TLS, RELRO, shared RX);
8. **готов runtime S1:** upstream target `std`, CRT, threads/futex,
   process/pipes/stdio, запуск RUNE с VFS и масштабируемая COW VaraniaFS;
   дальше постоянный supervisor, native Rust toolchain и package/build services;
9. **переходно:** persistent ring-3 `vfsd`, `renderd`, `displayd` и
   `compositord` готовы;
   displayd эксклюзивно владеет scanout, atomic present/vblank feedback и
   bounded supervisor restart проходят boot-test; дальше оконные surface
   queues, постоянный input service и terminal как отдельный процесс;
10. supervisor переносит restart policy из bootstrap pair в service manifests.

UI API уже отделён от framebuffer ownership, поэтому widget logic не должна
переписываться при замене прямых вызовов IPC-сообщениями.

## Масштаб памяти

On-disk offsets и физические адреса используют `u64`. Frame allocator уже
хранит ограниченный набор свободных extent'ов из usable Multiboot descriptors, а
не bitmap до максимального MMIO-адреса. Поэтому терабайты RAM не требуют
терабайтно-пропорциональной служебной таблицы. Будущий NUMA allocator разделит
эти extent'ы по node/zone, не меняя 64-битный ABI.

Детальный контракт и честная карта следующего этапа находятся в
[MICROKERNEL.md](MICROKERNEL.md).

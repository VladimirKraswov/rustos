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
                                  |-- GOP renderer
                                  |-- PS/2 input
                                  |-- UI components
                                  |-- window manager
                                  `-- desktop + terminal
```

Ранний GUI-сеанс работает на CPU0 и не использует heap. Unsafe MMIO и port I/O
сосредоточены в `graphics` и `arch`; widget/window/application logic написана
без unsafe.

## Путь к микроядру

Текущий desktop — проверочный вертикальный срез, а не финальная граница
привилегий. Перенос выполняется в следующем порядке:

1. собственные GDT/TSS/IDT и обработчики исключений;
2. physical allocator и kernel-owned address spaces;
3. процессы ring 3 и вытесняющий scheduler;
4. capability handles, sync/async IPC и shared memory;
5. `inputd`, `displayd`, compositor и terminal как отдельные процессы;
6. supervisor перезапускает упавшие сервисы без остановки системы.

UI API уже отделён от framebuffer ownership, поэтому widget logic не должна
переписываться при замене прямых вызовов IPC-сообщениями.

## Масштаб памяти

On-disk offsets и физические адреса используют `u64`. Bootstrap allocator не
создаёт массив размером до максимального MMIO-адреса. Полноценный frame
allocator будет сегментирован по usable descriptors, что позволяет работать
с терабайтными конфигурациями без гигантского непрерывного bitmap.

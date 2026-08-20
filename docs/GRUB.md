# Загрузка RustOS через GRUB

## Цепочка загрузки

AMD64-версия использует стандартный GRUB 2 и Multiboot2:

```text
x86_64 UEFI / OVMF
        ↓
EFI/BOOT/BOOTX64.EFI (standalone GRUB)
        ↓
kernel ELF64 ET_EXEC + initramfs module
        ↓
Multiboot2 tags → BootInfo v3 → Rust kernel
```

GRUB упаковывается как standalone EFI-приложение. Kernel, initramfs и
`grub.cfg` находятся в его read-only memdisk, поэтому загрузка не зависит от
реализации FAT-драйвера ядра. Постоянный VaraniaFS-диск остаётся отдельным
virtio block device.

## Multiboot2 entry

Файл `kernel/src/arch/x86_64/multiboot2.rs` содержит маленькую 32-битную
прелюдию и безопасный Rust-разборщик. Прелюдия:

1. сохраняет magic и адрес Multiboot information;
2. устанавливает собственную GDT;
3. временно отображает первые 4 GiB страницами по 2 MiB;
4. включает AMD64 long mode;
5. переходит на 512-KiB boot stack и вызывает Rust.

Разборщик проверяет границы каждого тега и переводит modules, memory map,
framebuffer и ACPI RSDP в общий `BootInfo`. Диапазоны kernel, initramfs и
самой структуры GRUB вырезаются из usable memory, прежде чем карту увидит
physical frame allocator.

Полный identity map строится заново. На современных CPU с 1-GiB pages он
покрывает физические адреса до 128 TiB: для PML4 и 256 PDPT достаточно чуть
больше 1 MiB. Статический pool равен 2 MiB, поэтому не раздувает BSS и даёт
GRUB загрузить систему уже при 128 MiB RAM. Если CPU не поддерживает 1-GiB
pages, используются 2-MiB pages; этот совместимый путь покрывает примерно
0,5 TiB. Превышение его явного бюджета приводит к ранней диагностической
остановке, а не к повреждению памяти.

## Видеорежим

GRUB получает режимы от UEFI video backend. Первый пункт меню пытается
установить 1600×900×32, затем другие широкоформатные режимы и только потом
`auto`. Отдельные пункты позволяют выбрать auto/EDID, 1920×1080, 1600×900 и
1280×720. Реально выбранные width, height, pitch и RGB masks приходят в
Multiboot2 framebuffer tag; ядро никогда не предполагает фиксированный pitch.

Некоторые firmware, включая используемую сборку OVMF, публикуют только
1280×800×32. Это тоже широкоформатный 16:10 режим и корректный fallback:
меню не обещает режим, которого нет у firmware/монитора.

После Multiboot hand-off firmware framebuffer не предоставляет mode-set или
VSync API. Поэтому без подходящего display device смена физического
режима по-прежнему требует restart и выбора пункта GRUB. В QEMU после
загрузки этот fallback заменяется native `virtio-gpu` scanout: он читает
EDID и применяет `DISPLAY MODE WxH` без перезапуска. Compositor при этом
не зависит ни от GRUB, ни от конкретного scanout backend.

## Сборка на Linux и macOS M1

На Linux используются системные `grub-file` и `grub-mkstandalone`, если они
установлены. На macOS GRUB host tools отсутствуют, поэтому
`scripts/build-grub.sh` автоматически использует небольшой кэшируемый Debian
container с закреплённым base-image digest. Docker Desktop нужен только для
первого создания container image; последующие сборки используют local cache.

Итоговые артефакты:

```text
build/grub/BOOTX64.EFI  standalone GRUB + kernel + initramfs
build/esp.img           GPT/ESP-образ для UEFI/OVMF
build/system.vfs        постоянный VaraniaFS-диск
```

`scripts/build.sh` проверяет Multiboot2 header через `grub-file`, затем
собирает и read-back проверяет ESP. `scripts/test-boot.sh` дополнительно ждёт
два serial-маркера GRUB hand-off перед обычными kernel milestones.

# Правила `boot/`

Bootloader — маленькая trust boundary до kernel. Не размещай здесь policy
процессов, VFS, GUI или драйверов.

- Проверяй все внешние размеры, Multiboot/UEFI descriptors, alignments и
  арифметику адресов до записи/map.
- Архитектурный код находится в `boot/uefi/src/arch`; общий `BootInfo` меняется
  только вместе с version/size validation в загрузчике и kernel.
- Memory map не помечает MMIO, firmware, kernel, module и boot structures как
  свободную RAM.
- `unsafe` и assembler сопровождаются причиной корректности для конкретного
  режима CPU и состоянием до/после перехода.
- Не объявляй AArch64 boot рабочим, пока реальная VM не доходит до kernel marker.

Проверки: target build загрузчика и kernel, `make test-arch`; для x86 boot path
обязателен `make test-boot`.

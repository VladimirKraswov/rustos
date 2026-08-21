# AArch64 UEFI-файрмварь (AAVMF / EDK2 A-Firmware)

Эталонная ARM-платформа RustOS — QEMU `virt` под UEFI. Этот каталог хранит
«софт» загрузочной цепочки ARM, который не является кодом ОС и не
пересобирается из исходников ядра: образ UEFI-файрмваря, загружающий
`BOOTAA64.EFI` с ESP-образа.

## Что здесь

| Файл | Что это |
|---|---|
| `edk2-aarch64-code.fd.bz2` | Сжатый (bz2) read-only образ UEFI-файрмваря (AAVMF layout). |
| `SHA256SUMS` | Pinned SHA-256 архива и распакованного образа. |

Распакованный `edk2-aarch64-code.fd` (64 MiB) в репозиторий не кладётся: его
генерирует `scripts/bootstrap-arm-firmware.sh` в `build/arm-firmware/`.

## Откуда образ и почему такой

- Распакованный образ = `pc-bios/edk2-aarch64-code.fd` из **QEMU v11.1.0**
  (тег), который QEMU сам использует как эталонный AAVMF-файрмварь для
  `qemu-system-aarch64 -machine virt`.
- Это сборка EDK II (AAVMF) с «flat»-раскладкой: код занимает 64 MiB flash-
  область, NVRAM-переменные лежат в отдельной 64 MiB области (vars), которую
  QEMU отдаёт вторым pflash-устройством.
- Байт-в-байт совпадает с `edk2-aarch64-code.fd`, который Homebrew-QEMU
  кладёт в `/opt/homebrew/share/qemu/` (проверено на macOS Apple Silicon).

Проверено командой `make test-arm-boot`: firmware загружает RustOS
`BOOTAA64.EFI`, публикует Device Tree и доходит через EL1/EL0, GICv3 и PSCI
до `RING3_MILESTONE_OK`.

## Воспроизводимость и обновление

- Хеш зафиксирован в `SHA256SUMS`; `scripts/bootstrap-arm-firmware.sh`
  сверяет и архив, и результат распаковки.
- Чтобы обновить образ к новой версии QEMU:
  1. скачать `pc-bios/edk2-aarch64-code.fd.bz2` из нужного тега QEMU;
  2. заменить `edk2-aarch64-code.fd.bz2`;
  3. пересчитать хеши и обновить `SHA256SUMS`;
  4. прогнать `make bootstrap-arm` и ARM-boot smoke (см. docs/BUILDING.md).

## Лицензия

EDK II распространяется под BSD-2-Clause (см. `edk2` upstream,
`Maintainers.txt`/`LICENSE`). Образ используется как есть, без модификаций.

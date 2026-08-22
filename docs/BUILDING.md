# Сборка и тестирование

## Требования

- Rust nightly `2026-08-18` с `rust-src`, `rustfmt`, `clippy`;
- QEMU x86-64 (и `qemu-system-aarch64` для ARM-варианта);
- стандартные POSIX shell tools;
- ShellCheck (требуется для `make lint`, но не для обычной сборки).

OVMF загружается `scripts/bootstrap-ovmf.sh` из зафиксированного Debian package
и проверяется SHA-256. AAVMF для ARM уже лежит в репозитории
(`firmware/aarch64/`, SHA-256 в `SHA256SUMS`), сетевая загрузка не требуется.
На Apple Silicon интерактивный ARM-профиль использует HVF и CPU `host`; на
native AArch64 Linux при наличии `/dev/kvm` — KVM. Cross-ISA интеграционные
тесты сохраняют TCG там, где нужна воспроизводимая эмуляция.

## Цели Makefile

```text
make bootstrap   подготовить OVMF
make build/run   ARM+UTM/HVF/VirGL на Apple Silicon, AMD64 на остальных
make build-x86   явно собрать AMD64 GUI-образ
make run-x86     явно запустить AMD64 VM
make run-virgl   AMD64 + virtio-vga-gl; нужен QEMU с virglrenderer
make build-arm   собрать полный ARM-образ (kernel/RUNE/VaraniaFS/AAVMF/ESP)
make run-arm     запустить ARM-вариант (QEMU virt + UEFI)
make setup-utm-gpu создать/обновить UTM VirGL/ANGLE-Metal профиль
make run-utm-gpu собрать ARM и запустить ускоренную VM через публичный UTM API
make lint        fmt + ShellCheck + Clippy -D warnings для host/обеих ISA/UEFI
make test-host   ABI, SystemUI/assets, scheduler, loader/format/fs/tool tests
make test-arch   собрать kernel/runtime/apps для AMD64 и AArch64
make test-boot   GRUB/Multiboot2 + CPL3 RUNE/VFS/fault/reclaim test
make test-display-fallback boot без virtio-gpu: CPU/headless/firmware fallback
make test-arm-boot AAVMF + EL0/EL1/GICv3/PSCI/VFS test
make test-arm-gui AAVMF + virtio GPU + xHCI USB HID + SystemUI smoke test
make test-gui    keyboard/mouse/window framebuffer test
make test-virgl  ring-3 Mesa/VirGL Aurora 3D -> GraphicsBuffer -> scanout test
make test-utm-gpu Apple Silicon E2E: RustOS VirGL -> UTM ANGLE -> Metal
make bootstrap-mesa скачать и проверить закреплённый upstream Mesa source
make test        полный test suite
make clean       удалить генерируемые артефакты
```

Большие бинарные артефакты находятся в `build/` и `target/` и не хранятся в
Git. Итоговый EFI-диск — `build/esp.img`.

## ARM-вариант (AArch64, QEMU `virt` + UEFI)

Цепочка `make build-arm` (`scripts/build-arm.sh`) собирает для AArch64 user
ELF, преобразует их в проверяемые RUNE, статически размещает kernel по
`0x40000000`, создаёт RIFS initramfs и persistent VaraniaFS, затем собирает
UEFI loader/ESP. AAVMF берётся из `firmware/aarch64/`
(`scripts/bootstrap-arm-firmware.sh`: vendored bz2 → homebrew fallback,
SHA-256 check, zeroed 64 MiB NVRAM template).

Сборка считается успешной только после UEFI loader и проверки GPT/FAT:
`build/arm/esp-arm.img` обязан содержать `EFI/BOOT/BOOTAA64.EFI`.
`build/arm/STATUS.txt` имеет `bootloader=ok` и `esp=ok`; прежнего успешного
статуса `PARTIAL` больше нет. Основные артефакты:

- `build/arm/kernel.elf`, `build/arm/initramfs.img`, `build/arm/esp-arm.img`;
- `build/arm/system/bin/*.rune`;
- `build/arm-system.vfs` (sparse 1-GiB persistent volume);
- `build/arm-firmware/edk2-aarch64-{code,vars-template}.fd`.

`make run-arm` (`scripts/run-arm.sh`) запускает QEMU `virt` с двумя pflash
(code + runtime NVRAM), modern virtio-mmio VaraniaFS/GPU, PCI xHCI,
USB keyboard/mouse и
ESP; serial остаётся в терминале. `acpi=off` обязателен: AAVMF публикует FDT,
из которого kernel получает CPU/PSCI. GOP у этой конфигурации `BltOnly`,
поэтому после handoff собственный modern virtio-mmio GPU driver создаёт
scanout и обеспечивает runtime mode-set.

На macOS ARM default — `hvf + host`; на AArch64 Linux — `kvm + host`; fallback
— `tcg + cortex-a72`. Переопределяемые параметры: `ARM_SMP` (4),
`ARM_MEMORY_MB` (1024), `ARM_CPU_MODEL`, `ARM_ACCEL`, `RUSTOS_FULLSCREEN`
(1 на macOS).

### Ускоренный профиль Apple Silicon

`make run` на Apple Silicon эквивалентен `make run-utm-gpu`. Скрипт создаёт
VM **RustOS GPU Development** через AppleScript API UTM, использует HVF и
подключает `virtio-gpu-gl-device`. UTM передаёт VirGL в virglrenderer, а затем
в ANGLE/Metal. Образы из `build/` подключаются напрямую и не копируются в
скрытый каталог VM.

```sh
brew install --cask utm  # один раз
make run                 # обычная интерактивная VM
make test-utm-gpu        # serial E2E proof без guest CPU rasterization
```

Запуск бинарника
`UTM.app/Contents/XPCServices/QEMUHelper.xpc/.../QEMULauncher` напрямую не
поддерживается. Это sandboxed XPC helper: macOS требует, чтобы его породил
родитель UTM. Репозиторий намеренно использует только `utmctl` и публичный
AppleScript API.

`make test-arm-boot` пересобирает kernel с `boot-test`, запускает headless
QEMU и требует: Device Tree, GICv3/Generic Timer, минимум два timer tick и
context switch, `discovered=2 online=2` после настоящего PSCI `CPU_ON`,
локализованный EL0 `BRK` (EC 0x3c), IPC/ABI/std/VFS/loader milestones,
`RING3_MILESTONE_OK` и `PSCI SYSTEM_OFF`. Логи сохраняются в
`build/test-results/arm-boot/`. Параметры теста: `ARM_BOOT_TEST_TIMEOUT`,
`ARM_BOOT_MEMORY_MB`, `ARM_BOOT_CPUS`, `ARM_BOOT_CPU_MODEL`.

GUI-тест общается с QEMU monitor через workspace tool `rustos-hmp`, поэтому
не зависит от несовместимых вариантов `nc` на macOS и Linux.
`make test-arm-gui` дополнительно требует перечисление xHCI HID и настоящие
USB keyboard/mouse events,
команду terminal, открытие Start и валидный PPM screendump; артефакты лежат в
`build/test-results/arm-gui/`.

`scripts/build.sh` сначала собирает freestanding user ELF из
`userspace/bootstrap`, помещает их в generated RIFS initramfs, затем собирает
kernel и UEFI loader. Boot-тест считается успешным только если serial содержит
успешный VFS capability call, локализованный user `#UD`, полный reclaim
address space и marker `RING3_MILESTONE_OK`.

`scripts/check-architectures.sh` создаёт настоящие ELF-артефакты для
`targets/x86_64-unknown-rustos.json` и
`targets/aarch64-unknown-rustos.json`. Это compile contract переносимости;
runtime-работоспособность отдельно доказывает `make test-arm-boot`.
По умолчанию boot- и GUI-тесты запускаются с минимальным профилем 128 MiB
RAM; значения можно переопределить, например
`BOOT_MEMORY_MB=4096 make test-boot` или `GUI_MEMORY_MB=512 make test-gui`.
Boot-тест использует два vCPU и дополнительно требует успешный MADT/AP startup,
ненулевые APIC timer/context-switch counters, конкурентную fault isolation и
IPC capability transfer. Число CPU переопределяется через `BOOT_CPUS`; тест
требует, чтобы все заявленные QEMU CPU стали online. Штатный CI-профиль — два.
CPU-модель boot-теста можно переопределить через `BOOT_CPU_MODEL`; например,
`BOOT_CPU_MODEL=max,-x2apic make test-boot` отдельно проверяет MMIO fallback.

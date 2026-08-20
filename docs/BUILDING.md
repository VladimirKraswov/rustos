# Сборка и тестирование

## Требования

- Rust nightly `2026-08-18` с `rust-src`, `rustfmt`, `clippy`;
- QEMU x86-64;
- стандартные POSIX shell tools.

OVMF загружается `scripts/bootstrap-ovmf.sh` из зафиксированного Debian package
и проверяется SHA-256. На macOS используется QEMU TCG, на Linux при наличии
`/dev/kvm` автоматически выбирается KVM.

## Цели Makefile

```text
make bootstrap  подготовить OVMF
make build      собрать интерактивный GUI-образ
make run        запустить графическую VM
make lint       fmt + Clippy -D warnings
make test-host  ABI, scheduler/lifecycle и host-tool unit tests
make test-boot  UEFI + CPL3 ELF/VFS/fault/reclaim test
make test-gui   keyboard/mouse/window framebuffer test
make test       полный test suite
make clean      удалить генерируемые артефакты
```

Большие бинарные артефакты находятся в `build/` и `target/` и не хранятся в
Git. Итоговый EFI-диск — `build/esp.img`.

GUI-тест общается с QEMU monitor через workspace tool `rustos-hmp`, поэтому
не зависит от несовместимых вариантов `nc` на macOS и Linux.

`scripts/build.sh` сначала собирает freestanding user ELF из
`userspace/bootstrap`, помещает их в generated RIFS initramfs, затем собирает
kernel и UEFI loader. Boot-тест считается успешным только если serial содержит
успешный VFS capability call, локализованный user `#UD`, полный reclaim
address space и marker `RING3_MILESTONE_OK`.
По умолчанию boot- и GUI-тесты запускаются с минимальным профилем 128 MiB
RAM; значения можно переопределить, например
`BOOT_MEMORY_MB=4096 make test-boot` или `GUI_MEMORY_MB=512 make test-gui`.

# Сборка и тестирование

## Требования

- Rust nightly `2026-08-18` с `rust-src`, `rustfmt`, `clippy`;
- QEMU x86-64;
- `nc` с Unix socket support для GUI integration test;
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
make test-host  host unit tests
make test-boot  UEFI/kernel handoff test
make test-gui   keyboard/mouse/window framebuffer test
make test       полный test suite
make clean      удалить генерируемые артефакты
```

Большие бинарные артефакты находятся в `build/` и `target/` и не хранятся в
Git. Итоговый EFI-диск — `build/esp.img`.

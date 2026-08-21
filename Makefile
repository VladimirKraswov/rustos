# RustOS — учебная 64-битная микроядерная ОС.
#
#   make build     — ядро + загрузчик + ESP-образ + OVMF (полная сборка, x86_64)
#   make run       — запустить VM интерактивно (serial — консоль; выход Ctrl-A, X)
#   make build-arm — полный ARM-образ: kernel/RUNE/initramfs/VaraniaFS/AAVMF/ESP
#   make run-arm   — запустить ARM-вариант (QEMU virt + UEFI)
#   make test-arm-boot — headless ARM EL1/EL0/GIC/PSCI integration test
#   make test      — все host/cross/x86/ARM/GUI проверки
#   make clean     — убрать артефакты

SHELL := /bin/bash

.PHONY: all bootstrap build run boot test test-host test-arch test-boot test-arm \
        test-arm-boot test-gui bootstrap-arm build-arm run-arm format lint clean

all: build

bootstrap:
	bash scripts/bootstrap-ovmf.sh

build:
	bash scripts/build.sh

run: build
	bash scripts/run.sh

boot: run

bootstrap-arm:
	bash scripts/bootstrap-arm-firmware.sh

build-arm:
	bash scripts/build-arm.sh

run-arm: build-arm
	bash scripts/run-arm.sh

test: test-host test-arch test-boot test-arm-boot test-gui

test-host:
	cargo test -p rustos-abi -p rustos-microkernel -p rustos-video -p rustos-pack -p rustos-image -p rustos-gui-check -p rustos-hmp -p varaniafs -p rustos-vfs-image

test-arch:
	bash scripts/check-architectures.sh

test-boot:
	RUSTOS_BOOT_TEST=1 bash scripts/build.sh
	bash scripts/test-boot.sh

# Короткий alias удобен локально; полное имя явно отличает boot integration
# от compile-only `test-arch`.
test-arm: test-arm-boot

test-arm-boot:
	RUSTOS_BOOT_TEST=1 bash scripts/build-arm.sh
	bash scripts/test-arm.sh

test-gui:
	bash scripts/test-gui.sh

format:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy -p rustos-abi -p rustos-microkernel -p rustos-video -p rustos-pack -p rustos-image -p rustos-gui-check -p rustos-hmp -p varaniafs -p rustos-vfs-image --all-targets -- -D warnings
	cargo clippy -Zjson-target-spec -Zbuild-std=core,alloc -p rustos-kernel -p rustos-runtime -p rustos-bootstrap-apps -p rustos-vfs-client -p rustos-vfs-dll -p rustos-elf-loader --target targets/x86_64-unknown-rustos.json -- -D warnings
	cargo clippy -Zjson-target-spec -Zbuild-std=core,alloc -p rustos-kernel -p rustos-runtime -p rustos-bootstrap-apps -p rustos-vfs-client -p rustos-vfs-dll -p rustos-elf-loader --target targets/aarch64-unknown-rustos.json -- -D warnings
	cargo clippy -p rustos-boot --target x86_64-unknown-uefi -- -D warnings
	cargo clippy -p rustos-boot --target aarch64-unknown-uefi -- -D warnings

clean:
	cargo clean
	rm -rf build boot/uefi/payload

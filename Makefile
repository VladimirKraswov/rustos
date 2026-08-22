# RustOS — учебная 64-битная микроядерная ОС.
#
#   make build/run — ARM+HVF на Apple Silicon, x86_64 на остальных хостах
#   make build-x86/run-x86 — явно собрать/запустить профиль AMD64
#   make build-arm — полный ARM-образ: kernel/RUNE/initramfs/VaraniaFS/AAVMF/ESP
#   make run-arm   — запустить ARM-вариант (QEMU virt + UEFI)
#   make test-arm-boot — headless ARM EL1/EL0/GIC/PSCI integration test
#   make test      — все host/cross/x86/ARM/GUI проверки
#   make clean     — убрать артефакты

SHELL := /bin/bash

# Явные группы не дают новому crate незаметно выпасть из CI. Host-набор
# содержит библиотеки с unit tests и host tools; RustOS-набор — все прямые
# freestanding packages, которые Clippy обязан проверить для обеих ISA.
HOST_CRATES := \
	rustos-abi rustos-microkernel rustos-video rustos-compositor rustos-system-ui \
	rustos-system-assets rustos-system-fonts rustos-rune-format rustos-runtime rustos-vfs-client rustos-surface-client \
	rustos-package-registry rustos-package-registry-tool \
	rustos-elf-loader rustos-rune-loader rustos-virgl rustos-mesa rustos-usb varaniafs rustos-image rustos-pack \
	rustos-ui-gpu rustos-gui-check rustos-hmp rustos-vfs-image rustos-rune rustos-ruidl \
	rustos-ruidl-compiler rustos-rui rustos-wallpaper-pack
HOST_CRATE_ARGS := $(addprefix -p ,$(HOST_CRATES))

DOC_CRATES := \
	rustos-abi rustos-microkernel rustos-video rustos-compositor rustos-system-ui \
	rustos-system-assets rustos-system-fonts rustos-rune-format rustos-runtime rustos-vfs-client rustos-surface-client \
	rustos-package-registry \
	rustos-elf-loader rustos-rune-loader rustos-ruidl rustos-ruidl-compiler rustos-virgl \
	rustos-ui-gpu rustos-mesa rustos-usb varaniafs
DOC_CRATE_ARGS := $(addprefix -p ,$(DOC_CRATES))

RUSTOS_CRATES := \
	rustos-kernel rustos-runtime rustos-crt rustos-bootstrap-apps rustos-compositor \
	rustos-system-fonts \
	rustos-vfs-client rustos-vfs-dll rustos-surface-client rustos-elf-loader rustos-rune-loader \
	rustos-package-registry rustos-virgl rustos-ui-gpu rustos-mesa
RUSTOS_CRATE_ARGS := $(addprefix -p ,$(RUSTOS_CRATES))

HOST_SYSTEM := $(shell uname -s)
HOST_MACHINE := $(shell uname -m)
DEFAULT_BUILD_TARGET := build-x86
DEFAULT_RUN_TARGET := run-x86
DEFAULT_SDK_LIBRARY := build/rune-system/lib/vfs-1.rune
DEFAULT_SDK_TARGET := x86_64-unknown-rustos
ifeq ($(HOST_SYSTEM),Darwin)
ifneq ($(filter arm64 aarch64,$(HOST_MACHINE)),)
DEFAULT_BUILD_TARGET := build-arm
DEFAULT_RUN_TARGET := run-utm-gpu
DEFAULT_SDK_LIBRARY := build/arm-rune-system/lib/vfs-1.rune
DEFAULT_SDK_TARGET := aarch64-unknown-rustos
endif
endif

.PHONY: all bootstrap build build-x86 run run-x86 boot test test-host test-arch sdk-resolve \
        test-boot test-arm test-arm-boot test-arm-gui test-gui bootstrap-arm build-arm \
        bootstrap-mesa run-arm run-utm-gpu setup-utm-gpu run-virgl test-virgl \
        test-utm-gpu test-display-fallback format lint clean

all: build

bootstrap:
	bash scripts/bootstrap-ovmf.sh

bootstrap-mesa:
	bash scripts/bootstrap-mesa.sh

sdk-resolve:
	@test -f $(DEFAULT_SDK_LIBRARY) || { echo "Сначала выполните make build" >&2; exit 1; }
	@mkdir -p build/sdk-cache
	cargo run -q -p rustos-ruidl-compiler --bin rustos-ruidl -- resolve $(DEFAULT_SDK_LIBRARY) build/sdk-cache $(DEFAULT_SDK_TARGET)

build: $(DEFAULT_BUILD_TARGET)

build-x86:
	bash scripts/build.sh

run: $(DEFAULT_RUN_TARGET)

run-x86: build-x86
	bash scripts/run.sh

run-virgl: build-x86
	bash scripts/run-virgl.sh

test-virgl:
	RUSTOS_VIRGL_TEST=1 bash scripts/build.sh
	bash scripts/test-virgl.sh

boot: run

bootstrap-arm:
	bash scripts/bootstrap-arm-firmware.sh

build-arm:
	bash scripts/build-arm.sh

run-arm: build-arm
	bash scripts/run-arm.sh

setup-utm-gpu:
	bash scripts/setup-utm-gpu.sh

run-utm-gpu:
	bash scripts/run-utm-gpu.sh

test-utm-gpu:
	bash scripts/test-utm-gpu.sh

test: test-host test-arch test-boot test-arm-boot test-arm-gui test-gui

test-host:
	cargo test $(HOST_CRATE_ARGS)

test-arch:
	bash scripts/check-architectures.sh

test-boot:
	RUSTOS_BOOT_TEST=1 bash scripts/build.sh
	bash scripts/test-boot.sh

test-display-fallback:
	RUSTOS_BOOT_TEST=1 bash scripts/build.sh
	BOOT_DISPLAY_BACKEND=firmware bash scripts/test-boot.sh

# Короткий alias удобен локально; полное имя явно отличает boot integration
# от compile-only `test-arch`.
test-arm: test-arm-boot

test-arm-boot:
	RUSTOS_BOOT_TEST=1 bash scripts/build-arm.sh
	bash scripts/test-arm.sh

test-arm-gui:
	bash scripts/test-arm-gui.sh

test-gui:
	bash scripts/test-gui.sh

format:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	shellcheck -x scripts/*.sh
	cargo clippy $(HOST_CRATE_ARGS) --all-targets -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps $(DOC_CRATE_ARGS)
	cargo clippy -Zjson-target-spec -Zbuild-std=core,alloc $(RUSTOS_CRATE_ARGS) --target targets/x86_64-unknown-rustos.json -- -D warnings
	cargo clippy -Zjson-target-spec -Zbuild-std=core,alloc $(RUSTOS_CRATE_ARGS) --target targets/aarch64-unknown-rustos.json -- -D warnings
	cargo clippy -p rustos-boot --target x86_64-unknown-uefi -- -D warnings
	cargo clippy -p rustos-boot --target aarch64-unknown-uefi -- -D warnings

clean:
	cargo clean
	rm -rf build boot/uefi/payload

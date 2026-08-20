# RustOS — учебная 64-битная микроядерная ОС.
#
#   make build     — ядро + загрузчик + ESP-образ + OVMF (полная сборка)
#   make run       — запустить VM интерактивно (serial — консоль; выход Ctrl-A, X)
#   make test      — boot-тест: serial-лог + ожидаемые строки; ядро exits через 0xF4
#   make clean     — убрать артефакты

SHELL := /bin/bash

.PHONY: all bootstrap build run boot test test-host test-boot test-gui format lint clean

all: build

bootstrap:
	bash scripts/bootstrap-ovmf.sh

build:
	bash scripts/build.sh

run: build
	bash scripts/run.sh

boot: run

test: test-host test-boot test-gui

test-host:
	cargo test -p rustos-abi -p rustos-microkernel -p rustos-video -p rustos-pack -p rustos-image -p rustos-gui-check -p rustos-hmp

test-boot:
	RUSTOS_BOOT_TEST=1 bash scripts/build.sh
	bash scripts/test-boot.sh

test-gui:
	bash scripts/test-gui.sh

format:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy -p rustos-abi -p rustos-microkernel -p rustos-video -p rustos-pack -p rustos-image -p rustos-gui-check -p rustos-hmp --all-targets -- -D warnings
	cargo clippy -Zjson-target-spec -Zbuild-std=core -p rustos-kernel -p rustos-runtime -p rustos-bootstrap-apps --target targets/x86_64-unknown-rustos.json -- -D warnings
	cargo clippy -p rustos-boot --target x86_64-unknown-uefi -- -D warnings

clean:
	cargo clean
	rm -rf build boot/uefi/payload

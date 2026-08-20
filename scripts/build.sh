#!/usr/bin/env bash
# Полная сборка: ядро → staging payload → initramfs (RIFS) → загрузчик → OVMF → ESP.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "[build] 1/6 kernel (x86_64-unknown-rustos, build-std=core)"
if [[ "${RUSTOS_BOOT_TEST:-0}" == "1" ]]; then
    echo "[build] kernel mode: boot-test"
    cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json --features boot-test
else
    echo "[build] kernel mode: interactive GUI"
    cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json
fi

echo "[build] 2/6 staging: boot/uefi/payload/kernel.elf"
mkdir -p boot/uefi/payload
cp -f target/x86_64-unknown-rustos/debug/rustos-kernel boot/uefi/payload/kernel.elf

echo "[build] 3/6 initramfs (RIFS v1)"
cargo run -q -p rustos-pack -- boot/initramfs boot/uefi/payload/initramfs.img

echo "[build] 4/6 UEFI bootloader (x86_64-unknown-uefi)"
cargo build -p rustos-boot --target x86_64-unknown-uefi

echo "[build] 5/6 OVMF"
bash scripts/bootstrap-ovmf.sh

echo "[build] 6/6 ESP image (GPT + FAT32 + EFI/BOOT/BOOTX64.EFI)"
mkdir -p build
cargo run -q -p rustos-image -- target/x86_64-unknown-uefi/debug/rustos-boot.efi build/esp.img
cargo run -q -p rustos-image -- --verify build/esp.img target/x86_64-unknown-uefi/debug/rustos-boot.efi

echo "[build] OK: build/esp.img + build/ovmf/"

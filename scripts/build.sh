#!/usr/bin/env bash
# Полная сборка: user ELF → ядро → initramfs staging → загрузчик → OVMF → ESP.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "[build] 1/8 bootstrap user ELF64 (ring 3)"
CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-bootstrap-apps --target targets/x86_64-unknown-rustos.json

echo "[build] 2/8 kernel (x86_64-unknown-rustos, build-std=core)"
if [[ "${RUSTOS_BOOT_TEST:-0}" == "1" ]]; then
    echo "[build] kernel mode: boot-test"
    cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json --features boot-test
else
    echo "[build] kernel mode: interactive GUI"
    cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json
fi

echo "[build] 3/8 staging: kernel + initramfs root"
mkdir -p boot/uefi/payload
cp -f target/x86_64-unknown-rustos/debug/rustos-kernel boot/uefi/payload/kernel.elf
STAGE="$ROOT/build/initramfs-root"
rm -rf "$STAGE"
mkdir -p "$STAGE/system/bin"
cp -R boot/initramfs/. "$STAGE/"
cp -f target/x86_64-unknown-rustos/debug/rustos-init "$STAGE/system/bin/init.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-fault-test \
    "$STAGE/system/bin/fault-test.elf"

echo "[build] 4/8 initramfs (RIFS v1)"
cargo run -q -p rustos-pack -- "$STAGE" boot/uefi/payload/initramfs.img
cargo run -q -p rustos-pack -- --verify boot/uefi/payload/initramfs.img

echo "[build] 5/8 UEFI bootloader (x86_64-unknown-uefi)"
cargo build -p rustos-boot --target x86_64-unknown-uefi

echo "[build] 6/8 OVMF"
bash scripts/bootstrap-ovmf.sh

echo "[build] 7/8 ESP image (GPT + FAT32 + EFI/BOOT/BOOTX64.EFI)"
mkdir -p build
cargo run -q -p rustos-image -- target/x86_64-unknown-uefi/debug/rustos-boot.efi build/esp.img
echo "[build] 8/8 verify ESP image"
cargo run -q -p rustos-image -- --verify build/esp.img target/x86_64-unknown-uefi/debug/rustos-boot.efi

echo "[build] OK: build/esp.img + build/ovmf/"

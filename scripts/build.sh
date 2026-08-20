#!/usr/bin/env bash
# Полная сборка: user ELF → ядро → initramfs staging → загрузчик → OVMF → ESP.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "[build] 1/9 user ELF64, dynamic-loader fixtures and system libraries"
RUSTFLAGS="-Z tls-model=initial-exec" CARGO_PROFILE_DEV_DEBUG=0 \
    cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-loader-fixture --target targets/x86_64-unknown-rustos.json
RUSTOS_DLL_DIR="$ROOT/target/x86_64-unknown-rustos/debug" \
    CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-loader-root --target targets/x86_64-unknown-rustos.json
CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-bootstrap-apps -p rustos-vfs-client --target targets/x86_64-unknown-rustos.json
CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-vfs-dll --target targets/x86_64-unknown-rustos.json

echo "[build] 2/9 kernel (x86_64-unknown-rustos, build-std=core)"
if [[ "${RUSTOS_BOOT_TEST:-0}" == "1" ]]; then
    echo "[build] kernel mode: boot-test"
    cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json --features boot-test
else
    echo "[build] kernel mode: interactive GUI"
    cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json
fi

echo "[build] 3/9 staging: kernel + initramfs root"
mkdir -p boot/uefi/payload
cp -f target/x86_64-unknown-rustos/debug/rustos-kernel boot/uefi/payload/kernel.elf
STAGE="$ROOT/build/initramfs-root"
rm -rf "$STAGE"
mkdir -p "$STAGE/system/bin" "$STAGE/system/lib"
cp -R boot/initramfs/. "$STAGE/"
cp -f target/x86_64-unknown-rustos/debug/rustos-init "$STAGE/system/bin/init.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-fault-test \
    "$STAGE/system/bin/fault-test.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-preempt-a \
    "$STAGE/system/bin/preempt-a.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-preempt-b \
    "$STAGE/system/bin/preempt-b.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-ipc-receiver \
    "$STAGE/system/bin/ipc-receiver.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-ipc-sender \
    "$STAGE/system/bin/ipc-sender.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-abi-lifecycle \
    "$STAGE/system/bin/abi-lifecycle.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-abi-child \
    "$STAGE/system/bin/abi-child.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-vfsd "$STAGE/system/bin/vfsd.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-vfs-test "$STAGE/system/bin/vfs-test.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-vfs-persistence \
    "$STAGE/system/bin/vfs-persistence.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-loader-test \
    "$STAGE/system/bin/loader-test.elf"
cp -f target/x86_64-unknown-rustos/debug/rustos-loader-child \
    "$STAGE/system/bin/loader-child.elf"
# Это ELF64 ET_DYN с unmangled C exports. Bootstrap test пока статически
# использует тот же client crate; dynamic loader подключит этот образ без
# изменения API приложения.
cp -f target/x86_64-unknown-rustos/debug/vfs_1.dll \
    "$STAGE/system/lib/vfs-1.dll"

echo "[build] 4/9 initramfs (RIFS v1)"
cargo run -q -p rustos-pack -- "$STAGE" boot/uefi/payload/initramfs.img
cargo run -q -p rustos-pack -- --verify boot/uefi/payload/initramfs.img

echo "[build] 5/9 persistent VaraniaFS volume (kept between interactive boots)"
mkdir -p build
cargo run -q -p rustos-vfs-image -- build/system.vfs 64
cargo run -q -p rustos-vfs-image -- --put build/system.vfs \
    target/x86_64-unknown-rustos/debug/fixture_1.dll /system/lib/fixture-1.dll
cargo run -q -p rustos-vfs-image -- --put build/system.vfs \
    target/x86_64-unknown-rustos/debug/loader_test_root.dll /apps/loader-test/root.elf
cargo run -q -p rustos-vfs-image -- --verify build/system.vfs

echo "[build] 6/9 UEFI bootloader (x86_64-unknown-uefi)"
cargo build -p rustos-boot --target x86_64-unknown-uefi

echo "[build] 7/9 OVMF"
bash scripts/bootstrap-ovmf.sh

echo "[build] 8/9 ESP image (GPT + FAT32 + EFI/BOOT/BOOTX64.EFI)"
mkdir -p build
cargo run -q -p rustos-image -- target/x86_64-unknown-uefi/debug/rustos-boot.efi build/esp.img
echo "[build] 9/9 verify ESP image"
cargo run -q -p rustos-image -- --verify build/esp.img target/x86_64-unknown-uefi/debug/rustos-boot.efi

echo "[build] OK: build/esp.img + build/ovmf/"

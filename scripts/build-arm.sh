#!/usr/bin/env bash
# Полная сборка ARM (AArch64) варианта RustOS — QEMU `virt` + UEFI (AAVMF).
#
# Зеркало scripts/build.sh (x86_64) для ARM-варианта. Собирает всё, что
# поддерживает текущий код ОС:
#   1) bootstrap user ELFs (ring 3) под targets/aarch64-unknown-rustos.json;
#   2) проверяемые native RUNE-контейнеры AArch64;
#   3) статически размещённое kernel под тот же target;
#   4) staging в build/arm/ + boot/uefi/payload/ (include_bytes! загрузчика);
#   5) initramfs RIFS и persistent VaraniaFS;
#   6) AAVMF UEFI-файрмварь (build/arm-firmware/);
#   7) UEFI-загрузчик под aarch64-unknown-uefi;
#   8) GPT/ESP с каноническим `EFI/BOOT/BOOTAA64.EFI`.
#
# Артефакты: build/arm/{kernel.elf,initramfs.img,system/bin/*.rune,
#                STATUS.txt,esp-arm.img}, build/arm-system.vfs и firmware.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARM_TARGET="targets/aarch64-unknown-rustos.json"
ARM_UEFI_TARGET="aarch64-unknown-uefi"
ARM_OUT="$ROOT/build/arm"
ARM_RUNE_ROOT="$ROOT/build/arm-rune-system"
ARM_RUNE_DIR="$ARM_RUNE_ROOT/bin"
ARM_RUNE_LIB_DIR="$ARM_RUNE_ROOT/lib"
ARM_RUNE_APP_DIR="$ARM_RUNE_ROOT/apps"
BOOTLOG="$ARM_OUT/bootloader-build.log"

echo "[build-arm] 1/8 bootstrap user ELFs (aarch64, ring 3)"
RUSTFLAGS="-Z tls-model=initial-exec" CARGO_PROFILE_DEV_DEBUG=0 \
    cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-loader-fixture --target "$ARM_TARGET"
RUSTOS_DLL_DIR="$ROOT/target/aarch64-unknown-rustos/debug" \
    CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-loader-root --target "$ARM_TARGET"
CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core,alloc build \
    -p rustos-bootstrap-apps -p rustos-vfs-client --target "$ARM_TARGET"
CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-vfs-dll --target "$ARM_TARGET"
bash scripts/build-std.sh build -p rustos-bootstrap-apps \
    --bin rustos-std-smoke --bin rustos-std-main --bin rustos-std-child \
    --features std-port --target "$ARM_TARGET"
bash scripts/build-std.sh build -p rustos-rune --target "$ARM_TARGET"
bash scripts/build-std.sh build -p rustos-sdk-hello --target "$ARM_TARGET"
STD_TARGET_DIR="$(bash scripts/rustos-std-target-dir.sh)"

echo "[build-arm] 2/8 AArch64 ELF -> native RUNE"
rm -rf "$ARM_RUNE_ROOT"
mkdir -p "$ARM_RUNE_DIR" "$ARM_RUNE_LIB_DIR" "$ARM_RUNE_APP_DIR"
for program in \
    init fault-test preempt-a preempt-b ipc-receiver ipc-sender \
    abi-lifecycle abi-child displayd compositord renderd gpu-demo surface-test vfsd vfs-test vfs-persistence \
    loader-test loader-child rune-runner
do
    cargo run -q -p rustos-rune -- \
        "$ROOT/target/aarch64-unknown-rustos/debug/rustos-$program" \
        "$ARM_RUNE_DIR/$program.rune"
    cargo run -q -p rustos-rune -- verify "$ARM_RUNE_DIR/$program.rune"
done
for program in std-smoke std-main std-child rune
do
    cargo run -q -p rustos-rune -- \
        "$STD_TARGET_DIR/aarch64-unknown-rustos/debug/rustos-$program" \
        "$ARM_RUNE_DIR/$program.rune"
    cargo run -q -p rustos-rune -- verify "$ARM_RUNE_DIR/$program.rune"
done
cargo run -q -p rustos-rune -- pack-manifest \
    target/aarch64-unknown-rustos/debug/fixture_1.dll \
    "$ARM_RUNE_LIB_DIR/fixture-1.rune" sdk/abi/fixture-answer.rune-abi
cargo run -q -p rustos-rune -- pack-manifest \
    target/aarch64-unknown-rustos/debug/loader_test_root.dll \
    "$ARM_RUNE_LIB_DIR/loader-root.rune" sdk/abi/loader-root.rune-abi
cargo run -q -p rustos-rune -- pack-manifest \
    target/aarch64-unknown-rustos/debug/vfs_1.dll \
    "$ARM_RUNE_LIB_DIR/vfs-1.rune" sdk/abi/vfs-1.rune-abi
for library in "$ARM_RUNE_LIB_DIR/"*.rune
do
    cargo run -q -p rustos-rune -- verify "$library"
done
RUIDL_CACHE="$ROOT/build/sdk-cache"
mkdir -p "$RUIDL_CACHE"
for library in fixture-1.rune vfs-1.rune
do
    cargo run -q -p rustos-ruidl-compiler --bin rustos-ruidl -- resolve \
        "$ARM_RUNE_LIB_DIR/$library" "$RUIDL_CACHE" aarch64-unknown-rustos >/dev/null
done
cargo run -q -p rustos-rune -- pack-manifest \
    "$STD_TARGET_DIR/aarch64-unknown-rustos/debug/rustos-sdk-hello" \
    "$ARM_RUNE_APP_DIR/hello.rune" sdk/examples/hello/hello.rune-abi
cargo run -q -p rustos-rune -- verify "$ARM_RUNE_APP_DIR/hello.rune"

echo "[build-arm] 3/8 kernel (aarch64-unknown-rustos, build-std=core)"
# Только ядро имеет фиксированный физический layout (зеркало x86:
# scripts/build.sh). Пользовательские программы выше собраны PIE и
# продолжают загружаться RUNE-loader'ом.
KERNEL_RUSTFLAGS="-C force-unwind-tables=no -C relocation-model=static -C link-arg=-no-pie -C link-arg=-T$ROOT/kernel/aarch64-uefi.ld"
if [[ "${RUSTOS_VIRGL_TEST:-0}" == "1" ]]; then
    echo "[build-arm] kernel mode: virgl-test"
    CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$KERNEL_RUSTFLAGS" \
        cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target "$ARM_TARGET" --features virgl-test
elif [[ "${RUSTOS_BOOT_TEST:-0}" == "1" ]]; then
    echo "[build-arm] kernel mode: boot-test"
    CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$KERNEL_RUSTFLAGS" \
        cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target "$ARM_TARGET" --features boot-test
else
    echo "[build-arm] kernel mode: interactive GUI"
    CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$KERNEL_RUSTFLAGS" \
        cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target "$ARM_TARGET"
fi

echo "[build-arm] 4/8 staging: build/arm/ + boot/uefi/payload (include_bytes!)"
rm -rf "$ARM_OUT"
mkdir -p "$ARM_OUT/system/bin" "$ROOT/boot/uefi/payload"
cp -f target/aarch64-unknown-rustos/debug/rustos-kernel "$ARM_OUT/kernel.elf"
cp -f "$ARM_OUT/kernel.elf" "$ROOT/boot/uefi/payload/kernel.elf"
STAGE="$ARM_OUT/initramfs-root"
mkdir -p "$STAGE/system/bin" "$STAGE/system/lib"
cp -R boot/initramfs/. "$STAGE/"
cp -f "$ARM_RUNE_DIR/"*.rune "$STAGE/system/bin/"
cp -f "$ARM_RUNE_LIB_DIR/vfs-1.rune" "$STAGE/system/lib/vfs-1.rune"
cp -f "$ARM_RUNE_DIR/"*.rune "$ARM_OUT/system/bin/"

echo "[build-arm] 5/8 initramfs (RIFS v1)"
cargo run -q -p rustos-pack -- "$STAGE" "$ROOT/boot/uefi/payload/initramfs.img"
cargo run -q -p rustos-pack -- --verify "$ROOT/boot/uefi/payload/initramfs.img"
cp -f "$ROOT/boot/uefi/payload/initramfs.img" "$ARM_OUT/initramfs.img"

echo "[build-arm] persistent VaraniaFS volume for virtio-mmio block"
cargo run -q -p rustos-vfs-image -- "$ROOT/build/arm-system.vfs" 1024
cargo run -q -p rustos-vfs-image -- --grow "$ROOT/build/arm-system.vfs" 1024
cargo run -q -p rustos-vfs-image -- --put "$ROOT/build/arm-system.vfs" \
    "$ARM_RUNE_LIB_DIR/fixture-1.rune" /system/lib/fixture-1.rune
cargo run -q -p rustos-vfs-image -- --put "$ROOT/build/arm-system.vfs" \
    "$ARM_RUNE_LIB_DIR/loader-root.rune" /apps/loader-test/root.rune
cargo run -q -p rustos-vfs-image -- --put "$ROOT/build/arm-system.vfs" \
    "$ARM_RUNE_DIR/std-child.rune" /apps/sdk/std-child.rune
cargo run -q -p rustos-vfs-image -- --put "$ROOT/build/arm-system.vfs" \
    "$ARM_RUNE_APP_DIR/hello.rune" /apps/examples/hello.rune
cargo run -q -p rustos-vfs-image -- --verify "$ROOT/build/arm-system.vfs"

echo "[build-arm] 6/8 AAVMF UEFI firmware (64 MiB code + 64 MiB vars-template)"
bash scripts/bootstrap-arm-firmware.sh

echo "[build-arm] 7/8 UEFI bootloader (aarch64-unknown-uefi)"
cargo build -p rustos-boot --target "$ARM_UEFI_TARGET" 2>&1 | tee "$BOOTLOG"
EFI="$ROOT/target/$ARM_UEFI_TARGET/debug/rustos-boot.efi"

echo "[build-arm] 8/8 ESP image (GPT + FAT32 + EFI/BOOT/BOOTAA64.EFI)"
cargo run -q -p rustos-image -- "$EFI" "$ARM_OUT/esp-arm.img" --efi-name BOOTAA64.EFI
cargo run -q -p rustos-image -- --verify "$ARM_OUT/esp-arm.img" "$EFI" --efi-name BOOTAA64.EFI

# Machine-readable статус для будущих ARM-boot integration-тестов.
{
    echo "kernel.elf=ok"
    echo "initramfs.img=ok"
    echo "user_elfs=ok"
    echo "rune=ok"
    echo "rust_std=ok"
    echo "firmware=ok"
    echo "system_disk=ok"
    echo "bootloader=ok"
    echo "esp=ok"
} > "$ARM_OUT/STATUS.txt"

echo "[build-arm] OK: $ARM_OUT/esp-arm.img (BOOTAA64.EFI) + kernel/RUNE/initramfs/VaraniaFS/firmware"

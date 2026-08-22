#!/usr/bin/env bash
# Полная сборка: user ELF → fixed Multiboot2 kernel → initramfs → GRUB → ESP.
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
CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core,alloc build \
    -p rustos-bootstrap-apps -p rustos-vfs-client --target targets/x86_64-unknown-rustos.json
CARGO_PROFILE_DEV_DEBUG=0 cargo -Zjson-target-spec -Zbuild-std=core build \
    -p rustos-vfs-dll --target targets/x86_64-unknown-rustos.json
bash scripts/build-std.sh build -p rustos-bootstrap-apps \
    --bin rustos-std-smoke --bin rustos-std-main --bin rustos-std-child --features std-port \
    --target targets/x86_64-unknown-rustos.json
bash scripts/build-std.sh build -p rustos-rune \
    --target targets/x86_64-unknown-rustos.json
bash scripts/build-std.sh build -p rustos-sdk-hello \
    --target targets/x86_64-unknown-rustos.json
STD_TARGET_DIR="$(bash scripts/rustos-std-target-dir.sh)"

# ELF остаётся удобным промежуточным форматом rustc/lld, но в system image
# попадают только проверяемые нативные контейнеры RUNE. Kernel ELF сохраняется:
# его до запуска RustOS читает GRUB, а не process manager.
RUNE_DIR="$ROOT/build/rune-system/bin"
RUNE_LIB_DIR="$ROOT/build/rune-system/lib"
RUNE_APP_DIR="$ROOT/build/rune-system/apps"
mkdir -p "$RUNE_DIR" "$RUNE_LIB_DIR" "$RUNE_APP_DIR"
for program in \
    init fault-test preempt-a preempt-b ipc-receiver ipc-sender \
    abi-lifecycle abi-child displayd compositord renderd gpu-demo surface-test vfsd vfs-test vfs-persistence \
    loader-test loader-child rune-runner std-smoke std-main std-child rune
do
    PROGRAM_ELF="$ROOT/target/x86_64-unknown-rustos/debug/rustos-$program"
    case "$program" in
        std-smoke|std-main|std-child|rune)
            PROGRAM_ELF="$STD_TARGET_DIR/x86_64-unknown-rustos/debug/rustos-$program"
            ;;
    esac
    cargo run -q -p rustos-rune -- \
        "$PROGRAM_ELF" \
        "$RUNE_DIR/$program.rune"
    cargo run -q -p rustos-rune -- verify "$RUNE_DIR/$program.rune"
done

# Динамические библиотеки и loader fixture используют тот же RUNE container,
# но дополнительно получают строгие interface/symbol ABI records из SDK manifest.
cargo run -q -p rustos-rune -- pack-manifest \
    target/x86_64-unknown-rustos/debug/fixture_1.dll \
    "$RUNE_LIB_DIR/fixture-1.rune" sdk/abi/fixture-answer.rune-abi
cargo run -q -p rustos-rune -- pack-manifest \
    target/x86_64-unknown-rustos/debug/loader_test_root.dll \
    "$RUNE_LIB_DIR/loader-root.rune" sdk/abi/loader-root.rune-abi
cargo run -q -p rustos-rune -- pack-manifest \
    target/x86_64-unknown-rustos/debug/vfs_1.dll \
    "$RUNE_LIB_DIR/vfs-1.rune" sdk/abi/vfs-1.rune-abi
for library in "$RUNE_LIB_DIR/"*.rune; do
    cargo run -q -p rustos-rune -- verify "$library"
done
cargo run -q -p rustos-rune -- pack-manifest \
    "$STD_TARGET_DIR/x86_64-unknown-rustos/debug/rustos-sdk-hello" \
    "$RUNE_APP_DIR/hello.rune" sdk/examples/hello/hello.rune-abi
cargo run -q -p rustos-rune -- verify "$RUNE_APP_DIR/hello.rune"

echo "[build] 2/9 kernel (x86_64-unknown-rustos, build-std=core)"
# Только ядро имеет фиксированный физический layout для GRUB. Пользовательские
# программы выше уже собраны PIE и продолжают загружаться RUNE-loader'ом.
KERNEL_RUSTFLAGS="-C force-unwind-tables=no -C relocation-model=static -C link-arg=-no-pie -C link-arg=-T$ROOT/kernel/x86_64-grub.ld"
if [[ "${RUSTOS_VIRGL_TEST:-0}" == "1" ]]; then
    echo "[build] kernel mode: virgl-test"
    CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$KERNEL_RUSTFLAGS" \
        cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json --features virgl-test
elif [[ "${RUSTOS_BOOT_TEST:-0}" == "1" ]]; then
    echo "[build] kernel mode: boot-test"
    CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$KERNEL_RUSTFLAGS" \
        cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json --features boot-test
else
    echo "[build] kernel mode: interactive GUI"
    CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$KERNEL_RUSTFLAGS" \
        cargo -Zjson-target-spec -Zbuild-std=core build -p rustos-kernel \
        --target targets/x86_64-unknown-rustos.json
fi

# Cargo кладёт варианты с разными feature в один человекочитаемый путь
# `target/.../rustos-kernel`. Последующие `cargo run` для host-инструментов не
# должны иметь возможности незаметно подменить выбранный boot-test/GUI-вариант.
# Поэтому, как и в ARM-сборке, фиксируем ядро сразу после компиляции и дальше
# работаем только с этой неизменяемой копией.
mkdir -p build/x86
cp -f target/x86_64-unknown-rustos/debug/rustos-kernel build/x86/kernel.elf

echo "[build] 3/9 staging: kernel + initramfs root"
STAGE="$ROOT/build/initramfs-root"
rm -rf "$STAGE"
mkdir -p "$STAGE/system/bin" "$STAGE/system/lib"
cp -R boot/initramfs/. "$STAGE/"
cp -f "$RUNE_DIR/"*.rune "$STAGE/system/bin/"
# В system image нет пользовательского ELF: DLL также поставляются как RUNE.
cp -f "$RUNE_LIB_DIR/vfs-1.rune" "$STAGE/system/lib/vfs-1.rune"

echo "[build] 4/9 initramfs (RIFS v1)"
cargo run -q -p rustos-pack -- "$STAGE" build/initramfs.img
cargo run -q -p rustos-pack -- --verify build/initramfs.img

echo "[build] 5/9 persistent VaraniaFS volume (kept between interactive boots)"
mkdir -p build
cargo run -q -p rustos-vfs-image -- build/system.vfs 1024
cargo run -q -p rustos-vfs-image -- --grow build/system.vfs 1024
cargo run -q -p rustos-vfs-image -- --put build/system.vfs \
    "$RUNE_LIB_DIR/fixture-1.rune" /system/lib/fixture-1.rune
cargo run -q -p rustos-vfs-image -- --put build/system.vfs \
    "$RUNE_LIB_DIR/loader-root.rune" /apps/loader-test/root.rune
cargo run -q -p rustos-vfs-image -- --put build/system.vfs \
    "$RUNE_DIR/std-child.rune" /apps/sdk/std-child.rune
cargo run -q -p rustos-vfs-image -- --put build/system.vfs \
    "$RUNE_APP_DIR/hello.rune" /apps/examples/hello.rune
cargo run -q -p rustos-vfs-image -- --verify build/system.vfs

echo "[build] 6/9 GRUB 2 standalone EFI (Multiboot2)"
bash scripts/build-grub.sh \
    build/x86/kernel.elf \
    build/initramfs.img \
    build/grub/BOOTX64.EFI

echo "[build] 7/9 OVMF"
bash scripts/bootstrap-ovmf.sh

echo "[build] 8/9 ESP image (GPT + FAT32 + GRUB EFI/BOOT/BOOTX64.EFI)"
mkdir -p build
cargo run -q -p rustos-image -- build/grub/BOOTX64.EFI build/esp.img
echo "[build] 9/9 verify ESP image"
cargo run -q -p rustos-image -- --verify build/esp.img build/grub/BOOTX64.EFI

echo "[build] OK: GRUB/Multiboot2 build/esp.img + build/ovmf/"

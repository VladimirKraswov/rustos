#!/usr/bin/env bash
# Собирает standalone GRUB x86_64 EFI с kernel/initramfs внутри memdisk.
#
# Linux использует установленный grub-mkstandalone. На macOS (включая M1)
# применяется кэшируемый Debian-контейнер, потому что GRUB не предоставляет
# native Darwin host tools. Получившийся BOOTX64.EFI одинаково запускается
# OVMF и реальной x86_64 UEFI firmware.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

KERNEL="${1:-target/x86_64-unknown-rustos/debug/rustos-kernel}"
INITRAMFS="${2:-build/initramfs.img}"
OUTPUT="${3:-build/grub/BOOTX64.EFI}"
CONFIG="$ROOT/boot/grub/grub.cfg"

[[ -f "$KERNEL" ]] || { echo "[grub] kernel not found: $KERNEL" >&2; exit 2; }
[[ -f "$INITRAMFS" ]] || { echo "[grub] initramfs not found: $INITRAMFS" >&2; exit 2; }
mkdir -p "$(dirname "$OUTPUT")"

MODULES="all_video efi_gop fat part_gpt normal configfile gfxterm serial multiboot2 memdisk"

build_native() {
    grub-file --is-x86-multiboot2 "$KERNEL" || {
        echo "[grub] kernel has no valid Multiboot2 header" >&2
        return 1
    }
    grub-mkstandalone \
        -O x86_64-efi \
        -o "$OUTPUT" \
        --modules="$MODULES" \
        "boot/grub/grub.cfg=$CONFIG" \
        "boot/rustos/kernel.elf=$KERNEL" \
        "boot/rustos/initramfs.img=$INITRAMFS"
}

if command -v grub-mkstandalone >/dev/null 2>&1 \
    && command -v grub-file >/dev/null 2>&1; then
    echo "[grub] using native host tools"
    build_native
else
    command -v docker >/dev/null 2>&1 || {
        echo "[grub] install grub-mkstandalone/grub-file or Docker Desktop" >&2
        exit 2
    }
    IMAGE="rustos-grub-builder:grub-2.12-deb13u2"
    if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
        echo "[grub] building cached Debian GRUB tool image"
        docker build --platform linux/amd64 -t "$IMAGE" boot/grub
    fi

    KERNEL_ABS="$(cd "$(dirname "$KERNEL")" && pwd)/$(basename "$KERNEL")"
    INITRAMFS_ABS="$(cd "$(dirname "$INITRAMFS")" && pwd)/$(basename "$INITRAMFS")"
    OUTPUT_DIR="$(cd "$(dirname "$OUTPUT")" && pwd)"
    OUTPUT_NAME="$(basename "$OUTPUT")"
    docker run --rm --platform linux/amd64 \
        -e OUTPUT_NAME="$OUTPUT_NAME" \
        -v "$KERNEL_ABS:/input/kernel.elf:ro" \
        -v "$INITRAMFS_ABS:/input/initramfs.img:ro" \
        -v "$CONFIG:/input/grub.cfg:ro" \
        -v "$OUTPUT_DIR:/output" \
        "$IMAGE" sh -eu -c '
            grub-file --is-x86-multiboot2 /input/kernel.elf
            grub-mkstandalone \
                -O x86_64-efi \
                -o "/output/$OUTPUT_NAME" \
                --modules="all_video efi_gop fat part_gpt normal configfile gfxterm serial multiboot2 memdisk" \
                "boot/grub/grub.cfg=/input/grub.cfg" \
                "boot/rustos/kernel.elf=/input/kernel.elf" \
                "boot/rustos/initramfs.img=/input/initramfs.img"
        '
fi

[[ -s "$OUTPUT" ]] || { echo "[grub] empty output: $OUTPUT" >&2; exit 1; }
echo "[grub] OK: $OUTPUT ($(wc -c < "$OUTPUT" | tr -d ' ') bytes)"

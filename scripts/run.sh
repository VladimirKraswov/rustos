#!/usr/bin/env bash
# Интерактивный графический запуск RustOS. Serial остаётся в терминале,
# framebuffer показывается отдельным окном QEMU без масштабирования готового
# bitmap. virtio-vga публикует современный wide EDID; GRUB выбирает его mode.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[[ -f build/esp.img ]] || { echo "нет build/esp.img — сначала: make build" >&2; exit 1; }
bash scripts/bootstrap-ovmf.sh >/dev/null
cp -f build/ovmf/OVMF_VARS.fd build/ovmf/OVMF_VARS_RUNTIME.fd

ACCEL=tcg
[[ -w /dev/kvm ]] && ACCEL=kvm
FIT_TO_WINDOW="${RUSTOS_FIT_TO_WINDOW:-0}"
FULLSCREEN="${RUSTOS_FULLSCREEN:-0}"
[[ "$FIT_TO_WINDOW" =~ ^[01]$ && "$FULLSCREEN" =~ ^[01]$ ]] || {
    echo "RUSTOS_FIT_TO_WINDOW и RUSTOS_FULLSCREEN должны быть 0 или 1" >&2
    exit 2
}

# 1:1 является безопасным режимом по умолчанию: QEMU меняет размер host window
# под гостевой framebuffer и не интерполирует уже отрисованные glyph/границы.
# Fit-to-window оставлен явной диагностической опцией, когда резкость не важна.
ZOOM_TO_FIT=off
[[ "$FIT_TO_WINDOW" == "1" ]] && ZOOM_TO_FIT=on
DISPLAY_ARGS=(-display "gtk,zoom-to-fit=$ZOOM_TO_FIT")
if [[ "$(uname -s)" == "Darwin" ]]; then
    COCOA_OPTIONS="cocoa,zoom-to-fit=$ZOOM_TO_FIT,show-cursor=on"
    [[ "$FULLSCREEN" == "1" ]] && COCOA_OPTIONS+=",full-screen=on"
    DISPLAY_ARGS=(-display "$COCOA_OPTIONS")
fi
WIDTH="${RUSTOS_DISPLAY_WIDTH:-1280}"
HEIGHT="${RUSTOS_DISPLAY_HEIGHT:-800}"
[[ "$WIDTH" =~ ^[1-9][0-9]*$ && "$HEIGHT" =~ ^[1-9][0-9]*$ ]] || {
    echo "RUSTOS_DISPLAY_WIDTH/HEIGHT должны быть положительными числами" >&2
    exit 2
}
MAPPING="1:1"
[[ "$FIT_TO_WINDOW" == "1" ]] && MAPPING="fit-to-window (bitmap scaling)"
echo "[run] qemu accel=$ACCEL, EDID=${WIDTH}x${HEIGHT}, output=$MAPPING, fullscreen=$FULLSCREEN, serial=console"

exec qemu-system-x86_64 \
    -machine q35 -cpu max -smp 2 -m 512 \
    -accel "$ACCEL" \
    -device virtio-vga,edid=on,xres="$WIDTH",yres="$HEIGHT" \
    -drive if=pflash,format=raw,readonly=on,file=build/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=build/ovmf/OVMF_VARS_RUNTIME.fd \
    -drive if=none,id=systemdisk,format=raw,file=build/system.vfs \
    -device virtio-blk-pci,drive=systemdisk,disable-modern=on,addr=0x5 \
    -drive if=virtio,format=raw,readonly=on,file=build/esp.img \
    -serial mon:stdio "${DISPLAY_ARGS[@]}" -no-reboot

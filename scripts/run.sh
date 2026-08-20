#!/usr/bin/env bash
# Интерактивный графический запуск RustOS. Serial остаётся в терминале,
# framebuffer показывается отдельным масштабируемым окном QEMU. virtio-vga
# публикует современный wide EDID; GRUB выбирает его preferred mode.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[[ -f build/esp.img ]] || { echo "нет build/esp.img — сначала: make build" >&2; exit 1; }
bash scripts/bootstrap-ovmf.sh >/dev/null
cp -f build/ovmf/OVMF_VARS.fd build/ovmf/OVMF_VARS_RUNTIME.fd

ACCEL=tcg
[[ -w /dev/kvm ]] && ACCEL=kvm
DISPLAY_ARGS=(-display gtk,zoom-to-fit=on)
if [[ "$(uname -s)" == "Darwin" ]]; then
    DISPLAY_ARGS=(-display cocoa,zoom-to-fit=on,show-cursor=on)
    # На Retina-экранах обычное окно QEMU получается слишком мелким. По
    # умолчанию открываем VM во весь экран; RUSTOS_FULLSCREEN=0 это отключает.
    if [[ "${RUSTOS_FULLSCREEN:-1}" != "0" ]]; then
        DISPLAY_ARGS=(-display cocoa,zoom-to-fit=on,show-cursor=on,full-screen=on)
    fi
fi
WIDTH="${RUSTOS_DISPLAY_WIDTH:-1600}"
HEIGHT="${RUSTOS_DISPLAY_HEIGHT:-900}"
[[ "$WIDTH" =~ ^[1-9][0-9]*$ && "$HEIGHT" =~ ^[1-9][0-9]*$ ]] || {
    echo "RUSTOS_DISPLAY_WIDTH/HEIGHT должны быть положительными числами" >&2
    exit 2
}
echo "[run] qemu accel=$ACCEL, GRUB/Multiboot2, EDID request=${WIDTH}x${HEIGHT}, serial=console"

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

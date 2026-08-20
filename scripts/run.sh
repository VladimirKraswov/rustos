#!/usr/bin/env bash
# Интерактивный графический запуск RustOS. Serial остаётся в терминале,
# framebuffer показывается отдельным масштабируемым окном QEMU.
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
echo "[run] qemu accel=$ACCEL, graphical GOP, serial=console"

exec qemu-system-x86_64 \
    -machine q35 -cpu max -smp 2 -m 512 \
    -accel "$ACCEL" \
    -drive if=pflash,format=raw,readonly=on,file=build/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=build/ovmf/OVMF_VARS_RUNTIME.fd \
    -drive if=virtio,format=raw,readonly=on,file=build/esp.img \
    -serial mon:stdio "${DISPLAY_ARGS[@]}" -no-reboot

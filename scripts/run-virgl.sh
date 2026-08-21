#!/usr/bin/env bash
# Интерактивный AMD64-профиль с настоящим Virtio GPU VirGL backend.
# Скрипт не подменяет отсутствие 3D software renderer'ом внутри гостя.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
QEMU="${RUSTOS_VIRGL_QEMU:-qemu-system-x86_64}"

[[ -f build/esp.img ]] || { echo "нет build/esp.img — сначала: make build-x86" >&2; exit 1; }
if ! "$QEMU" -device help 2>&1 | grep -Eq 'name "virtio-vga-gl"([,[:space:]]|$)'; then
    cat >&2 <<'EOF'
QEMU собран без virtio-vga-gl/VirGL.
На Linux установите QEMU с virglrenderer. На macOS обычная Homebrew-сборка
не содержит этого backend; укажите совместимую сборку через RUSTOS_VIRGL_QEMU.
CPU fallback намеренно не включается: он скрыл бы отсутствие 3D path.
EOF
    exit 2
fi

bash scripts/bootstrap-ovmf.sh >/dev/null
cp -f build/ovmf/OVMF_VARS.fd build/ovmf/OVMF_VARS_VIRGL.fd
DISPLAY_BACKEND="${RUSTOS_VIRGL_DISPLAY:-gtk,gl=on,zoom-to-fit=on}"

echo "[run-virgl] device=virtio-vga-gl display=$DISPLAY_BACKEND guest-rasterizer=disabled"
exec "$QEMU" \
    -machine q35 -cpu max -smp 2 -m 512 -accel tcg \
    -device virtio-vga-gl,edid=on,xres=1280,yres=800 \
    -drive if=pflash,format=raw,readonly=on,file=build/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=build/ovmf/OVMF_VARS_VIRGL.fd \
    -drive if=none,id=systemdisk,format=raw,file=build/system.vfs \
    -device virtio-blk-pci,drive=systemdisk,disable-modern=on,addr=0x5 \
    -drive if=virtio,format=raw,readonly=on,file=build/esp.img \
    -serial mon:stdio -display "$DISPLAY_BACKEND" -no-reboot

#!/usr/bin/env bash
# Интерактивный запуск ARM-варианта RustOS: QEMU `virt` + UEFI (AAVMF).
#
# Требует полного build/arm/esp-arm.img, создаваемого `make build-arm`.
# Serial остаётся в терминале, framebuffer — в масштабируемом окне QEMU.
#
# Переопределяемые параметры:
#   ARM_SMP (по умолчанию 2), ARM_MEMORY_MB (512), ARM_CPU_MODEL (cortex-a72).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ ! -f build/arm/esp-arm.img ]]; then
    echo "нет build/arm/esp-arm.img — сначала: make build-arm" >&2
    exit 1
fi

bash scripts/bootstrap-arm-firmware.sh >/dev/null
cp -f build/arm-firmware/edk2-aarch64-vars-template.fd build/arm-firmware/edk2-aarch64-vars-runtime.fd

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

ARM_SMP="${ARM_SMP:-2}"
ARM_MEMORY_MB="${ARM_MEMORY_MB:-512}"
# cortex-a72 — консервативная эталонная CPU-модель QEMU virt (GIC, Generic
# Timer, LPA); ARM_CPU_MODEL=max даёт новые FEAT'ы для дебага.
ARM_CPU_MODEL="${ARM_CPU_MODEL:-cortex-a72}"

echo "[run-arm] qemu accel=$ACCEL, machine=virt, cpu=$ARM_CPU_MODEL, smp=$ARM_SMP, mem=${ARM_MEMORY_MB}M, UEFI=AAVMF"

exec qemu-system-aarch64 \
    -machine virt,gic-version=3,acpi=off \
    -cpu "$ARM_CPU_MODEL" -smp "$ARM_SMP" -m "$ARM_MEMORY_MB" \
    -accel "$ACCEL" \
    -drive if=pflash,format=raw,readonly=on,file=build/arm-firmware/edk2-aarch64-code.fd \
    -drive if=pflash,format=raw,file=build/arm-firmware/edk2-aarch64-vars-runtime.fd \
    -drive if=none,id=systemdisk,format=raw,file=build/arm-system.vfs \
    -device virtio-blk-device,drive=systemdisk \
    -global virtio-mmio.force-legacy=false \
    -drive if=virtio,format=raw,readonly=on,file=build/arm/esp-arm.img \
    -serial mon:stdio "${DISPLAY_ARGS[@]}" -no-reboot

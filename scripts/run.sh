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

PLATFORM="$(uname -s)"
DEFAULT_POLICY=actual
[[ "$PLATFORM" == "Darwin" ]] && DEFAULT_POLICY=integer
DISPLAY_POLICY="${RUSTOS_DISPLAY_POLICY:-$DEFAULT_POLICY}"
case "$DISPLAY_POLICY" in
    integer)
        DEFAULT_FIT=1
        DEFAULT_FULLSCREEN=1
        ;;
    actual)
        DEFAULT_FIT=0
        DEFAULT_FULLSCREEN=0
        ;;
    fit)
        DEFAULT_FIT=1
        DEFAULT_FULLSCREEN=1
        ;;
    *)
        echo "RUSTOS_DISPLAY_POLICY должен быть integer, actual или fit" >&2
        exit 2
        ;;
esac

WIDTH="${RUSTOS_DISPLAY_WIDTH:-}"
HEIGHT="${RUSTOS_DISPLAY_HEIGHT:-}"
if [[ -n "$WIDTH" || -n "$HEIGHT" ]]; then
    [[ -n "$WIDTH" && -n "$HEIGHT" ]] || {
        echo "RUSTOS_DISPLAY_WIDTH и RUSTOS_DISPLAY_HEIGHT задаются вместе" >&2
        exit 2
    }
    PROFILE="manual ${WIDTH}x${HEIGHT}"
else
    WIDTH=1280
    HEIGHT=800
    PROFILE="default ${WIDTH}x${HEIGHT}"
    if [[ "$PLATFORM" == "Darwin" && "$DISPLAY_POLICY" == "integer" ]]; then
        HOST_DISPLAY="${RUSTOS_HOST_DISPLAY:-0}"
        [[ "$HOST_DISPLAY" =~ ^[0-9]+$ ]] || {
            echo "RUSTOS_HOST_DISPLAY должен быть индексом экрана" >&2
            exit 2
        }
        HOST_METRICS="$(osascript -l JavaScript scripts/macos-display-metrics.js "$HOST_DISPLAY")" || {
            echo "не удалось прочитать Cocoa display metrics" >&2
            exit 2
        }
        read -r HOST_WIDTH HOST_HEIGHT HOST_BACKING HOST_DISPLAY_COUNT <<<"$HOST_METRICS"
        INTEGER_MODE="$(bash scripts/select-integer-display-mode.sh \
            "$HOST_WIDTH" "$HOST_HEIGHT" 1600 900)" || {
            echo "[run] integer-fit недоступен; используйте RUSTOS_DISPLAY_POLICY=actual" >&2
            exit 2
        }
        read -r WIDTH HEIGHT INTEGER_SCALE INTEGER_FIT <<<"$INTEGER_MODE"
        PROFILE="screen=$HOST_DISPLAY/$HOST_DISPLAY_COUNT host=${HOST_WIDTH}x${HOST_HEIGHT} backing=$HOST_BACKING guest=${WIDTH}x${HEIGHT} scale=x${INTEGER_SCALE} $INTEGER_FIT"
    fi
fi

FIT_TO_WINDOW="${RUSTOS_FIT_TO_WINDOW:-$DEFAULT_FIT}"
FULLSCREEN="${RUSTOS_FULLSCREEN:-$DEFAULT_FULLSCREEN}"
[[ "$FIT_TO_WINDOW" =~ ^[01]$ && "$FULLSCREEN" =~ ^[01]$ ]] || {
    echo "RUSTOS_FIT_TO_WINDOW и RUSTOS_FULLSCREEN должны быть 0 или 1" >&2
    exit 2
}

# 1:1 является безопасным режимом по умолчанию: QEMU меняет размер host window
# под гостевой framebuffer и не интерполирует уже отрисованные glyph/границы.
# Fit-to-window оставлен явной диагностической опцией, когда резкость не важна.
ZOOM_TO_FIT=off
[[ "$FIT_TO_WINDOW" == "1" ]] && ZOOM_TO_FIT=on
GTK_OPTIONS="gtk,zoom-to-fit=$ZOOM_TO_FIT"
[[ "$FULLSCREEN" == "1" ]] && GTK_OPTIONS+=",full-screen=on"
DISPLAY_ARGS=(-display "$GTK_OPTIONS")
if [[ "$PLATFORM" == "Darwin" ]]; then
    COCOA_OPTIONS="cocoa,zoom-to-fit=$ZOOM_TO_FIT,show-cursor=on"
    # QEMU 9+ умеет отдельно отключать фильтрацию zoom. В integer profile это
    # сохраняет ровную толщину пикселей и не превращает границы в «мыло».
    QEMU_MAJOR="$(qemu-system-x86_64 --version | sed -nE '1s/.*version ([0-9]+).*/\1/p')"
    if [[ "$ZOOM_TO_FIT" == "on" && "${QEMU_MAJOR:-0}" -ge 9 ]]; then
        COCOA_OPTIONS+=",zoom-interpolation=off"
    fi
    [[ "$FULLSCREEN" == "1" ]] && COCOA_OPTIONS+=",full-screen=on"
    DISPLAY_ARGS=(-display "$COCOA_OPTIONS")
fi
[[ "$WIDTH" =~ ^[1-9][0-9]*$ && "$HEIGHT" =~ ^[1-9][0-9]*$ ]] || {
    echo "RUSTOS_DISPLAY_WIDTH/HEIGHT должны быть положительными числами" >&2
    exit 2
}
MAPPING="1:1"
[[ "$FIT_TO_WINDOW" == "1" ]] && MAPPING="fit-to-window"
[[ "$DISPLAY_POLICY" == "integer" && -n "${INTEGER_SCALE:-}" ]] && MAPPING="integer x${INTEGER_SCALE} ($INTEGER_FIT)"
echo "[run] qemu accel=$ACCEL, policy=$DISPLAY_POLICY, profile=[$PROFILE]"
echo "[run] EDID=${WIDTH}x${HEIGHT}, output=$MAPPING, fullscreen=$FULLSCREEN, serial=console"

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

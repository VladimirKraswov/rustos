#!/usr/bin/env bash
# End-to-end VirGL test on Apple Silicon: RustOS -> UTM -> ANGLE -> Metal.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM_NAME="${RUSTOS_UTM_VM_NAME:-RustOS GPU Development}"
RESULT_DIR="$ROOT/build/test-results/utm-gpu"
SERIAL_LOG="$RESULT_DIR/serial.log"
TIMEOUT_SECONDS="${RUSTOS_UTM_GPU_TIMEOUT:-240}"
cd "$ROOT"

mkdir -p "$RESULT_DIR"
: > "$SERIAL_LOG"

if VM_STATE="$(utmctl status "$VM_NAME" 2>/dev/null)" && [[ "$VM_STATE" != "stopped" ]]; then
    utmctl stop "$VM_NAME" --force >/dev/null
    for _ in {1..80}; do
        [[ "$(utmctl status "$VM_NAME" 2>/dev/null || true)" == "stopped" ]] && break
        sleep 0.25
    done
fi

if [[ "${RUSTOS_UTM_SKIP_BUILD:-0}" != "1" ]]; then
    RUSTOS_VIRGL_TEST=1 bash scripts/build-arm.sh
fi
bash scripts/setup-utm-gpu.sh
utmctl start "$VM_NAME" >/dev/null

PTY=""
for _ in {1..100}; do
    PTY="$(osascript -e 'tell application "UTM" to get address of first serial port of virtual machine named "'"$VM_NAME"'"' 2>/dev/null || true)"
    [[ -n "$PTY" && "$PTY" != "missing value" && -e "$PTY" ]] && break
    sleep 0.1
done
if [[ -z "$PTY" || ! -e "$PTY" ]]; then
    echo "[utm-gpu-test] serial PTTY unavailable" >&2
    exit 3
fi

stty -f "$PTY" raw -echo 2>/dev/null || true
tee "$SERIAL_LOG" < "$PTY" &
SERIAL_PID=$!
cleanup() {
    kill "$SERIAL_PID" 2>/dev/null || true
    utmctl stop "$VM_NAME" --force >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

READY=0
for ((second = 0; second < TIMEOUT_SECONDS; second++)); do
    if grep -Fq '[virgl-test] MESA_SHOWCASE_READY scanout=graphics-buffer cpu-raster=no' "$SERIAL_LOG"; then
        READY=1
        break
    fi
    if grep -Fq '[virgl-test] FATAL:' "$SERIAL_LOG"; then
        break
    fi
    sleep 1
done

grep -Fq '[gpu-demo] AURORA_3D_READY frames=48 renderer=mesa-virgl cpu-raster=no' "$SERIAL_LOG"
grep -Fq '[virgl-test] MESA_SHOWCASE_READY scanout=graphics-buffer cpu-raster=no' "$SERIAL_LOG"
if [[ "$READY" != "1" ]]; then
    echo "[utm-gpu-test] accelerated scene timeout" >&2
    exit 4
fi
if grep -Fq '[virgl] unavailable:' "$SERIAL_LOG"; then
    echo "[utm-gpu-test] device exposed only 2D VirtIO GPU" >&2
    exit 5
fi

echo "[utm-gpu-test] PASS: guest VirGL commands reached UTM ANGLE/Metal without guest CPU rasterization"

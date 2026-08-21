#!/usr/bin/env bash
# End-to-end: ring-3 renderd -> async Virtio fence -> GraphicsBuffer ->
# compositord -> displayd -> VirGL scanout. Скриншот проверяет сам треугольник.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
QEMU="${RUSTOS_VIRGL_QEMU:-qemu-system-x86_64}"
if ! "$QEMU" -device help 2>&1 | grep -Eq 'name "virtio-vga-gl"([,[:space:]]|$)'; then
    echo "[virgl-test] FAIL: $QEMU не предоставляет virtio-vga-gl" >&2
    exit 2
fi

bash scripts/bootstrap-ovmf.sh >/dev/null
cargo build -q -p rustos-gui-check
CHECK_TOOL="$ROOT/target/debug/rustos-gui-check"
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustos-virgl-test.XXXXXX")"
RESULT_DIR="$ROOT/build/test-results/virgl"
mkdir -p "$RESULT_DIR"
cp -f build/ovmf/OVMF_VARS.fd "$RUN_DIR/VARS.fd"

QPID=""
XVFB_PID=""
cleanup() {
    trap - EXIT INT TERM HUP
    if [[ -n "$QPID" ]] && kill -0 "$QPID" 2>/dev/null; then
        kill -TERM "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill -TERM "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
    fi
    for file in serial.log qemu-stderr.log xvfb-stderr.log triangle.ppm triangle.xwd; do
        [[ -f "$RUN_DIR/$file" ]] && cp -f "$RUN_DIR/$file" "$RESULT_DIR/$file"
    done
}
trap cleanup EXIT INT TERM HUP

if ! command -v Xvfb >/dev/null 2>&1; then
    echo "[virgl-test] FAIL: Xvfb нужен для захвата GL scanout" >&2
    exit 2
fi

# HMP `screendump` принципиально не видит GL/dmabuf scanout (`no surface`).
# Xvfb с `-fbdir` публикует реальную X11 display surface в XWD-файле. Именно
# её мы проверяем: это уже выход host VirGL renderer, показанный QEMU GTK.
DISPLAY_NUMBER_FILE="$RUN_DIR/xvfb-display"
Xvfb -displayfd 1 -screen 0 1600x1000x24 -fbdir "$RUN_DIR" \
    -nolisten tcp -noreset >"$DISPLAY_NUMBER_FILE" \
    2>"$RUN_DIR/xvfb-stderr.log" &
XVFB_PID=$!
for _ in {1..50}; do
    [[ -s "$DISPLAY_NUMBER_FILE" ]] && break
    kill -0 "$XVFB_PID" 2>/dev/null || break
    sleep 0.1
done
[[ -s "$DISPLAY_NUMBER_FILE" ]] || {
    echo "[virgl-test] Xvfb did not publish a display" >&2
    cat "$RUN_DIR/xvfb-stderr.log" >&2 2>/dev/null || true
    exit 1
}
VIRGL_DISPLAY=":$(tr -d '\r\n' <"$DISPLAY_NUMBER_FILE")"

QEMU_ARGS=(
    -machine q35 -cpu max -smp 2 -m 512 -accel tcg
    -device "virtio-vga-gl,edid=on,xres=1280,yres=800"
    -drive "if=pflash,format=raw,readonly=on,file=build/ovmf/OVMF_CODE.fd"
    -drive "if=pflash,format=raw,file=$RUN_DIR/VARS.fd"
    -drive "if=none,id=systemdisk,format=raw,file=build/system.vfs"
    -device "virtio-blk-pci,drive=systemdisk,disable-modern=on,addr=0x5"
    -drive "if=virtio,format=raw,readonly=on,file=build/esp.img"
    -serial file:"$RUN_DIR/serial.log"
    -monitor "unix:$RUN_DIR/monitor.sock,server=on,wait=off"
    -display "gtk,gl=on,show-menubar=off,show-tabs=off,zoom-to-fit=off"
    -snapshot -no-reboot -no-shutdown
)

env DISPLAY="$VIRGL_DISPLAY" LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}" \
    "$QEMU" "${QEMU_ARGS[@]}" >/dev/null 2>"$RUN_DIR/qemu-stderr.log" &
QPID=$!

ready=0
for _ in $(seq 1 "${VIRGL_TEST_TIMEOUT:-240}"); do
    if grep -Fq '[virgl-test] TRIANGLE_READY scanout=graphics-buffer cpu-raster=no' \
        "$RUN_DIR/serial.log" 2>/dev/null; then
        ready=1
        break
    fi
    kill -0 "$QPID" 2>/dev/null || break
    sleep 1
done
if [[ "$ready" != 1 ]]; then
    echo "[virgl-test] triangle marker timeout" >&2
    tail -80 "$RUN_DIR/serial.log" >&2 2>/dev/null || true
    cat "$RUN_DIR/qemu-stderr.log" >&2 2>/dev/null || true
    exit 1
fi

# GTK получает dmabuf после atomic present асинхронно относительно serial.
# Кадр статичен, поэтому одна bounded-пауза даёт display thread завершить
# swap без внесения недетерминированности в содержимое.
sleep 1
cp -f "$RUN_DIR/Xvfb_screen0" "$RUN_DIR/triangle.xwd"
[[ -s "$RUN_DIR/triangle.xwd" ]] || {
    echo "[virgl-test] Xvfb display surface is empty" >&2
    exit 1
}
"$CHECK_TOOL" --virgl "$RUN_DIR/triangle.xwd" "$RUN_DIR/triangle.ppm"
grep -Fq '[virgl] ring3 renderd async-fence triangle zero-copy scanout verified' \
    "$RUN_DIR/serial.log"
echo "[virgl-test] PASS: 3D triangle reached scanout without guest CPU rasterization"

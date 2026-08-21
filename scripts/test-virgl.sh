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
cargo build -q -p rustos-hmp -p rustos-gui-check
HMP_TOOL="$ROOT/target/debug/rustos-hmp"
CHECK_TOOL="$ROOT/target/debug/rustos-gui-check"
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustos-virgl-test.XXXXXX")"
RESULT_DIR="$ROOT/build/test-results/virgl"
mkdir -p "$RESULT_DIR"
cp -f build/ovmf/OVMF_VARS.fd "$RUN_DIR/VARS.fd"

QPID=""
cleanup() {
    trap - EXIT INT TERM HUP
    if [[ -n "$QPID" ]] && kill -0 "$QPID" 2>/dev/null; then
        kill -TERM "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    for file in serial.log qemu-stderr.log triangle.ppm; do
        [[ -f "$RUN_DIR/$file" ]] && cp -f "$RUN_DIR/$file" "$RESULT_DIR/$file"
    done
}
trap cleanup EXIT INT TERM HUP

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
    -display "gtk,gl=on" -snapshot -no-reboot -no-shutdown
)

if [[ -z "${DISPLAY:-}" ]] && command -v xvfb-run >/dev/null 2>&1; then
    xvfb-run -a env LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}" \
        "$QEMU" "${QEMU_ARGS[@]}" >/dev/null 2>"$RUN_DIR/qemu-stderr.log" &
else
    "$QEMU" "${QEMU_ARGS[@]}" >/dev/null 2>"$RUN_DIR/qemu-stderr.log" &
fi
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

printf 'screendump %s/triangle.ppm\n' "$RUN_DIR" | \
    "$HMP_TOOL" "$RUN_DIR/monitor.sock" >/dev/null
# HMP принимает команду синхронно, но display backend завершает запись PPM
# асинхронно. Небольшое bounded-ожидание не скрывает ошибку и исключает гонку
# между закрытием monitor socket и первым write файла.
for _ in {1..50}; do
    [[ -s "$RUN_DIR/triangle.ppm" ]] && break
    sleep 0.1
done
[[ -s "$RUN_DIR/triangle.ppm" ]] || { echo "[virgl-test] empty screenshot" >&2; exit 1; }
"$CHECK_TOOL" --virgl "$RUN_DIR/triangle.ppm"
grep -Fq '[virgl] ring3 renderd async-fence triangle zero-copy scanout verified' \
    "$RUN_DIR/serial.log"
echo "[virgl-test] PASS: 3D triangle reached scanout without guest CPU rasterization"

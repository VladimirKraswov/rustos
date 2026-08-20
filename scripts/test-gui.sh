#!/usr/bin/env bash
# Интеграционный GUI-тест: boot, клавиатура, terminal, мышь, window manager.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
bash scripts/build.sh >/dev/null
bash scripts/bootstrap-ovmf.sh >/dev/null

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustos-gui-test.XXXXXX")"
RESULT_DIR="$ROOT/build/test-results/gui"
mkdir -p "$RESULT_DIR"
cp -f build/ovmf/OVMF_VARS.fd "$RUN_DIR/VARS.fd"

QPID=""
GUI_TIMEOUT="${GUI_TEST_TIMEOUT:-360}"
# OpenBSD netcat на Linux продолжает ждать EOF от постоянного HMP-сокета.
# `-N` делает half-close после EOF stdin. В macOS этот флаг означает другое,
# а системный nc и без него корректно заканчивает одноразовый запрос.
HMP_CLIENT=(nc -U)
if [[ "$(uname -s)" != "Darwin" ]]; then
    HMP_CLIENT=(nc -N -U)
fi

hmp() {
    "${HMP_CLIENT[@]}" "$RUN_DIR/monitor.sock" >/dev/null
}

cleanup() {
    trap - EXIT INT TERM HUP
    if [[ -n "$QPID" ]] && kill -0 "$QPID" 2>/dev/null; then
        kill -TERM "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    for file in serial.log qemu-stderr.log terminal.ppm minimized.ppm; do
        [[ -f "$RUN_DIR/$file" ]] && cp -f "$RUN_DIR/$file" "$RESULT_DIR/$file"
    done
}
trap cleanup EXIT INT TERM HUP

qemu-system-x86_64 \
    -machine q35 -cpu max -smp 2 -m 512 -accel tcg \
    -drive if=pflash,format=raw,readonly=on,file=build/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file="$RUN_DIR/VARS.fd" \
    -drive if=virtio,format=raw,readonly=on,file=build/esp.img \
    -serial file:"$RUN_DIR/serial.log" \
    -monitor unix:"$RUN_DIR/monitor.sock",server=on,wait=off \
    -display none -no-reboot -no-shutdown \
    >/dev/null 2>"$RUN_DIR/qemu-stderr.log" &
QPID=$!

ready=0
for _ in $(seq 1 "$GUI_TIMEOUT"); do
    if grep -q 'GUI_READY' "$RUN_DIR/serial.log" 2>/dev/null; then
        ready=1
        break
    fi
    kill -0 "$QPID" 2>/dev/null || break
    sleep 1
done
[[ $ready == 1 ]] || {
    echo "[gui-test] GUI_READY timeout after ${GUI_TIMEOUT}s"
    exit 1
}

# Команда идёт через настоящий PS/2 keyboard path.
printf 'sendkey h\nsendkey e\nsendkey l\nsendkey p\nsendkey ret\n' \
    | hmp
for _ in $(seq 1 40); do
    grep -q '\[terminal\] command: help' "$RUN_DIR/serial.log" && break
    sleep 0.1
done
grep -q '\[terminal\] command: help' "$RUN_DIR/serial.log"
printf 'screendump %s/terminal.ppm\n' "$RUN_DIR" \
    | hmp
sleep 0.5

# Курсор стартует в центре 1280x800; перемещаем его к minimize-кнопке.
printf 'mouse_move 435 -325\n' | hmp
sleep 0.2
printf 'mouse_button 1\nmouse_button 0\n' | hmp
for _ in $(seq 1 40); do
    grep -q '\[wm\] terminal minimized' "$RUN_DIR/serial.log" && break
    sleep 0.1
done
grep -q '\[wm\] terminal minimized' "$RUN_DIR/serial.log"
printf 'screendump %s/minimized.ppm\n' "$RUN_DIR" \
    | hmp
sleep 0.5

cargo run -q -p rustos-gui-check -- "$RUN_DIR/terminal.ppm" "$RUN_DIR/minimized.ppm"
printf 'quit\n' | hmp || true
wait "$QPID" 2>/dev/null || true
QPID=""
echo "[gui-test] PASS: keyboard, terminal, mouse and minimize"

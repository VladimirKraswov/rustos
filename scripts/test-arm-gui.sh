#!/usr/bin/env bash
# ARM GUI integration: AAVMF → EL0 milestones → virtio GPU/input → SystemUI.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bash scripts/build-arm.sh >/dev/null
bash scripts/bootstrap-arm-firmware.sh >/dev/null
cargo build -q -p rustos-hmp
HMP_TOOL="$ROOT/target/debug/rustos-hmp"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustos-arm-gui-test.XXXXXX")"
RESULT_DIR="$ROOT/build/test-results/arm-gui"
mkdir -p "$RESULT_DIR"
cp -f build/arm-firmware/edk2-aarch64-vars-template.fd "$RUN_DIR/VARS.fd"
# Интерактивный QEMU держит writable VaraniaFS image эксклюзивно. GUI-test
# использует собственную sparse-копию: тесты можно запускать, не закрывая VM
# пользователя, а их записи никогда не меняют developer volume.
SYSTEM_DISK="$RUN_DIR/system.vfs"
cp -f build/arm-system.vfs "$SYSTEM_DISK"

HOST_SYSTEM="$(uname -s)"
HOST_MACHINE="$(uname -m)"
ACCEL=tcg
CPU_MODEL=cortex-a72
if [[ "$HOST_SYSTEM" == "Darwin" && "$HOST_MACHINE" =~ ^(arm64|aarch64)$ ]]; then
    ACCEL=hvf
    CPU_MODEL=host
elif [[ "$HOST_SYSTEM" == "Linux" && "$HOST_MACHINE" =~ ^(arm64|aarch64)$ && -w /dev/kvm ]]; then
    ACCEL=kvm
    CPU_MODEL=host
fi
ACCEL="${ARM_GUI_TEST_ACCEL:-$ACCEL}"
CPU_MODEL="${ARM_GUI_TEST_CPU_MODEL:-$CPU_MODEL}"
MEMORY_MB="${ARM_GUI_TEST_MEMORY_MB:-512}"
CPUS="${ARM_GUI_TEST_CPUS:-2}"
TIMEOUT="${ARM_GUI_TEST_TIMEOUT:-360}"

QPID=""
stop_qemu() {
    if [[ -z "$QPID" ]]; then
        return
    fi
    if kill -0 "$QPID" 2>/dev/null; then
        kill -TERM "$QPID" 2>/dev/null || true
        for _ in $(seq 1 50); do
            kill -0 "$QPID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$QPID" 2>/dev/null || true
    fi
    wait "$QPID" 2>/dev/null || true
    QPID=""
}

cleanup() {
    trap - EXIT INT TERM HUP
    stop_qemu
    for file in serial.log qemu-stderr.log desktop.ppm; do
        [[ -f "$RUN_DIR/$file" ]] && cp -f "$RUN_DIR/$file" "$RESULT_DIR/$file"
    done
    rm -rf "$RUN_DIR"
}
trap cleanup EXIT INT TERM HUP

qemu-system-aarch64 \
    -machine virt,gic-version=3,acpi=off \
    -cpu "$CPU_MODEL" -smp "$CPUS" -m "$MEMORY_MB" -accel "$ACCEL" \
    -drive if=pflash,format=raw,readonly=on,file=build/arm-firmware/edk2-aarch64-code.fd \
    -drive if=pflash,format=raw,file="$RUN_DIR/VARS.fd" \
    -drive if=none,id=systemdisk,format=raw,file="$SYSTEM_DISK" \
    -device virtio-blk-device,drive=systemdisk \
    -device virtio-gpu-device,xres=1280,yres=720 \
    -device virtio-keyboard-device \
    -device virtio-mouse-device \
    -global virtio-mmio.force-legacy=false \
    -drive if=virtio,format=raw,readonly=on,file=build/arm/esp-arm.img \
    -serial file:"$RUN_DIR/serial.log" \
    -monitor unix:"$RUN_DIR/monitor.sock",server=on,wait=off \
    -display none -no-reboot \
    >/dev/null 2>"$RUN_DIR/qemu-stderr.log" &
QPID=$!

ready=0
for _ in $(seq 1 "$TIMEOUT"); do
    if grep -q 'GUI_READY' "$RUN_DIR/serial.log" 2>/dev/null; then
        ready=1
        break
    fi
    kill -0 "$QPID" 2>/dev/null || break
    sleep 1
done
[[ $ready == 1 ]] || {
    if kill -0 "$QPID" 2>/dev/null; then
        echo "[test-arm-gui] GUI_READY timeout after ${TIMEOUT}s" >&2
    else
        echo "[test-arm-gui] QEMU exited before GUI_READY" >&2
    fi
    tail -n 80 "$RUN_DIR/serial.log" 2>/dev/null || true
    tail -n 40 "$RUN_DIR/qemu-stderr.log" 2>/dev/null || true
    exit 1
}

grep -Fq '[microkernel] RING3_MILESTONE_OK' "$RUN_DIR/serial.log"
grep -Fq '[video] virtio-gpu modern MMIO controlq ready' "$RUN_DIR/serial.log"
grep -Fq 'mouse=virtio-input-mmio' "$RUN_DIR/serial.log"

# HMP только генерирует аппаратные события; до terminal они проходят через
# virtio event queue, Linux-keycode decoder и общий SystemUI focus routing.
printf 'sendkey h 20\nsendkey e 20\nsendkey l 20\nsendkey p 20\n' \
    | "$HMP_TOOL" "$RUN_DIR/monitor.sock" 120 >/dev/null
sleep 0.2
printf 'sendkey ret 20\n' | "$HMP_TOOL" "$RUN_DIR/monitor.sock" 100 >/dev/null
for _ in $(seq 1 150); do
    grep -Fq '[terminal] command: help' "$RUN_DIR/serial.log" && break
    sleep 0.1
done
grep -Fq '[terminal] command: help' "$RUN_DIR/serial.log"

# Курсор начинает в центре 1280×720. Перемещаем его к Start и проверяем
# button down/up через независимый virtio-mouse device.
printf 'mouse_move -578 337\n' | "$HMP_TOOL" "$RUN_DIR/monitor.sock" 100 >/dev/null
sleep 0.3
printf 'mouse_button 1\nmouse_button 0\n' \
    | "$HMP_TOOL" "$RUN_DIR/monitor.sock" 100 >/dev/null
for _ in $(seq 1 150); do
    grep -Fq '[start] opened component-runtime=system-ui-v1' "$RUN_DIR/serial.log" && break
    sleep 0.1
done
grep -Fq '[start] opened component-runtime=system-ui-v1' "$RUN_DIR/serial.log"

printf 'screendump %s/desktop.ppm\n' "$RUN_DIR" \
    | "$HMP_TOOL" "$RUN_DIR/monitor.sock" >/dev/null
[[ -s "$RUN_DIR/desktop.ppm" ]]
[[ "$(head -c 2 "$RUN_DIR/desktop.ppm")" == "P6" ]]

echo "[test-arm-gui] PASS accel=$ACCEL cpu=$CPU_MODEL: GPU + keyboard + mouse"

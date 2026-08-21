#!/usr/bin/env bash
# Воспроизводимый boot-тест: OVMF -> ESP -> ядро -> isa-debug-exit.
#
# Каждый запуск получает собственные writable VARS и журналы. Поэтому
# прерванный тест не блокирует следующий эксклюзивным lock QEMU.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

[[ -f build/esp.img ]] || { echo "нет build/esp.img — сначала: make build" >&2; exit 1; }
bash scripts/bootstrap-ovmf.sh >/dev/null

ACCEL=tcg
case "$(uname -m)" in
    x86_64|amd64)
        [[ -w /dev/kvm ]] && ACCEL=kvm
        ;;
esac
if [[ "$ACCEL" == "kvm" ]]; then
    DEFAULT_TIMEOUT=120
else
    # OVMF x86-64 под TCG на Apple Silicon загружается несколько минут.
    DEFAULT_TIMEOUT=420
fi
TIMEOUT="${BOOT_TEST_TIMEOUT:-$DEFAULT_TIMEOUT}"
MEMORY_MB="${BOOT_MEMORY_MB:-128}"
CPUS="${BOOT_CPUS:-2}"
CPU_MODEL="${BOOT_CPU_MODEL:-max}"
[[ "$MEMORY_MB" =~ ^[1-9][0-9]*$ ]] || {
    echo "BOOT_MEMORY_MB должен быть положительным числом MiB" >&2
    exit 2
}
[[ "$CPUS" =~ ^[1-9][0-9]*$ ]] || {
    echo "BOOT_CPUS должен быть положительным числом" >&2
    exit 2
}

RESULT_DIR="$ROOT/build/test-results/boot"
mkdir -p "$RESULT_DIR"
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustos-boot.XXXXXX")"
VARS="$RUN_DIR/OVMF_VARS.fd"
LOG="$RUN_DIR/serial.log"
STDERR_LOG="$RUN_DIR/qemu-stderr.log"
TIMED_OUT="$RUN_DIR/timed-out"
cp -f build/ovmf/OVMF_VARS.fd "$VARS"

QPID=""
WATCHDOG=""
# Функция вызывается косвенно через `trap`; ShellCheck не строит такой edge.
# shellcheck disable=SC2329
cleanup() {
    trap - EXIT INT TERM HUP
    if [[ -n "$WATCHDOG" ]]; then
        kill "$WATCHDOG" 2>/dev/null || true
        wait "$WATCHDOG" 2>/dev/null || true
    fi
    if [[ -n "$QPID" ]] && kill -0 "$QPID" 2>/dev/null; then
        kill -TERM "$QPID" 2>/dev/null || true
        # Короткая мягкая остановка, затем гарантированная очистка.
        for _ in 1 2 3 4 5; do
            kill -0 "$QPID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    [[ -f "$LOG" ]] && cp -f "$LOG" "$RESULT_DIR/serial.log"
    [[ -f "$STDERR_LOG" ]] && cp -f "$STDERR_LOG" "$RESULT_DIR/qemu-stderr.log"
    rm -rf "$RUN_DIR"
}
trap cleanup EXIT INT TERM HUP

echo "[test] qemu accel=$ACCEL, cpu=$CPU_MODEL, cpus=$CPUS, memory=${MEMORY_MB}MiB, timeout=${TIMEOUT}s"
qemu-system-x86_64 \
    -machine q35 -cpu "$CPU_MODEL" -smp "$CPUS" -m "$MEMORY_MB" \
    -accel "$ACCEL" \
    -device virtio-vga,edid=on,xres=1280,yres=800 \
    -drive if=pflash,format=raw,readonly=on,file=build/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file="$VARS" \
    -drive if=none,id=systemdisk,format=raw,file=build/system.vfs \
    -device virtio-blk-pci,drive=systemdisk,disable-modern=on,addr=0x5 \
    -drive if=virtio,format=raw,readonly=on,file=build/esp.img \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -serial file:"$LOG" -display none -no-reboot \
    >/dev/null 2>"$STDERR_LOG" &
QPID=$!

(
    sleep "$TIMEOUT"
    if kill -0 "$QPID" 2>/dev/null; then
        : >"$TIMED_OUT"
        kill -TERM "$QPID" 2>/dev/null || true
    fi
) &
WATCHDOG=$!

wait "$QPID"
RC=$?
kill "$WATCHDOG" 2>/dev/null || true
wait "$WATCHDOG" 2>/dev/null || true
WATCHDOG=""
QPID=""

fail=0
if [[ -f "$TIMED_OUT" ]]; then
    echo "[test] FAIL: timeout ${TIMEOUT}s"
    fail=1
fi

# isa-debug-exit возвращает (guest_code << 1) | 1. Значит guest success=0
# наблюдается хостом как RC=1. RC=0 здесь означал бы обычное завершение QEMU,
# например после SIGTERM, и не может считаться успехом гостя.
if [[ $RC -ne 1 ]]; then
    echo "[test] FAIL: код QEMU=$RC, ожидался 1 (guest isa-debug-exit code 0)"
    fail=1
else
    echo "[test] OK: isa-debug-exit guest code=0 (QEMU rc=1)"
fi

patterns=(
    "\[grub\] Multiboot2 tags normalized; installing identity map"
    "\[grub\] long-mode identity map ready; entering kernel"
    "RustOS 0.1.0"
    "\[boot\] BootInfo v[1-9][0-9]* ok"
    "\[boot\] usable RAM: [1-9][0-9]* MiB"
    "\[boot\] memory map: [1-9][0-9]* regions"
    "\[selftest\] framebuffer first pixel = 0x[0-9a-f]*"
    "\[process\] init.rune exited cleanly; VFS capability verified"
    "\[isolation\] user #UD contained; kernel and GUI continue"
    "\[memory\] user address spaces reclaimed"
    "\[irq\] controller=(xAPIC|x2APIC) boot-cpu=[0-9]+ counter-MHz=[1-9][0-9]* timer=(periodic|tsc-deadline)"
    "\[smp\] discovery=ACPI MADT discovered=${CPUS} online=${CPUS} APs parked safely"
    "\[preempt\] timer ticks=[1-9][0-9]* context-switches=[1-9][0-9]*"
    "\[isolation\] concurrent #UD terminated one process; survivor exited=22"
    "\[ipc\] queued block/wake and attenuated VFS capability verified"
    "\[abi-v4\] spawn/wait/kill threads VM shared-memory TLS clock verified"
    "\[graphics-abi-v6\] exclusive scanout atomic-present estimated-vblank supervisor-restart verified"
    "\[std-startup\] ordinary fn main argv and process-local environment verified"
    "\[std\] allocator fs threads futex process pipes stdio native SDK and VFS executable verified in ring3 RUNE"
    "\[vfsd\] open/read/write/seek/readdir/create/rename over shared memory verified"
    "\[vfsd\] restart recovered committed VaraniaFS metadata and file data"
    "\[loader\] RUNE interfaces imports ABI TLS RELRO and cross-process shared RX verified"
    "\[process-manager\] dynamic create/exit/reap reclaimed all frames"
    "\[scheduler\] priority, affinity and fault-containment policy verified"
    "\[microkernel\] RING3_MILESTONE_OK"
    "\[boot\] kernel test done, exit code=0"
)
for pattern in "${patterns[@]}"; do
    if grep -Eq "$pattern" "$LOG"; then
        echo "[test] OK: $pattern"
    else
        echo "[test] FAIL: в serial нет: $pattern"
        fail=1
    fi
done

if [[ $fail -eq 0 ]]; then
    echo "[test] PASS"
    exit 0
fi

echo "[test] FAIL — последние 40 строк serial:"
tail -n 40 "$LOG" 2>/dev/null || true
if [[ -s "$STDERR_LOG" ]]; then
    echo "[test] QEMU stderr:"
    tail -n 30 "$STDERR_LOG"
fi
exit 1

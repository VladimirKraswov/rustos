#!/usr/bin/env bash
# Воспроизводимый ARM boot-тест: AAVMF -> BOOTAA64.EFI -> EL1/EL0 -> PSCI off.
#
# Тест всегда использует отдельные writable NVRAM и VaraniaFS-копию. Это
# исключает влияние предыдущего интерактивного запуска и параллельный lock
# одного образа двумя экземплярами QEMU.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

[[ -f build/arm/esp-arm.img ]] || {
    echo "нет build/arm/esp-arm.img — сначала: RUSTOS_BOOT_TEST=1 make build-arm" >&2
    exit 1
}
[[ -f build/arm-system.vfs ]] || {
    echo "нет build/arm-system.vfs — сначала: make build-arm" >&2
    exit 1
}
bash scripts/bootstrap-arm-firmware.sh >/dev/null

ACCEL=tcg
case "$(uname -m)" in
    arm64|aarch64)
        [[ -w /dev/kvm ]] && ACCEL=kvm
        ;;
esac
DEFAULT_TIMEOUT=180
[[ "$ACCEL" == "kvm" ]] && DEFAULT_TIMEOUT=90
TIMEOUT="${ARM_BOOT_TEST_TIMEOUT:-$DEFAULT_TIMEOUT}"
MEMORY_MB="${ARM_BOOT_MEMORY_MB:-512}"
CPUS="${ARM_BOOT_CPUS:-2}"
CPU_MODEL="${ARM_BOOT_CPU_MODEL:-cortex-a72}"
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ ]] || {
    echo "ARM_BOOT_TEST_TIMEOUT должен быть положительным числом секунд" >&2
    exit 2
}
[[ "$MEMORY_MB" =~ ^[1-9][0-9]*$ ]] || {
    echo "ARM_BOOT_MEMORY_MB должен быть положительным числом MiB" >&2
    exit 2
}
[[ "$CPUS" =~ ^[1-9][0-9]*$ ]] || {
    echo "ARM_BOOT_CPUS должен быть положительным числом" >&2
    exit 2
}

RESULT_DIR="$ROOT/build/test-results/arm-boot"
mkdir -p "$RESULT_DIR"
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustos-arm-boot.XXXXXX")"
VARS="$RUN_DIR/AAVMF_VARS.fd"
SYSTEM_DISK="$RUN_DIR/system.vfs"
LOG="$RUN_DIR/serial.log"
STDERR_LOG="$RUN_DIR/qemu-stderr.log"
TIMED_OUT="$RUN_DIR/timed-out"
cp -f build/arm-firmware/edk2-aarch64-vars-template.fd "$VARS"
# Исходный image sparse; обычный cp сохраняет holes на macOS и GNU/Linux.
cp -f build/arm-system.vfs "$SYSTEM_DISK"

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

echo "[test-arm] qemu accel=$ACCEL, cpu=$CPU_MODEL, cpus=$CPUS, memory=${MEMORY_MB}MiB, timeout=${TIMEOUT}s"
qemu-system-aarch64 \
    -machine virt,gic-version=3,acpi=off \
    -cpu "$CPU_MODEL" -smp "$CPUS" -m "$MEMORY_MB" -accel "$ACCEL" \
    -drive if=pflash,format=raw,readonly=on,file=build/arm-firmware/edk2-aarch64-code.fd \
    -drive if=pflash,format=raw,file="$VARS" \
    -drive if=none,id=systemdisk,format=raw,file="$SYSTEM_DISK" \
    -device virtio-blk-device,drive=systemdisk \
    -global virtio-mmio.force-legacy=false \
    -drive if=virtio,format=raw,readonly=on,file=build/arm/esp-arm.img \
    -serial file:"$LOG" -monitor none -display none -no-reboot \
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
    echo "[test-arm] FAIL: timeout ${TIMEOUT}s"
    fail=1
fi
# PSCI SYSTEM_OFF штатно завершает qemu-system-aarch64 с кодом 0. Успех
# дополнительно защищён guest-маркерами ниже, поэтому простой ранний exit не
# может быть принят за пройденный тест.
if [[ $RC -ne 0 ]]; then
    echo "[test-arm] FAIL: код QEMU=$RC, ожидался 0 (PSCI SYSTEM_OFF)"
    fail=1
else
    echo "[test-arm] OK: PSCI SYSTEM_OFF (QEMU rc=0)"
fi

patterns=(
    "RustOS 0.1.0 .*\(AArch64\)"
    "\[boot\] firmware=Device Tree root=0x0*[1-9a-f][0-9a-f]*"
    "\[arch\] backend=AArch64 exceptions=EL1/VBAR"
    "\[process\] init.rune exited cleanly; VFS capability verified"
    "\[fault\] contained pid=.* vector=60"
    "\[isolation\] user #UD contained; kernel and GUI continue"
    "\[irq\] controller=GICv3 boot-cpu=0 counter-MHz=[1-9][0-9]* timer=generic-one-shot"
    "\[smp\] discovery=Device Tree \+ PSCI discovered=${CPUS} online=${CPUS} APs parked safely"
    "\[isolation\] concurrent #UD terminated one process; survivor exited=22"
    "\[ipc\] queued block/wake and attenuated VFS capability verified"
    "\[abi-v4\] spawn/wait/kill threads VM shared-memory TLS clock verified"
    "\[std\] allocator fs threads futex process pipes stdio native SDK and VFS executable verified in ring3 RUNE"
    "\[vfsd\] restart recovered committed VaraniaFS metadata and file data"
    "\[loader\] RUNE interfaces imports ABI TLS RELRO and cross-process shared RX verified"
    "\[microkernel\] RING3_MILESTONE_OK"
    "\[boot\] kernel test done, exit code=0"
)
for pattern in "${patterns[@]}"; do
    if grep -Eq "$pattern" "$LOG"; then
        echo "[test-arm] OK: $pattern"
    else
        echo "[test-arm] FAIL: в serial нет: $pattern"
        fail=1
    fi
done

preempt_line="$(grep -E '\[preempt\] timer ticks=[0-9]+ context-switches=[0-9]+' "$LOG" | tail -n 1)"
timer_ticks="$(printf '%s\n' "$preempt_line" | sed -E 's/.*ticks=([0-9]+).*/\1/')"
context_switches="$(printf '%s\n' "$preempt_line" | sed -E 's/.*context-switches=([0-9]+).*/\1/')"
if [[ "$timer_ticks" =~ ^[0-9]+$ && "$context_switches" =~ ^[0-9]+$ ]] \
    && (( timer_ticks >= 2 && context_switches >= 2 )); then
    echo "[test-arm] OK: timer ticks=$timer_ticks context-switches=$context_switches"
else
    echo "[test-arm] FAIL: нужны timer ticks>=2 и context-switches>=2"
    fail=1
fi

if [[ $fail -eq 0 ]]; then
    echo "[test-arm] PASS"
    exit 0
fi

echo "[test-arm] FAIL — последние 50 строк serial:"
tail -n 50 "$LOG" 2>/dev/null || true
if [[ -s "$STDERR_LOG" ]]; then
    echo "[test-arm] QEMU stderr:"
    tail -n 30 "$STDERR_LOG"
fi
exit 1

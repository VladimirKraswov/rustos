#!/usr/bin/env bash
# Интеграционный GUI-тест: boot, клавиатура, terminal, мышь, window manager.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
bash scripts/build.sh >/dev/null
bash scripts/bootstrap-ovmf.sh >/dev/null
cargo build -q -p rustos-hmp
HMP_TOOL="$ROOT/target/debug/rustos-hmp"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustos-gui-test.XXXXXX")"
RESULT_DIR="$ROOT/build/test-results/gui"
mkdir -p "$RESULT_DIR"
cp -f build/ovmf/OVMF_VARS.fd "$RUN_DIR/VARS.fd"

QPID=""
GUI_TIMEOUT="${GUI_TEST_TIMEOUT:-360}"
MEMORY_MB="${GUI_MEMORY_MB:-128}"
CPU_MODEL="${GUI_CPU_MODEL:-max}"
[[ "$MEMORY_MB" =~ ^[1-9][0-9]*$ ]] || {
    echo "GUI_MEMORY_MB должен быть положительным числом MiB" >&2
    exit 2
}
hmp() {
    "$HMP_TOOL" "$RUN_DIR/monitor.sock" "${1:-0}" >/dev/null
}

# Ввод ASCII-команды через настоящий виртуальный PS/2 controller. HMP здесь
# только нажимает клавиши; guest всё равно проходит тот же scancode parser,
# terminal и VFS command path, что и пользователь.
send_command() {
    local value="$1"
    local commands=""
    local character key lower index
    for ((index = 0; index < ${#value}; index++)); do
        character="${value:index:1}"
        case "$character" in
            ' ') key=spc ;;
            '/') key=slash ;;
            '.') key=dot ;;
            '-') key=minus ;;
            [A-Z])
                # HMP использует имена физических клавиш: заглавная буква
                # — это комбинация Shift + соответствующая US-клавиша.
                lower="$(printf '%s' "$character" | tr '[:upper:]' '[:lower:]')"
                key="shift-${lower}"
                ;;
            *) key="$character" ;;
        esac
        commands="${commands}sendkey ${key} 20\n"
    done
    # Один persistent HMP connection, prompt после каждой команды и короткая
    # пауза после отпускания клавиши не переполняют виртуальный 8042. Guest
    # обновляет только dirty-строку ввода, поэтому полный software-render
    # больше не задерживает чтение следующего scancode.
    # 160 ms учитывает самый медленный macOS/Apple-Silicon TCG: HMP prompt
    # подтверждает принятие команды monitor'ом, но не окончание отложенной
    # make/break-пары PS/2 внутри гостя.
    printf '%b' "$commands" | hmp 160
    # Enter отправляем отдельно: QEMU возвращает monitor prompt раньше, чем
    # release последней буквы гарантированно покинет PS/2 output buffer.
    sleep 0.2
    printf 'sendkey ret 20\n' | hmp 100
}

wait_for_serial() {
    local pattern="$1"
    local _
    for _ in $(seq 1 80); do
        grep -Fq "$pattern" "$RUN_DIR/serial.log" && return 0
        sleep 0.1
    done
    echo "[gui-test] serial marker timeout: $pattern" >&2
    return 1
}

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
        # Убиваем только конкретный QEMU этого теста. Эта ветка нужна, чтобы
        # CI никогда не зависал навсегда на неисправном monitor shutdown.
        kill -KILL "$QPID" 2>/dev/null || true
    fi
    wait "$QPID" 2>/dev/null || true
    QPID=""
}

cleanup() {
    trap - EXIT INT TERM HUP
    stop_qemu
    for file in serial.log qemu-stderr.log terminal.ppm dragged.ppm minimized.ppm; do
        [[ -f "$RUN_DIR/$file" ]] && cp -f "$RUN_DIR/$file" "$RESULT_DIR/$file"
    done
}
trap cleanup EXIT INT TERM HUP

qemu-system-x86_64 \
    -machine q35 -cpu "$CPU_MODEL" -smp 2 -m "$MEMORY_MB" -accel tcg \
    -device virtio-vga,edid=on,xres=1280,yres=800 \
    -drive if=pflash,format=raw,readonly=on,file=build/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file="$RUN_DIR/VARS.fd" \
    -drive if=none,id=systemdisk,format=raw,file=build/system.vfs \
    -device virtio-blk-pci,drive=systemdisk,disable-modern=on,addr=0x5 \
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
grep -q '\[microkernel\] RING3_MILESTONE_OK' "$RUN_DIR/serial.log"
grep -Eq '\[video\] scanout=grub-fb mode=[1-9][0-9]*x[1-9][0-9]* format=(rgb888|bgr888) present=immediate page-flip=no' \
    "$RUN_DIR/serial.log"
grep -q '\[isolation\] user #UD contained; kernel and GUI continue' "$RUN_DIR/serial.log"
grep -q '\[memory\] user address spaces reclaimed' "$RUN_DIR/serial.log"
grep -Eq '\[smp\] discovery=ACPI MADT discovered=2 online=2 APs parked safely' "$RUN_DIR/serial.log"
grep -Eq '\[preempt\] timer ticks=[1-9][0-9]* context-switches=[1-9][0-9]*' "$RUN_DIR/serial.log"
grep -q '\[isolation\] concurrent #UD terminated one process; survivor exited=22' "$RUN_DIR/serial.log"
grep -q '\[ipc\] queued block/wake and attenuated VFS capability verified' "$RUN_DIR/serial.log"
grep -q '\[process-manager\] dynamic create/exit/reap reclaimed all frames' "$RUN_DIR/serial.log"
grep -q '\[scheduler\] priority, affinity and fault-containment policy verified' "$RUN_DIR/serial.log"

# Команда идёт через настоящий PS/2 keyboard path.
send_command 'help'
wait_for_serial '[terminal] command: help'

# Display manager: monitor info, runtime software color profile and honest
# firmware mode-set boundary. Возвращаем truecolor до screenshot-проверок.
send_command 'display'
wait_for_serial '[display] info driver=grub-fb mode='
send_command 'display color gray8'
wait_for_serial '[display] color=gray8'
send_command 'display color truecolor'
wait_for_serial '[display] color=truecolor24'
send_command 'display mode 1280x720'
wait_for_serial '[display] mode request=1280x720 result=reboot-required'

# Полный bootstrap filesystem workflow: initramfs listing, RAM-file write,
# read и cwd-relative source directory. Проверяем не только shell parser, но
# и реальные VFS operation markers.
send_command 'ls'
wait_for_serial '[vfs] LIST path=/ value='
send_command 'write note hello'
wait_for_serial '[vfs] WRITE path=/note value=5'
send_command 'cat note'
wait_for_serial '[vfs] READ path=/note value=5'
send_command 'cd src'
wait_for_serial '[vfs] CHDIR path=/src value=0'
send_command 'write main rust'
wait_for_serial '[vfs] WRITE path=/src/main value=4'
send_command 'cat /boot/README.txt'
wait_for_serial '[vfs] READ path=/boot/README.txt value='
# Desktop terminal теперь действительно запускает изолированное приложение с
# persistent VaraniaFS, а не вызывает ещё одну kernel-команду.
send_command 'run /apps/examples/hello.rune gui'
wait_for_serial '[terminal-run] path=/apps/examples/hello.rune status=0 exception=0 output='
# Serial marker появляется непосредственно после обработки события, а QEMU
# обновляет display surface по таймеру. Небольшая пауза ДО screendump не даёт
# тесту случайно прочитать предыдущий кадр compositor'а.
sleep 0.25
printf 'screendump %s/terminal.ppm\n' "$RUN_DIR" \
    | hmp

# Настоящий drag: курсор стартует в центре 1280x800. Перемещаемся на
# заголовок, удерживаем левую кнопку и сдвигаем окно вправо-вниз.
printf 'mouse_move 0 -325\n' | hmp
sleep 0.1
printf 'mouse_button 1\n' | hmp
for _ in $(seq 1 40); do
    grep -q '\[wm\] terminal drag started' "$RUN_DIR/serial.log" && break
    sleep 0.1
done
grep -q '\[wm\] terminal drag started' "$RUN_DIR/serial.log"
# Не один большой скачок, а серия движений: это regression для preview path.
# Пауза не даёт искусственному HMP producer переполнить одно-byte 8042 быстрее,
# чем это вообще способна сделать физическая PS/2-мышь.
drag_commands=""
for _ in $(seq 1 4); do
    drag_commands="${drag_commands}mouse_move 30 20\n"
done
printf '%b' "$drag_commands" | hmp 30
sleep 0.2
printf 'mouse_button 0\n' | hmp
for _ in $(seq 1 40); do
    grep -q '\[wm\] terminal drag finished' "$RUN_DIR/serial.log" && break
    sleep 0.1
done
grep -q '\[wm\] terminal drag finished' "$RUN_DIR/serial.log"
grep -Eq '\[wm\] terminal drag finished frames=[1-9][0-9]* packets=[1-9][0-9]* present-kpx=[1-9][0-9]* compositor=preview' \
    "$RUN_DIR/serial.log"
sleep 0.25
printf 'screendump %s/dragged.ppm\n' "$RUN_DIR" | hmp

# После drag курсор находится около (760,155), а окно упёрлось в правую
# границу и целиком осталось над taskbar. Перемещаемся к minimize-кнопке.
printf 'mouse_move 435 -20\n' | hmp
sleep 0.2
printf 'mouse_button 1\nmouse_button 0\n' | hmp
for _ in $(seq 1 40); do
    grep -q '\[wm\] terminal minimized' "$RUN_DIR/serial.log" && break
    sleep 0.1
done
grep -q '\[wm\] terminal minimized' "$RUN_DIR/serial.log"
wait_for_serial '[wm] frame committed minimized=1'
printf 'screendump %s/minimized.ppm\n' "$RUN_DIR" \
    | hmp

cargo run -q -p rustos-gui-check -- \
    "$RUN_DIR/terminal.ppm" "$RUN_DIR/dragged.ppm" "$RUN_DIR/minimized.ppm"
printf 'quit\n' | hmp || true
# HMP `quit` обычно завершает QEMU сразу, но закрытие Unix socket и обработка
# команды могут состязаться. Bounded shutdown исключает вечный `wait` в CI.
for _ in $(seq 1 20); do
    kill -0 "$QPID" 2>/dev/null || break
    sleep 0.1
done
stop_qemu
echo "[gui-test] PASS: keyboard, VFS + ring3 RUN, buffered drag and minimize"

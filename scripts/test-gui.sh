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
            '.') key='dot' ;;
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

# Большие relative jumps QEMU раскладывает на несколько PS/2 packets. Если
# сразу поставить mouse-down, он может обогнать хвост движения в 8042 queue.
# Дробим автоматические перемещения так же, как это делает физическая мышь.
move_mouse() {
    local dx="$1"
    local dy="$2"
    local steps="${3:-4}"
    local abs_x=${dx#-}
    local abs_y=${dy#-}
    local required_x=$(((abs_x + 95) / 96))
    local required_y=$(((abs_y + 95) / 96))
    # Один PS/2 packet кодирует signed delta. Не полагаемся на то, что QEMU
    # сам одинаково раздробит скачок >127 во всех версиях/режимах мыши.
    ((steps < required_x)) && steps=$required_x
    ((steps < required_y)) && steps=$required_y
    local step_x=$((dx / steps))
    local step_y=$((dy / steps))
    local sent_x=0
    local sent_y=0
    local index part_x part_y
    for ((index = 1; index <= steps; index++)); do
        if ((index == steps)); then
            part_x=$((dx - sent_x))
            part_y=$((dy - sent_y))
        else
            part_x="$step_x"
            part_y="$step_y"
        fi
        printf 'mouse_move %d %d\n' "$part_x" "$part_y" | hmp
        sent_x=$((sent_x + part_x))
        sent_y=$((sent_y + part_y))
        sleep 0.1
    done
    # `mouse_move` подтверждается QEMU monitor'ом до того, как
    # guest успеет вычитать последний PS/2 packet. Под TCG без settle
    # mouse-down изредка приходил в предыдущую координату и ложно
    # ломал double-click/close lifecycle checks.
    sleep "${GUI_MOUSE_SETTLE_SECONDS:-0.25}"
}

# Критичные lifecycle-clicks не должны зависеть от накопленного
# relative PS/2 remainder. Снача гарантированно упираемся в (0, 0),
# затем идём в абсолютную guest-координату малыми пакетами.
move_mouse_to() {
    local x="$1"
    local y="$2"
    move_mouse -2048 -2048 20
    move_mouse "$x" "$y" 12
}

wait_for_serial() {
    local pattern="$1"
    local _
    # Некоторые ring-3 команды создают несколько процессов и на macOS/TCG
    # законно занимают больше восьми секунд. Короткий polling interval
    # сохраняем, но даём интеграционному пути до 30 секунд.
    for _ in $(seq 1 300); do
        grep -Fq "$pattern" "$RUN_DIR/serial.log" && return 0
        sleep 0.1
    done
    echo "[gui-test] serial marker timeout: $pattern" >&2
    return 1
}

wait_for_serial_count() {
    local pattern="$1"
    local expected="$2"
    local count _
    for _ in $(seq 1 300); do
        count=$(grep -Fc "$pattern" "$RUN_DIR/serial.log" || true)
        ((count >= expected)) && return 0
        sleep 0.1
    done
    echo "[gui-test] serial marker count timeout: $pattern expected=$expected" >&2
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
    for file in serial.log qemu-stderr.log start-menu.ppm desktop-menu.ppm desktop-settings.ppm mode-720.ppm fonts.ppm cursor-busy.ppm wallpaper-autumn.ppm lifecycle.ppm terminal.ppm ui-gallery.ppm explorer.ppm dragged.ppm resized.ppm minimized.ppm; do
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
grep -Fq '[graphics-abi-v6] exclusive scanout atomic-present estimated-vblank supervisor-restart verified' \
    "$RUN_DIR/serial.log"
grep -Fq '[supervisor] persistent displayd/compositord atomic-present services ready' \
    "$RUN_DIR/serial.log"
grep -Eq '\[video\] scanout=virtio-gpu mode=1280x800 format=bgr888 present=immediate page-flip=no' \
    "$RUN_DIR/serial.log"
grep -q '\[display-metrics\] logical=1280x800 physical=1280x800 device-scale-milli=1000 framebuffer=1280x800 compositor-scale-milli=1000' \
    "$RUN_DIR/serial.log"
grep -q '\[isolation\] user #UD contained; kernel and GUI continue' "$RUN_DIR/serial.log"
grep -q '\[memory\] user address spaces reclaimed' "$RUN_DIR/serial.log"
grep -Eq '\[smp\] discovery=ACPI MADT discovered=2 online=2 APs parked safely' "$RUN_DIR/serial.log"
grep -Eq '\[preempt\] timer ticks=[1-9][0-9]* context-switches=[1-9][0-9]*' "$RUN_DIR/serial.log"
grep -q '\[isolation\] concurrent #UD terminated one process; survivor exited=22' "$RUN_DIR/serial.log"
grep -q '\[ipc\] queued block/wake and attenuated VFS capability verified' "$RUN_DIR/serial.log"
grep -q '\[process-manager\] dynamic create/exit/reap reclaimed all frames' "$RUN_DIR/serial.log"
grep -q '\[scheduler\] priority, affinity and fault-containment policy verified' "$RUN_DIR/serial.log"
grep -Eq '\[clock\] source=cmos-rtc time=[0-2][0-9]:[0-5][0-9] date=[0-3][0-9]\.[01][0-9]\.[0-9]{4}' \
    "$RUN_DIR/serial.log"

# Start является настоящим system-ui tree. Down/Up проходят общий pointer
# capture, а popup состоит из Menu/Button/Image/Text и не меняет WindowId.
move_mouse -578 377 5
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[start] opened component-runtime=system-ui-v1'
sleep 0.25
printf 'screendump %s/start-menu.ppm\n' "$RUN_DIR" | hmp
[[ -s "$RUN_DIR/start-menu.ppm" ]] || {
    echo "[gui-test] component Start screenshot is empty" >&2
    exit 1
}
# Повторное нажатие той же Button закрывает popup; возвращаем курсор в центр,
# чтобы существующий lifecycle сценарий сохранил детерминированные координаты.
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[start] closed component-runtime=system-ui-v1'
move_mouse 578 -377 5
sleep 0.2

# Проверяем настоящий double-click и создание второго независимого экземпляра:
# стартовая точка курсора детерминирована (640,400), icon — около (65,78).
move_mouse_to 65 78
sleep 0.4
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
sleep 0.12
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[app] spawn id=0x02 kind=TERMINAL'
wait_for_serial '[desktop] new terminal requested by double-click'
move_mouse 575 322 4
sleep 0.3

# Lifecycle regression: меняем process-local cwd второго shell, закрываем его
# системной кнопкой и создаём новый. Новый WindowId и PWD=/ доказывают, что X
# уничтожил application state, а не спрятал старый объект. Первый terminal
# остаётся живым за ним — одновременно существует больше одного окна.
send_command 'cd src'
wait_for_serial '[vfs] CHDIR path=/src value=0'
send_command 'write /lifecycle shared'
wait_for_serial '[vfs] WRITE path=/lifecycle value=6'
# id=2: rect≈(148,85,1040,640), close center≈(1170,102).
move_mouse_to 1170 102
sleep 0.15
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[app] exit id=0x02 kind=TERMINAL released-frames='
grep -q 'windows=1' "$RUN_DIR/serial.log"
# Снова double-click по desktop icon; ID не переиспользуется.
move_mouse_to 65 78
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
sleep 0.12
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[app] spawn id=0x03 kind=TERMINAL'
move_mouse 575 322 4
sleep 0.25
send_command 'pwd'
wait_for_serial '[vfs] PWD path=/ value=0'
send_command 'cat /lifecycle'
wait_for_serial '[vfs] READ path=/lifecycle value=6'
printf 'screendump %s/lifecycle.ppm\n' "$RUN_DIR" | hmp

# Команда идёт через настоящий PS/2 keyboard path.
send_command 'help'
wait_for_serial '[terminal] command: help'

# System fonts: family, bold+italic and variable em-size are changed through
# the same PS/2 terminal path as a human uses. Then restore the default
# console profile so the remaining screenshot geometry is deterministic.
send_command 'font family sans'
wait_for_serial '[font] terminal family=sans size=18 style=regular'
send_command 'font style bolditalic'
wait_for_serial '[font] terminal family=sans size=18 style=bolditalic'
send_command 'font size 20'
wait_for_serial '[font] terminal family=sans size=20 style=bolditalic'
sleep 0.25
printf 'screendump %s/fonts.ppm\n' "$RUN_DIR" | hmp
send_command 'font family console'
wait_for_serial '[font] terminal family=console size=20 style=bolditalic'
send_command 'font style regular'
wait_for_serial '[font] terminal family=console size=20 style=regular'
send_command 'font size 18'
wait_for_serial '[font] terminal family=console size=18 style=regular'

# Input/resource services: hardware PS/2 rate plus portable software profile,
# animated cursor preview and hot-swappable visual packs.
send_command 'mouse rate 200'
wait_for_serial '[input] mouse profile updated rate=200'
send_command 'mouse sensitivity 125'
wait_for_serial '[input] mouse profile updated rate=200 sensitivity=125%'
send_command 'mouse double 500'
wait_for_serial '[input] mouse profile updated rate=200 sensitivity=125% double-ms=500'
send_command 'cursor preview busy'
wait_for_serial '[cursor] value=BUSY theme=light'
sleep 0.25
printf 'screendump %s/cursor-busy.ppm\n' "$RUN_DIR" | hmp
send_command 'cursor auto'
wait_for_serial '[cursor] value=AUTO theme=light'
send_command 'icons theme midnight'
wait_for_serial '[assets] icon-pack=midnight'
send_command 'icons theme classic'
wait_for_serial '[assets] icon-pack=classic'
send_command 'wallpaper autumn'
wait_for_serial '[desktop] wallpaper=autumn'
sleep 0.25
printf 'screendump %s/wallpaper-autumn.ppm\n' "$RUN_DIR" | hmp
send_command 'wallpaper spring'
wait_for_serial '[desktop] wallpaper=spring'
send_command 'mouse sensitivity 100'
wait_for_serial '[input] mouse profile updated rate=200 sensitivity=100% double-ms=500'

# Display manager: monitor info, runtime software color profile and honest
# firmware mode-set boundary. Возвращаем truecolor до screenshot-проверок.
send_command 'display'
wait_for_serial '[display] info driver=virtio-gpu mode=1280x800'
send_command 'display modes'
wait_for_serial '[display] modes count='
send_command 'display color gray8'
wait_for_serial '[display] color=gray8'
send_command 'display color truecolor'
wait_for_serial '[display] color=truecolor24'
send_command 'display mode 1280x720'
wait_for_serial '[display] mode request=1280x720 result=active'
sleep 0.25
printf 'screendump %s/mode-720.ppm\n' "$RUN_DIR" | hmp
[[ "$(sed -n '2p' "$RUN_DIR/mode-720.ppm")" == "1280 720" ]] || {
    echo "[gui-test] native mode-set did not resize QEMU surface to 1280x720" >&2
    exit 1
}
send_command 'display mode 1280x800'
wait_for_serial '[display] mode request=1280x800 result=active'

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

# Новый System UI проходит тот же keyboard/window path. Проверяем запуск
# декларативного component tree и сохраняем отдельный screenshot artifact.
send_command 'uidemo'
wait_for_serial '[app] spawn id=0x04 kind=UI GALLERY'
wait_for_serial '[ui] Gallery opened runtime=system-ui-v1 independent-window=1'
sleep 0.25
printf 'screendump %s/ui-gallery.ppm\n' "$RUN_DIR" | hmp
[[ -s "$RUN_DIR/ui-gallery.ppm" ]] || {
    echo "[gui-test] UI Gallery screenshot is empty" >&2
    exit 1
}

# Закрываем UI Gallery, затем первый boot terminal. Третий terminal остаётся
# живым и независимым; дальнейшие drag/resize/minimize проверяют именно его.
# UI создаётся уже после mode round-trip: rect≈(204,106,1040,640),
# close center≈(1226,123).
move_mouse_to 1226 123
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[app] exit id=0x04 kind=UI GALLERY released-frames='

# Проводник запускается как отдельное приложение через terminal command, а не
# подменяет содержимое существующего окна. Нажатие toolbar Button создаёт
# настоящий VFS-каталог; screenshot проверяет системные folder icons и layout.
send_command 'explorer'
wait_for_serial '[app] spawn id=0x05 kind=EXPLORER'
wait_for_serial '[explorer] operation=READY path=/'
# id=5: rect≈(232,104,1040,640). После отдельного menu bar и address bar
# центр component-кнопки «Новая папка» находится в toolbar около (639,198).
move_mouse_to 639 198
# Сохраняем и первый кадр: при ошибке hit-test он остаётся полезным visual
# artifact, а при успехе ниже будет заменён состоянием после создания папки.
printf 'screendump %s/explorer.ppm\n' "$RUN_DIR" | hmp
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[explorer] operation=MKDIR path=/Новая папка'
# Down/Up и command должны пройти application-local damage path. При
# 1280×800 полный кадр равен 1024 kpx; лимит 299 kpx ловит возврат регрессии
# «один control → present всего framebuffer» без зависимости от того, успел
# ли TCG объединить промежуточные hover packets.
wait_for_serial '[compositor] repaint=incremental scope=application'
grep -Eq '\[compositor\] repaint=incremental scope=application rects=[1-9][0-9]* present-kpx=([0-9]{1,2}|[12][0-9]{2}) full-screen=no' \
    "$RUN_DIR/serial.log"
sleep 0.25
printf 'screendump %s/explorer.ppm\n' "$RUN_DIR" | hmp
[[ -s "$RUN_DIR/explorer.ppm" ]] || {
    echo "[gui-test] Explorer screenshot is empty" >&2
    exit 1
}
# Закрытие освобождает application frames. Возвращаем cursor в прежнюю точку,
# чтобы оставшаяся geometry-regression не зависела от нового сценария.
move_mouse_to 1254 121
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[app] exit id=0x05 kind=EXPLORER released-frames='
move_mouse -28 2 1

# taskbar id=1 (первая кнопка), затем его close center≈(1142,43).
move_mouse -1012 656 7
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[wm] focus id=0x01'
move_mouse 928 -736 7
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[app] exit id=0x01 kind=TERMINAL released-frames='

# Reflow малого 1280x720 режима оставил id=3 в (176,26). Перед базовым
# screenshot ставим его в прежнюю детерминированную geometry (120,57), чтобы
# visual checker сравнивал именно движение, а не смену набора окон.
move_mouse -542 0 4
printf 'mouse_button 1\n' | hmp
wait_for_serial '[wm] drag started id=0x03'
printf 'mouse_move -56 31\n' | hmp
sleep 0.2
printf 'mouse_button 0\n' | hmp
wait_for_serial '[wm] drag finished id=0x03'
sleep 0.25
# Базовый кадр для geometry verifier: те же слои, что в dragged/minimized,
# но до начала жеста. Ранний terminal screenshot с несколькими окнами уже
# выполнил свою проверку, поэтому здесь безопасно обновить artifact.
printf 'screendump %s/terminal.ppm\n' "$RUN_DIR" | hmp

# Настоящий drag оставшегося id=3: cursor после позиционирования находится
# около (544,74); выбираем свободную точку title и сдвигаем окно вправо-вниз.
move_mouse 56 0 1
printf 'mouse_button 1\n' | hmp
wait_for_serial '[wm] drag started id=0x03'
drag_finished_before=$(grep -Fc '[wm] drag finished id=0x03' "$RUN_DIR/serial.log" || true)
# Не один большой скачок, а серия движений: это regression для preview path.
# Пауза не даёт искусственному HMP producer переполнить одно-byte 8042 быстрее,
# чем это вообще способна сделать физическая PS/2-мышь.
drag_commands=""
for _ in $(seq 1 4); do
    drag_commands="${drag_commands}mouse_move 30 20\n"
done
# На TCG monitor подтверждает command раньше, чем все три байта PS/2 packet
# дошли до 8042. Частые HMP-команды могут поставить release между байтами
# последнего движения и превратить корректный guest decoder в ложный timeout.
# 100 ms всё ещё намного быстрее человеческого drag, но сохраняет packet
# boundary и проверяет именно четыре независимых movement event.
printf '%b' "$drag_commands" | hmp 100
sleep 0.5
printf 'mouse_button 0\n' | hmp
wait_for_serial_count '[wm] drag finished id=0x03' "$((drag_finished_before + 1))"
grep -Eq '\[wm\] drag finished id=0x03 frames=[1-9][0-9]* packets=[1-9][0-9]* present-kpx=[1-9][0-9]* compositor=layer-cache' \
    "$RUN_DIR/serial.log"
sleep 0.25
printf 'screendump %s/dragged.ppm\n' "$RUN_DIR" | hmp

# Resize за правый верхний угол проверяет обе оси и layer cache с несколькими
# окнами. Cursor после drag около (720,154), настоящий top-right edge —
# около (1279,137): берём точку внутри 6px hit area, а не рядом с ней.
move_mouse 559 -17 4
printf 'mouse_button 1\n' | hmp
wait_for_serial '[wm] resize started id=0x03'
resize_commands=""
for _ in $(seq 1 4); do
    resize_commands="${resize_commands}mouse_move -10 8\n"
done
printf '%b' "$resize_commands" | hmp 30
sleep 0.2
printf 'mouse_button 0\n' | hmp
wait_for_serial '[wm] resize finished id=0x03'
grep -Eq '\[wm\] resize finished id=0x03 frames=[1-9][0-9]* packets=[1-9][0-9]* present-kpx=[1-9][0-9]* compositor=layer-cache' \
    "$RUN_DIR/serial.log"
sleep 0.25
printf 'screendump %s/resized.ppm\n' "$RUN_DIR" | hmp

# После diagonal resize cursor≈(1239,169), итоговый right≈1240, а minimize
# center≈(1160,155).
printf 'mouse_move -79 -14\n' | hmp
sleep 0.2
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[wm] window minimized id=0x03'
# Marker пишется при принятии window command. Даём медленному TCG закончить
# полный wallpaper redraw и доставить virtio-gpu FLUSH в display frontend до
# HMP screendump; 0.3 s на загруженном Apple Silicon хосте было погранично.
sleep "${GUI_FULL_REDRAW_SETTLE_SECONDS:-1.0}"
printf 'screendump %s/minimized.ppm\n' "$RUN_DIR" \
    | hmp

cargo run -q -p rustos-gui-check -- \
    "$RUN_DIR/terminal.ppm" "$RUN_DIR/dragged.ppm" "$RUN_DIR/minimized.ppm"

# Desktop context menu принадлежит shell, открывается настоящей правой
# кнопкой и не проходит сквозь popup. Сначала проверяем Arrange, затем отдельное
# Settings-приложение и две команды его component tree.
move_mouse -560 245 5
printf 'mouse_button 2\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[desktop-menu] opened component-runtime=system-ui-v1 x=600 y=400'
sleep 0.2
printf 'screendump %s/desktop-menu.ppm\n' "$RUN_DIR" | hmp
move_mouse 100 30 2
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[desktop-menu] command=arrange-icons'

# Повторный popup в новой позиции, второй MenuItem — Properties.
printf 'mouse_button 2\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[desktop-menu] opened component-runtime=system-ui-v1 x=700 y=430'
move_mouse 100 84 2
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[app] spawn id=0x06 kind=SETTINGS'
wait_for_serial '[desktop-menu] command=properties'

# Settings rect≈(260,134,760,620): work-area clamp поднимает окно целиком над
# taskbar. QEMU HMP не публикует wheel packet для negotiated PS/2 mouse во
# всех версиях, поэтому интеграционно проверяем тот же ScrollModel через
# фокусированный пункт + PageDown. Wheel routing покрыт SystemUI unit tests.
move_mouse_to 600 390
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
printf 'sendkey pgdn 20\n' | hmp
wait_for_serial '[settings] resolution-list scrolled input=keyboard'
# Выбираем осенние обои и UI scale 125%. Абсолютная постановка делает тест
# независимым от числа wheel packets виртуального PS/2 устройства.
move_mouse_to 639 595
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[settings] wallpaper=autumn'
move_mouse 0 86 2
printf 'mouse_button 1\n' | hmp
sleep 0.08
printf 'mouse_button 0\n' | hmp
wait_for_serial '[settings] ui-scale=1250'
sleep 0.25
printf 'screendump %s/desktop-settings.ppm\n' "$RUN_DIR" | hmp
[[ -s "$RUN_DIR/desktop-menu.ppm" && -s "$RUN_DIR/desktop-settings.ppm" ]] || {
    echo "[gui-test] desktop menu/settings screenshot is empty" >&2
    exit 1
}
printf 'quit\n' | hmp || true
# HMP `quit` обычно завершает QEMU сразу, но закрытие Unix socket и обработка
# команды могут состязаться. Bounded shutdown исключает вечный `wait` в CI.
for _ in $(seq 1 20); do
    kill -0 "$QPID" 2>/dev/null || break
    sleep 0.1
done
stop_qemu
echo "[gui-test] PASS: independent windows + Explorer, desktop popup/settings, lifecycle, VFS + ring3 RUN, buffered drag/resize/minimize"

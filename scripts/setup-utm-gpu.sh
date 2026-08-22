#!/usr/bin/env bash
# Создаёт воспроизводимую UTM/HVF VM с VirtIO GPU VirGL и ANGLE/Metal.
#
# Скрипт намеренно использует публичный AppleScript API UTM. Внутренний
# QEMULauncher является sandboxed XPC helper и не должен запускаться напрямую.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM_NAME="${RUSTOS_UTM_VM_NAME:-RustOS GPU Development}"
MEMORY_MB="${RUSTOS_UTM_MEMORY_MB:-2048}"
CPU_COUNT="${RUSTOS_UTM_CPUS:-4}"
ESP="$ROOT/build/arm/esp-arm.img"
SYSTEM_DISK="$ROOT/build/arm-system.vfs"

if [[ "$(uname -s)" != "Darwin" || ! "$(uname -m)" =~ ^(arm64|aarch64)$ ]]; then
    echo "[utm-gpu] профиль поддерживается только на Apple Silicon macOS" >&2
    exit 2
fi
if [[ ! -d /Applications/UTM.app ]] || ! command -v utmctl >/dev/null 2>&1; then
    echo "[utm-gpu] установите UTM: brew install --cask utm" >&2
    exit 2
fi
if [[ ! -f "$ESP" || ! -f "$SYSTEM_DISK" ]]; then
    echo "[utm-gpu] нет ARM-образов; сначала выполните make build-arm" >&2
    exit 2
fi

open -a UTM

# Сначала создаём только базовый record. UTM иногда открывает новую VM сразу,
# поэтому конфигурацию устройств применяем отдельным шагом после stop.
VM_ID="$(osascript - "$VM_NAME" <<'APPLESCRIPT'
on run argv
    set vmName to item 1 of argv
    tell application "UTM"
        set auto terminate to false
        if exists virtual machine named vmName then
            set vm to virtual machine named vmName
        else
            set vm to make new virtual machine with properties {backend:qemu, configuration:{name:vmName, architecture:"aarch64"}}
        end if
        return id of vm
    end tell
end run
APPLESCRIPT
)"

VM_STATE="$(utmctl status "$VM_ID" 2>/dev/null || true)"
if [[ "$VM_STATE" != "stopped" ]]; then
    utmctl stop "$VM_ID" --force >/dev/null
    for _ in {1..80}; do
        [[ "$(utmctl status "$VM_ID" 2>/dev/null || true)" == "stopped" ]] && break
        sleep 0.25
    done
fi
if [[ "$(utmctl status "$VM_ID" 2>/dev/null || true)" != "stopped" ]]; then
    echo "[utm-gpu] VM не остановилась; конфигурация не изменена" >&2
    exit 3
fi

# UTM default и явный backend 2 используют ANGLE Metal. Записываем выбор
# явно, чтобы пользовательская старая настройка ANGLE OpenGL не победила.
defaults write com.utmapp.UTM QEMURendererBackend -int 2

osascript - "$VM_NAME" "$ESP" "$SYSTEM_DISK" "$MEMORY_MB" "$CPU_COUNT" <<'APPLESCRIPT'
on run argv
    set vmName to item 1 of argv
    set espPath to item 2 of argv
    set systemPath to item 3 of argv
    set memoryMiB to (item 4 of argv) as integer
    set cpuCountValue to (item 5 of argv) as integer
    -- `POSIX file` должен вычисляться до `tell application`: иначе переменная
    -- ошибочно превращается в AppleEvent-команду адресованную UTM.
    set espImage to POSIX file espPath
    set systemImage to POSIX file systemPath

    tell application "UTM"
        set vm to virtual machine named vmName
        set c to configuration of vm
        set memory of c to memoryMiB
        set cpu cores of c to cpuCountValue
        set hypervisor of c to true
        set uefi of c to true
        set machine of c to "virt"

        -- RustOS AArch64 drivers use modern VirtIO MMIO. The `-gl-device`
        -- host model passes VirGL commands to UTM virglrenderer/ANGLE/Metal.
        set displays of c to {{hardware:"virtio-gpu-gl-device", dynamic resolution:false, native resolution:true, upscaling filter:linear, downscaling filter:linear}}

        -- Build artifacts stay the single source of truth. `file urls` asks
        -- UTM to issue security-scoped bookmarks for its XPC QEMU helper.
        set drives of c to {}
        set espArgument to "if=none,media=disk,format=raw,id=rustosesp,file=" & espPath
        set systemArgument to "if=none,media=disk,format=raw,id=rustossystem,file=" & systemPath
        -- UTM сам создаёт input-xHCI и подключает к нему usb-tablet,
        -- usb-mouse и usb-kbd. Нельзя добавлять ещё один input-xHCI вручную:
        -- frontend посылает host events в своё устройство, а guest мог
        -- выбрать одноимённое устройство на другом контроллере. Служебный
        -- controller UTM для usbredir не содержит HID и в input route не
        -- участвует. Absolute tablet — основной capture-free pointer RustOS.
        set qemu additional arguments of c to {{argument string:"-machine"}, {argument string:"virt,acpi=off,gic-version=3"}, {argument string:"-global"}, {argument string:"virtio-mmio.force-legacy=false"}, {argument string:"-drive"}, {argument string:espArgument, file urls:{espImage}}, {argument string:"-device"}, {argument string:"virtio-blk-pci,drive=rustosesp,bootindex=0"}, {argument string:"-drive"}, {argument string:systemArgument, file urls:{systemImage}}, {argument string:"-device"}, {argument string:"virtio-blk-device,drive=rustossystem"}}
        update configuration of vm with c
    end tell
end run
APPLESCRIPT

echo "[utm-gpu] ready: name=$VM_NAME id=$VM_ID backend=VirGL/ANGLE-Metal accel=HVF input=utm-xhci-hid-tablet"

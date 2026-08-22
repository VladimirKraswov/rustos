#!/usr/bin/env bash
# Пересобирает ARM-образ и открывает ускоренную RustOS VM в UTM.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM_NAME="${RUSTOS_UTM_VM_NAME:-RustOS GPU Development}"
cd "$ROOT"

if [[ ! -d /Applications/UTM.app ]] || ! command -v utmctl >/dev/null 2>&1; then
    echo "[utm-gpu] установите UTM: brew install --cask utm" >&2
    exit 2
fi

# Нельзя изменять raw VaraniaFS, пока QEMU держит её открытой.
if VM_STATE="$(utmctl status "$VM_NAME" 2>/dev/null)" && [[ "$VM_STATE" != "stopped" ]]; then
    utmctl stop "$VM_NAME" --force >/dev/null
    for _ in {1..80}; do
        [[ "$(utmctl status "$VM_NAME" 2>/dev/null || true)" == "stopped" ]] && break
        sleep 0.25
    done
fi

if [[ "${RUSTOS_UTM_SKIP_BUILD:-0}" != "1" ]]; then
    make build-arm
fi
bash scripts/setup-utm-gpu.sh
utmctl start "$VM_NAME" >/dev/null

# `utmctl` обращается к публичному API UTM и запускает sandboxed QEMU helper
# через его штатного родителя. Внутренний QEMULauncher нельзя исполнять как
# обычный бинарник: macOS закономерно завершит его до старта QEMU.
for _ in {1..30}; do
    [[ "$(utmctl status "$VM_NAME" 2>/dev/null || true)" == "started" ]] && break
    sleep 0.2
done
if [[ "$(utmctl status "$VM_NAME" 2>/dev/null || true)" != "started" ]]; then
    echo "[utm-gpu] UTM не удержал VM в состоянии started" >&2
    echo "[utm-gpu] не запускайте Contents/XPCServices/.../QEMULauncher вручную" >&2
    exit 3
fi

osascript -e 'tell application "UTM" to activate' >/dev/null
echo "[utm-gpu] RustOS started; Aurora 3D is available from the desktop shortcut"

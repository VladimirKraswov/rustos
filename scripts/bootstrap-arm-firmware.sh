#!/usr/bin/env bash
# ARM UEFI bootstrap: готовит build/arm-firmware/ для QEMU `virt` + UEFI.
#
#   build/arm-firmware/edk2-aarch64-code.fd        — read-only UEFI-файрмварь (AAVMF)
#   build/arm-firmware/edk2-aarch64-vars-template.fd — NVRAM-шаблон (64 MiB, zeroed)
#
# Источник code-образа (pinned, в порядке приоритета):
#   1) firmware/aarch64/edk2-aarch64-code.fd.bz2 — закоммичен в репозиторий
#      (QEMU v11.1.0, сверка SHA-256 архива и распакованного образа);
#   2) homebrew-QEMU: /opt/homebrew/share/qemu/edk2-aarch64-code.fd (offline).
#
# NVRAM-шаблон создаётся zeroed (factory): EDK2 инициализирует store при
# первом старте — тот же приём, что в bootstrap-ovmf.sh (проверено на QEMU 11).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/build/arm-firmware"

VENDORED_BZ2="$ROOT/firmware/aarch64/edk2-aarch64-code.fd.bz2"
HB_CODE="/opt/homebrew/share/qemu/edk2-aarch64-code.fd"

# Pinned SHA-256 (см. firmware/aarch64/SHA256SUMS).
BZ2_SHA256="c023444108b7a132fdebf70c4765cd2dd9af2a9ff7d001a743aaabe87c20a458"
FD_SHA256="47765fe344818cbc464b1c14ae658fb4b854f5c2ceffa982411731eb4865594d"
ZERO_VARS_SHA256="3b6a07d0d404fab4e23b6d34bc6696a6a312dd92821332385e5af7c01c421351"

# AAVMF-раскладка: code и vars по 64 MiB (0x4000000).
FD_SIZE=67108864

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

if [[ -f "$DEST/edk2-aarch64-code.fd" && -f "$DEST/edk2-aarch64-vars-template.fd" ]]; then
    code_hash="$(sha256_of "$DEST/edk2-aarch64-code.fd")"
    vars_hash="$(sha256_of "$DEST/edk2-aarch64-vars-template.fd")"
    if [[ "$code_hash" == "$FD_SHA256" && "$vars_hash" == "$ZERO_VARS_SHA256" ]]; then
        echo "[arm-fw] already present: $DEST"
        exit 0
    fi
    echo "[arm-fw] existing code/vars SHA-256 mismatch, re-provisioning" >&2
    rm -f "$DEST/edk2-aarch64-code.fd" "$DEST/edk2-aarch64-vars-template.fd"
fi

mkdir -p "$DEST"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 1) Vendored bz2 из репозитория (офлайн, основной путь).
if [[ -f "$VENDORED_BZ2" ]]; then
    actual="$(sha256_of "$VENDORED_BZ2")"
    if [[ "$actual" != "$BZ2_SHA256" ]]; then
        echo "[arm-fw] ERROR: vendored bz2 SHA-256 mismatch: $actual" >&2
        exit 1
    fi
    bzip2 -dc "$VENDORED_BZ2" > "$TMP/edk2-aarch64-code.fd"
    actual="$(sha256_of "$TMP/edk2-aarch64-code.fd")"
    if [[ "$actual" != "$FD_SHA256" ]]; then
        echo "[arm-fw] ERROR: unpacked code SHA-256 mismatch: $actual" >&2
        exit 1
    fi
    cp "$TMP/edk2-aarch64-code.fd" "$DEST/edk2-aarch64-code.fd"
    echo "[arm-fw] OK: vendored AAVMF code (QEMU v11.1.0)"
else
    # 2) Offline fallback: homebrew-QEMU образ (тот же AAVMF, сверка по SHA-256).
    if [[ -f "$HB_CODE" ]]; then
        actual="$(sha256_of "$HB_CODE")"
        if [[ "$actual" != "$FD_SHA256" ]]; then
            echo "[arm-fw] ERROR: homebrew code SHA-256 mismatch: $actual" >&2
            exit 1
        fi
        cp "$HB_CODE" "$DEST/edk2-aarch64-code.fd"
        echo "[arm-fw] WARN: offline fallback — homebrew AAVMF code"
    else
        echo "[arm-fw] ERROR: нет firmware/aarch64/edk2-aarch64-code.fd.bz2 и нет $HB_CODE" >&2
        exit 1
    fi
fi

# NVRAM-шаблон: zeroed 64 MiB (factory reset; EDK2 заполнит при первом старте).
dd if=/dev/zero of="$DEST/edk2-aarch64-vars-template.fd" bs="$FD_SIZE" count=1 2>/dev/null
if [[ "$(wc -c < "$DEST/edk2-aarch64-vars-template.fd")" -ne "$FD_SIZE" ]]; then
    echo "[arm-fw] ERROR: vars template size mismatch" >&2
    exit 1
fi
if [[ "$(sha256_of "$DEST/edk2-aarch64-vars-template.fd")" != "$ZERO_VARS_SHA256" ]]; then
    echo "[arm-fw] ERROR: vars template is not a zeroed factory image" >&2
    exit 1
fi

echo "[arm-fw] OK: $DEST (code + vars-template)"

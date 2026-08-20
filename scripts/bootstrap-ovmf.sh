#!/usr/bin/env bash
# OVMF bootstrap: готовит build/ovmf/OVMF_CODE.fd + build/ovmf/OVMF_VARS.fd.
#
# Источник (pinned): Debian stable, пакет ovmf (source: edk2), plain-вариант БЕЗ
# Secure Boot (в factory VARS нет PK → secure-boot setup mode → неподписанный
# EFI-загрузчик стартует). Layout code(3653632) + vars(540672) = 4MiB — штатный
# QEMU OVMF (два pflash-драйва).
#
# Fallback (без сети, macOS): homebrew edk2-x86_64-code.fd + zeroed vars.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/build/ovmf"

OVMF_DEB_URL="https://deb.debian.org/debian/pool/main/e/edk2/ovmf_2025.02-8+deb13u1_all.deb"
OVMF_DEB_SHA256="78e0d54df11fc77406cb7a0bc9a39e5bca6d1cbe06556b91d9a73491c52decdf"
OVMF_CODE_SHA256="624e06de18b4fa535e90db7160d00d3d07d206422b89999bf1e27d920264e4e0"
OVMF_VARS_SHA256="5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e"
OVMF_VARS_SIZE=540672

if [[ -f "$DEST/OVMF_CODE.fd" && -f "$DEST/OVMF_VARS.fd" ]]; then
    echo "[ovmf] already present: $DEST"
    exit 0
fi

mkdir -p "$DEST"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if command -v curl >/dev/null 2>&1 && command -v ar >/dev/null 2>&1; then
    echo "[ovmf] downloading pinned Debian ovmf .deb"
    if ! curl -fSL --retry 2 --max-time 300 -o "$TMP/ovmf.deb" "$OVMF_DEB_URL"; then
        echo "[ovmf] download failed" >&2
    else
        actual="$(sha256_of "$TMP/ovmf.deb")"
        if [[ "$actual" != "$OVMF_DEB_SHA256" ]]; then
            echo "[ovmf] ERROR: .deb SHA256 mismatch: $actual" >&2
            exit 1
        fi
        ( cd "$TMP" && ar x ovmf.deb && tar -xf data.tar.xz )
        code="$TMP/usr/share/OVMF/OVMF_CODE_4M.fd"
        vars="$TMP/usr/share/OVMF/OVMF_VARS_4M.fd"
        [[ -f "$code" && -f "$vars" ]] || { echo "[ovmf] ERROR: fd files not found in .deb" >&2; exit 1; }
        if [[ "$(sha256_of "$code")" != "$OVMF_CODE_SHA256" ]]; then
            echo "[ovmf] ERROR: OVMF_CODE.fd SHA256 mismatch" >&2
            exit 1
        fi
        if [[ "$(sha256_of "$vars")" != "$OVMF_VARS_SHA256" ]]; then
            echo "[ovmf] ERROR: OVMF_VARS.fd SHA256 mismatch" >&2
            exit 1
        fi
        cp "$code" "$DEST/OVMF_CODE.fd"
        cp "$vars" "$DEST/OVMF_VARS.fd"
        echo "[ovmf] OK: Debian ovmf 2025.02-8+deb13u1 (plain, no Secure Boot)"
        exit 0
    fi
fi

# Fallback: local homebrew OVMF code + zeroed vars (zeroed store = factory,
# OVMF инициализирует его при первом старте; без PK → unsigned EFI OK).
HB_CODE="/opt/homebrew/share/qemu/edk2-x86_64-code.fd"
if [[ -f "$HB_CODE" ]]; then
    cp "$HB_CODE" "$DEST/OVMF_CODE.fd"
    dd if=/dev/zero of="$DEST/OVMF_VARS.fd" bs="$OVMF_VARS_SIZE" count=1 2>/dev/null
    echo "[ovmf] WARN: offline fallback — homebrew code + zeroed vars"
    exit 0
fi

echo "[ovmf] ERROR: no network and no local OVMF found ($HB_CODE)" >&2
exit 1

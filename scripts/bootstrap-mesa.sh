#!/usr/bin/env bash
# Загружает ровно тот upstream Mesa snapshot, на котором развивается порт.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="26.2.0"
SHA256="efd4bb08cdb7c365a812cd4e6c9202ab55b2f22cdcd13c7d6c4f9647b799a4ef"
ARCHIVE="$ROOT/build/downloads/mesa-$VERSION.tar.xz"
DESTINATION="$ROOT/build/third-party/mesa-$VERSION"
TEMP_DESTINATION="$ROOT/build/third-party/.mesa-$VERSION.tmp"
URL="https://archive.mesa3d.org/mesa-$VERSION.tar.xz"

mkdir -p "$(dirname "$ARCHIVE")" "$(dirname "$DESTINATION")"
if [[ ! -f "$ARCHIVE" ]]; then
    echo "[mesa] download $URL"
    curl --fail --location --retry 3 --output "$ARCHIVE" "$URL"
fi

if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$SHA256" "$ARCHIVE" | sha256sum --check -
else
    [[ "$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')" == "$SHA256" ]]
fi

if [[ -f "$DESTINATION/meson.build" ]]; then
    echo "[mesa] already prepared: $DESTINATION"
    exit 0
fi

# Обе цели находятся внутри build/third-party и заданы полными путями: очистка
# не может затронуть исходники или пользовательский каталог.
rm -rf "$TEMP_DESTINATION"
mkdir -p "$TEMP_DESTINATION"
tar -xJf "$ARCHIVE" --strip-components=1 -C "$TEMP_DESTINATION"
mv "$TEMP_DESTINATION" "$DESTINATION"
echo "[mesa] source ready: $DESTINATION (SHA-256 verified)"

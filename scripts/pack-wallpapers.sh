#!/usr/bin/env bash
# Пересобирает компактные CPU-friendly обои из PNG master assets.
# Скрипт одинаково работает на macOS и Linux; обычной сборке ОС он не нужен.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE="$ROOT/system-assets/assets/wallpapers/source"
PACKED="$ROOT/system-assets/assets/wallpapers/packed"

if ! command -v ffmpeg >/dev/null 2>&1; then
    echo "[wallpapers] ffmpeg is required only to regenerate packed assets" >&2
    exit 1
fi

mkdir -p "$PACKED"
for name in spring-river autumn-river winter-field; do
    echo "[wallpapers] $name.png -> $name.rgb565"
    ffmpeg -loglevel error -y \
        -i "$SOURCE/$name.png" \
        -vf "scale=640:360:flags=lanczos" \
        -pix_fmt rgb565le -f rawvideo "$PACKED/$name.rgb565"
    size="$(wc -c < "$PACKED/$name.rgb565" | tr -d ' ')"
    if [[ "$size" != "460800" ]]; then
        echo "[wallpapers] invalid packed size for $name: $size" >&2
        exit 1
    fi
done

echo "[wallpapers] OK: three 640x360 RGB565 assets"

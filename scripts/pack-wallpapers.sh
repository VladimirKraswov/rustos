#!/usr/bin/env bash
# Пересобирает широкоформатные runtime-обои из PNG master assets.
# Скрипт одинаково работает на macOS и Linux; обычной сборке ОС он не нужен.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE="$ROOT/system-assets/assets/wallpapers/source"
PACKED="$ROOT/system-assets/assets/wallpapers/packed"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/rustos-wallpapers.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

if ! command -v ffmpeg >/dev/null 2>&1; then
    echo "[wallpapers] ffmpeg is required only to regenerate packed assets" >&2
    exit 1
fi

mkdir -p "$PACKED"
for name in spring-river autumn-river winter-field; do
    echo "[wallpapers] $name.png -> $name.rbc1"
    ffmpeg -loglevel error -y \
        -i "$SOURCE/$name.png" \
        -vf "scale=1280:720:flags=lanczos" \
        -pix_fmt rgb24 -f rawvideo "$WORK/$name.rgb888"
    cargo run --quiet -p rustos-wallpaper-pack -- \
        1280 720 "$WORK/$name.rgb888" "$PACKED/$name.rbc1"
    size="$(wc -c < "$PACKED/$name.rbc1" | tr -d ' ')"
    if [[ "$size" != "460800" ]]; then
        echo "[wallpapers] invalid packed size for $name: $size" >&2
        exit 1
    fi
done

echo "[wallpapers] OK: three 1280x720 block-compressed assets"

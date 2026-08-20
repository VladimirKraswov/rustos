#!/usr/bin/env bash
# Создаёт build-only sysroot с портом RustOS std. Исходный rustup toolchain
# остаётся неизменным, поэтому параллельные host/UEFI сборки безопасны.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REAL_RUSTC="$(rustup which rustc)"
REAL_SYSROOT="$($REAL_RUSTC --print sysroot)"
COMMIT_HASH="$($REAL_RUSTC -Vv | awk '/^commit-hash:/ { print $2 }')"
[[ -n "$COMMIT_HASH" ]] || { echo "[std] cannot read rustc commit hash" >&2; exit 1; }

PORT_ROOT="$ROOT/ports/rust"
OVERLAY_ROOT="$ROOT/build/rustos-std-sysroot/$COMMIT_HASH"
OVERLAY_SYSROOT="$OVERLAY_ROOT/sysroot"
RUST_SOURCE="$OVERLAY_SYSROOT/lib/rustlib/src/rust"

mkdir -p "$OVERLAY_SYSROOT/lib/rustlib/src"

# dylib/rlib toolchain содержат сотни мегабайт и неизменяемы. Символические
# ссылки оставляют в build/ только 78 MiB исходников rust-src, которые патчим.
for entry in "$REAL_SYSROOT/lib/"*; do
    [[ "${entry##*/}" == "rustlib" ]] && continue
    [[ -e "$OVERLAY_SYSROOT/lib/${entry##*/}" ]] || \
        ln -s "$entry" "$OVERLAY_SYSROOT/lib/${entry##*/}"
done
mkdir -p "$OVERLAY_SYSROOT/lib/rustlib"
for entry in "$REAL_SYSROOT/lib/rustlib/"*; do
    [[ "${entry##*/}" == "src" ]] && continue
    [[ -e "$OVERLAY_SYSROOT/lib/rustlib/${entry##*/}" ]] || \
        ln -s "$entry" "$OVERLAY_SYSROOT/lib/rustlib/${entry##*/}"
done

if [[ ! -f "$RUST_SOURCE/library/std/Cargo.toml" ]]; then
    echo "[std] copy pinned rust-src into build overlay"
    cp -R "$REAL_SYSROOT/lib/rustlib/src/rust" "$RUST_SOURCE"
fi

PATCH_MARKER="$RUST_SOURCE/.rustos-pal-routing-v1"
if [[ ! -f "$PATCH_MARKER" ]]; then
    echo "[std] apply RustOS PAL routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0001-rustos-pal-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$PATCH_MARKER"
fi

FS_PATCH_MARKER="$RUST_SOURCE/.rustos-fs-routing-v1"
if [[ ! -f "$FS_PATCH_MARKER" ]]; then
    echo "[std] apply RustOS std::fs routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0002-rustos-fs-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$FS_PATCH_MARKER"
fi

STARTUP_PATCH_MARKER="$RUST_SOURCE/.rustos-startup-routing-v1"
if [[ ! -f "$STARTUP_PATCH_MARKER" ]]; then
    echo "[std] apply RustOS startup/env routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0003-rustos-startup-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$STARTUP_PATCH_MARKER"
fi

FS_STARTUP_PATCH_MARKER="$RUST_SOURCE/.rustos-fs-startup-hook-v1"
if [[ ! -f "$FS_STARTUP_PATCH_MARKER" ]]; then
    echo "[std] expose RustOS std::fs startup hook to PAL"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0004-rustos-fs-startup-hook.patch"
    printf '%s\n' "$COMMIT_HASH" > "$FS_STARTUP_PATCH_MARKER"
fi

THREAD_PATCH_MARKER="$RUST_SOURCE/.rustos-thread-routing-v1"
if [[ ! -f "$THREAD_PATCH_MARKER" ]]; then
    echo "[std] apply RustOS native thread routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0005-rustos-thread-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$THREAD_PATCH_MARKER"
fi

PIPE_PATCH_MARKER="$RUST_SOURCE/.rustos-pipe-routing-v1"
if [[ ! -f "$PIPE_PATCH_MARKER" ]]; then
    echo "[std] apply RustOS capability pipe routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0006-rustos-pipe-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$PIPE_PATCH_MARKER"
fi

STDIO_PATCH_MARKER="$RUST_SOURCE/.rustos-stdio-routing-v1"
if [[ ! -f "$STDIO_PATCH_MARKER" ]]; then
    echo "[std] apply RustOS stdio routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0007-rustos-stdio-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$STDIO_PATCH_MARKER"
fi

PROCESS_PATCH_MARKER="$RUST_SOURCE/.rustos-process-routing-v1"
if [[ ! -f "$PROCESS_PATCH_MARKER" ]]; then
    echo "[std] apply RustOS process routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0008-rustos-process-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$PROCESS_PATCH_MARKER"
fi

PATHS_PATCH_MARKER="$RUST_SOURCE/.rustos-paths-routing-v1"
if [[ ! -f "$PATHS_PATCH_MARKER" ]]; then
    echo "[std] apply RustOS process-local path routing patch"
    patch -d "$RUST_SOURCE" -p1 --forward \
        < "$PORT_ROOT/patches/0009-rustos-paths-routing.patch"
    printf '%s\n' "$COMMIT_HASH" > "$PATHS_PATCH_MARKER"
fi

# Обновляем только реально изменившиеся PAL-файлы. Обычный `cp -R` каждый раз
# менял mtime и заставлял Cargo заново собирать весь upstream std (несколько
# минут даже при неизменном port source).
while IFS= read -r -d '' source; do
    relative="${source#"$PORT_ROOT/std-overlay/library/"}"
    destination="$RUST_SOURCE/library/$relative"
    if [[ ! -f "$destination" ]] || ! cmp -s "$source" "$destination"; then
        mkdir -p "$(dirname "$destination")"
        cp "$source" "$destination"
    fi
done < <(find "$PORT_ROOT/std-overlay/library" -type f -print0)

printf '[std] overlay ready: %s\n' "$OVERLAY_SYSROOT"

#!/usr/bin/env bash
# Собирает upstream Rust std и RUNE smoke-программу для RustOS.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bash scripts/prepare-rustos-std.sh

REAL_RUSTC="$(rustup which rustc)"
COMMIT_HASH="$($REAL_RUSTC -Vv | awk '/^commit-hash:/ { print $2 }')"
export RUSTOS_REAL_RUSTC="$REAL_RUSTC"
RUSTOS_REAL_SYSROOT="$($REAL_RUSTC --print sysroot)"
export RUSTOS_REAL_SYSROOT
export RUSTOS_STD_SYSROOT="$ROOT/build/rustos-std-sysroot/$COMMIT_HASH/sysroot"
export RUSTC="$ROOT/scripts/rustc-rustos-std.sh"
# Content-addressed target dir — часть корректности порта, а не оптимизация.
# Изменённый PAL никогда не линкуется со старым libstd из Cargo cache.
CARGO_TARGET_DIR="$(bash scripts/rustos-std-target-dir.sh)"
export CARGO_TARGET_DIR
# RustOS не использует Unix crt1.o. `_start` находится в rustos-crt rlib;
# `-u` гарантирует извлечение startup object даже без явного вызова из main.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-u_start"

echo "[std] build core + alloc + std for requested RustOS target"
echo "[std] content-addressed target: $CARGO_TARGET_DIR"
cargo -Zjson-target-spec \
    -Zbuild-std=core,alloc,std,panic_abort \
    -Zbuild-std-features=compiler-builtins-mem \
    "$@"

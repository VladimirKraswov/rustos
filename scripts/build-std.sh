#!/usr/bin/env bash
# Собирает upstream Rust std и RUNE smoke-программу для RustOS.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bash scripts/prepare-rustos-std.sh

REAL_RUSTC="$(rustup which rustc)"
COMMIT_HASH="$($REAL_RUSTC -Vv | awk '/^commit-hash:/ { print $2 }')"
export RUSTOS_REAL_RUSTC="$REAL_RUSTC"
export RUSTOS_REAL_SYSROOT="$($REAL_RUSTC --print sysroot)"
export RUSTOS_STD_SYSROOT="$ROOT/build/rustos-std-sysroot/$COMMIT_HASH/sysroot"
export RUSTC="$ROOT/scripts/rustc-rustos-std.sh"

echo "[std] build core + alloc + std for x86_64-unknown-rustos"
cargo -Zjson-target-spec \
    -Zbuild-std=core,alloc,std,panic_abort \
    -Zbuild-std-features=compiler-builtins-mem \
    "$@"

#!/usr/bin/env bash
# Печатает content-addressed Cargo target dir для patched upstream std.
# Cargo не всегда включает внешний build-sysroot в fingerprint приложения;
# отдельный каталог по hash исключает незаметное повторное использование PAL.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REAL_RUSTC="$(rustup which rustc)"
COMMIT_HASH="$($REAL_RUSTC -Vv | awk '/^commit-hash:/ { print $2 }')"

PORT_HASH="$({
    find \
        "$ROOT/ports/rust/std-overlay" \
        "$ROOT/ports/rust/patches" \
        -type f -print
    printf '%s\n' \
        "$ROOT/scripts/prepare-rustos-std.sh" \
        "$ROOT/scripts/rustc-rustos-std.sh" \
        "$ROOT/targets/x86_64-unknown-rustos.json" \
        "$ROOT/targets/aarch64-unknown-rustos.json"
} | LC_ALL=C sort | while IFS= read -r file; do
    printf '%s ' "${file#"$ROOT/"}"
    git hash-object "$file"
done | git hash-object --stdin)"

printf '%s\n' "$ROOT/build/rustos-std-target/$COMMIT_HASH/$PORT_HASH"

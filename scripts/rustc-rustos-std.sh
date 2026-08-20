#!/usr/bin/env bash
# Cargo `-Zbuild-std` ищет library/ относительно `rustc --print sysroot`.
# Wrapper подменяет только этот запрос и добавляет overlay sysroot ко всем
# реальным компиляциям, не изменяя установленный rustup toolchain.
set -euo pipefail

: "${RUSTOS_REAL_RUSTC:?scripts/build-std.sh must set RUSTOS_REAL_RUSTC}"
: "${RUSTOS_REAL_SYSROOT:?scripts/build-std.sh must set RUSTOS_REAL_SYSROOT}"
: "${RUSTOS_STD_SYSROOT:?scripts/build-std.sh must set RUSTOS_STD_SYSROOT}"

if [[ "$*" == "--print sysroot" || "$*" == "--print=sysroot" ]]; then
    printf '%s\n' "$RUSTOS_STD_SYSROOT"
    exit 0
fi

# rustup proxy обычно добавляет toolchain dylib-каталог сам. Мы вызываем
# настоящий rustc напрямую, поэтому явно наследуем его для rust-lld/LLVM.
export DYLD_LIBRARY_PATH="$RUSTOS_REAL_SYSROOT/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
export LD_LIBRARY_PATH="$RUSTOS_REAL_SYSROOT/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$RUSTOS_REAL_RUSTC" --sysroot "$RUSTOS_STD_SYSROOT" "$@"

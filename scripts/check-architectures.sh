#!/usr/bin/env bash
# Не даёт переносимой части ядра и user software незаметно снова связаться с
# одной ISA. Проверяется полноценная компиляция каждого RustOS ELF target.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bash scripts/check-sdk-boundaries.sh

violations=""
while IFS= read -r source; do
    case "$source" in
        kernel/src/arch/*|runtime/src/arch.rs|boot/uefi/src/arch/*) ;;
        ports/rust/std-overlay/library/std/src/sys/pal/rustos/mod.rs) ;;
        *) violations+="${source}"$'\n' ;;
    esac
done < <(git grep -l -E '(^|[^[:alnum:]_])(global_)?asm!' -- '*.rs' || true)
if [[ -n "$violations" ]]; then
    echo "[arch-check] ISA-specific assembler outside arch boundary:" >&2
    printf '%s' "$violations" >&2
    exit 1
fi
echo "[arch-check] assembler boundary: OK"

for arch in x86_64 aarch64; do
    target="targets/${arch}-unknown-rustos.json"
    echo "[arch-check] ${arch}: runtime + bootstrap applications"
    # rune-runner и vfsd используют `alloc`, поэтому architecture
    # gate должен собирать тот же freestanding sysroot, что и build.sh.
    cargo -Zjson-target-spec -Zbuild-std=core,alloc build \
        -p rustos-runtime -p rustos-bootstrap-apps \
        --target "$target"

    echo "[arch-check] ${arch}: kernel"
    # Общий target является PIE для ring-3 программ. Kernel обеих ISA,
    # напротив, статически размещён bootstrap'ом и обязан проходить gate с теми
    # же linker flags, что scripts/build*.sh.
    linker_script="$ROOT/kernel/${arch}-grub.ld"
    [[ "$arch" == "aarch64" ]] && linker_script="$ROOT/kernel/aarch64-uefi.ld"
    kernel_rustflags="-C force-unwind-tables=no -C relocation-model=static -C link-arg=-no-pie -C link-arg=-T$linker_script"
    CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$kernel_rustflags" \
        cargo -Zjson-target-spec -Zbuild-std=core build \
        -p rustos-kernel --target "$target"
done

echo "[arch-check] OK: x86_64 + aarch64"

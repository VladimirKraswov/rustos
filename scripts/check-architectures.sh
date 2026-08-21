#!/usr/bin/env bash
# Не даёт переносимой части ядра и user software незаметно снова связаться с
# одной ISA. Проверяется полноценная компиляция каждого RustOS ELF target.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

violations=""
while IFS= read -r source; do
    case "$source" in
        kernel/src/arch/*|runtime/src/arch.rs|boot/uefi/src/arch/*) ;;
        *) violations+="${source}"$'\n' ;;
    esac
done < <(git grep -l -E '(^|[^[:alnum:]_])(global_)?asm!' -- \
    kernel/src runtime/src userspace/bootstrap/src boot/uefi/src || true)
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
    if [[ "$arch" == "x86_64" ]]; then
        # Общий target является PIE для ring-3 программ. GRUB kernel, напротив,
        # загружается по фиксированному физическому адресу и обязан проходить
        # architecture gate с теми же relocation/linker flags, что build.sh.
        KERNEL_RUSTFLAGS="-C force-unwind-tables=no -C relocation-model=static -C link-arg=-no-pie -C link-arg=-T$ROOT/kernel/x86_64-grub.ld"
        CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="$KERNEL_RUSTFLAGS" \
            cargo -Zjson-target-spec -Zbuild-std=core build \
            -p rustos-kernel --target "$target"
    else
        cargo -Zjson-target-spec -Zbuild-std=core build \
            -p rustos-kernel --target "$target"
    fi
done

echo "[arch-check] OK: x86_64 + aarch64"

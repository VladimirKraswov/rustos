#!/usr/bin/env bash
# Подбирает guest mode, который QEMU увеличит на целое число без интерполяции.
# Допускается letterbox: главное, чтобы ограничивающей осью оставался точный
# integer factor, а не дробное растяжение готового framebuffer.
set -euo pipefail

[[ $# == 4 ]] || {
    echo "usage: $0 HOST_WIDTH HOST_HEIGHT MAX_GUEST_WIDTH MAX_GUEST_HEIGHT" >&2
    exit 2
}

HOST_WIDTH="$1"
HOST_HEIGHT="$2"
MAX_GUEST_WIDTH="$3"
MAX_GUEST_HEIGHT="$4"
for value in "$HOST_WIDTH" "$HOST_HEIGHT" "$MAX_GUEST_WIDTH" "$MAX_GUEST_HEIGHT"; do
    [[ "$value" =~ ^[1-9][0-9]*$ ]] || {
        echo "display dimensions must be positive integers" >&2
        exit 2
    }
done

# Ниже 800×540 desktop уже теряет полезную площадь: крупные системные окна
# перестают помещаться. В таком случае launcher честно использует fallback.
MIN_GUEST_WIDTH=800
MIN_GUEST_HEIGHT=540

for SCALE in 2 3 4 5 6; do
    ((HOST_WIDTH % SCALE == 0 && HOST_HEIGHT % SCALE == 0)) || continue
    DESIRED_WIDTH=$((HOST_WIDTH / SCALE))
    DESIRED_HEIGHT=$((HOST_HEIGHT / SCALE))

    # Если обе оси больше предела, обрезка обеих снова дала бы дробный scale.
    ((DESIRED_WIDTH > MAX_GUEST_WIDTH && DESIRED_HEIGHT > MAX_GUEST_HEIGHT)) && continue

    GUEST_WIDTH="$DESIRED_WIDTH"
    GUEST_HEIGHT="$DESIRED_HEIGHT"
    ((GUEST_WIDTH > MAX_GUEST_WIDTH)) && GUEST_WIDTH="$MAX_GUEST_WIDTH"
    ((GUEST_HEIGHT > MAX_GUEST_HEIGHT)) && GUEST_HEIGHT="$MAX_GUEST_HEIGHT"
    ((GUEST_WIDTH >= MIN_GUEST_WIDTH && GUEST_HEIGHT >= MIN_GUEST_HEIGHT)) || continue

    FIT=fill
    if ((GUEST_WIDTH != DESIRED_WIDTH || GUEST_HEIGHT != DESIRED_HEIGHT)); then
        FIT=letterbox
    fi
    echo "$GUEST_WIDTH $GUEST_HEIGHT $SCALE $FIT"
    exit 0
done

echo "не найден integer mode для ${HOST_WIDTH}x${HOST_HEIGHT}" >&2
exit 3

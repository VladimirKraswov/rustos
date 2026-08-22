#!/usr/bin/env bash
# Не даёт обычному приложению обойти стабильный SDK и связаться с renderer,
# surface protocol или syscall ABI. Системные сервисы намеренно находятся вне
# sdk/examples и проверяются своими capability/lifecycle тестами.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

forbidden_packages='rustos-(abi|runtime|surface-client|ui-gpu|virgl|video|mesa)[[:space:]]*='
forbidden_imports='rustos_(abi|runtime|surface|ui_gpu|virgl|video|mesa)(::|[[:space:]])'
failed=0

while IFS= read -r manifest; do
    if rg -n "$forbidden_packages" "$manifest"; then
        echo "[sdk-boundary] запрещённая внутренняя зависимость: $manifest" >&2
        failed=1
    fi
done < <(find sdk/examples -name Cargo.toml -type f -print | sort)

while IFS= read -r source; do
    if rg -n "$forbidden_imports" "$source"; then
        echo "[sdk-boundary] запрещённый внутренний import: $source" >&2
        failed=1
    fi
done < <(find sdk/examples -name '*.rs' -type f -print | sort)

if (( failed != 0 )); then
    echo "[sdk-boundary] обычное приложение использует только std и публичные SDK facade" >&2
    exit 1
fi

echo "[sdk-boundary] OK: приложения изолированы от ABI/surface/renderer internals"

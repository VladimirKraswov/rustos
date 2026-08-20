# Правила `runtime/`

Runtime — низкоуровневый PAL для ring-3 RustOS, а не POSIX/Linux эмулятор.

- Syscall numbers и wire records берутся из `rustos-abi`, не дублируются
  локальными constants/structs.
- Safe API проверяет lengths, ownership и lifetime handle; raw syscall wrapper
  остаётся маленьким и документирует clobbers/register ABI.
- Process/thread-local errno, CWD, environment и TLS нельзя хранить глобально.
- Blocking API не busy-spin, если ABI предоставляет wait/futex/endpoint.
- `std`-совместимость реализует ожидаемую source semantics поверх capabilities;
  не вводи скрытый доступ к `/proc`, fork или Unix signals.
- AMD64/AArch64 детали сосредоточены в `src/arch.rs`; общий API одинаков.

Проверки: оба RustOS target через `make test-arch`; PAL/std behavior —
соответствующий ring-3 smoke и `make test-boot`.

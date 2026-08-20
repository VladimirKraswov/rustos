# Правила `abi/`

Наследуй корневой `AGENTS.md`. Этот crate — стабильная wire-граница между
kernel, сервисами, DLL facade и приложениями.

- Все публичные records имеют `#[repr(C)]`/`#[repr(transparent)]`, типы
  фиксированной ширины и документированную семантику каждого поля.
- Не передавай Rust enum layout, references, slices, trait objects, `usize` или
  process-local pointers. Buffer описывается handle/offset/length и ownership.
- Reserved поля при отправке равны нулю, при чтении проверяются. Добавление поля
  требует version/size negotiation; не переиспользуй старый tag с новым смыслом.
- Rights являются маской разрешений: проверяй неизвестные bits, attenuation и
  невозможность повышения прав получателем.
- Добавляй compile-time size/alignment assertions, encode/decode happy path,
  truncated/overflow/unknown-version негативные тесты.
- ABI crate не содержит service policy, allocator, syscall implementation или
  аппаратный код.

Проверки: `cargo test -p rustos-abi`, затем `make test-host` и `make test-arch`.

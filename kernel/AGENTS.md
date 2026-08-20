# Правила `kernel/`

Kernel должен оставаться механизмом микроядра, а не местом для новых
пользовательских функций. Прочитай `docs/MICROKERNEL.md` и документ подсистемы.

- ISA-specific code и asm находятся только в `src/arch`. Общие process, GUI,
  VFS и loader modules не ветвятся по register/page-table details.
- Доступ к hardware концентрируется в driver/platform adapter. App code не
  выполняет port I/O, MMIO и не получает physical address.
- Любой user pointer сначала range/permission/overflow check; kernel не
  разыменовывает его после смены address space.
- В hot path interrupt/syscall нет heap, unbounded loops, форматирования больших
  строк и ожидания user service без timeout/state transition.
- Fault, malformed IPC/RUNE/VFS input и исчерпание capacity возвращают ошибку
  или завершают только владельца. Kernel panic допустим только для нарушения
  внутреннего invariant, которое невозможно локализовать.
- Bootstrap apps в `src/apps` являются временными. Новый business logic лучше
  реализовать ring 3 service/app и оставить в kernel лишь typed bridge.
- Новая архитектурно независимая логика по возможности получает host unit test
  в отдельном crate, а не проверяется только загрузкой VM.

Проверки: freestanding x86 build, `make test-arch`; lifecycle/ABI —
`make test-boot`, видимый GUI/input — `make test-gui`.

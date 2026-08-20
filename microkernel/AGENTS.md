# Правила `microkernel/`

Это переносимое `no_std` ядро политик lifecycle, scheduler, IPC и capabilities.
Оно не знает регистров AMD64/AArch64, APIC/GIC, page-table format и framebuffer.

- Platform operations выражай узким trait/событием; реализация живёт в
  `kernel/src/arch`.
- PID/TID/capability ID остаются generation-safe. Освобождение обязано удалить
  очереди, waiters, mappings и переданные references ровно один раз.
- Scheduler не выполняет allocator или unbounded scan в interrupt hot path.
- Driver/service priority — явная policy с starvation test, не magic branch.
- IPC queue всегда bounded. Полная очередь блокирует/возвращает typed error, но
  не перезаписывает старое сообщение.
- Fault тест обязан доказать, что завершается только виновная задача и survivor
  продолжает исполнение.

Проверки: `cargo test -p rustos-microkernel`, затем `make test-host`; изменение
context/lifecycle/IPC дополнительно требует `make test-boot`.

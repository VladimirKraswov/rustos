# Self-hosting RustOS

Цель self-hosting достигнута не тогда, когда в образ скопирован файл `rustc`,
а когда запущенный внутри RustOS toolchain способен из исходников получить
новый kernel, системные DLL, сервисы и приложения, после чего этот образ
проходит boot-тест без участия компилятора macOS/Linux.

## Текущая граница реализации

Сейчас завершён runtime-фундамент между S1 и S2. Настоящая upstream
`core + alloc + std` собирается для RustOS; обычный `fn main` получает
argv/environment, process-local CWD и typed startup capabilities. Syscall ABI
v4 предоставляет spawn/wait/kill, native threads, blocking futex, anonymous и
shared VM, TLS, pipes, stdio и monotonic clock. Sparse page registry и slab
allocator больше не ограничивают процесс несколькими мегабайтами.

Изолированный ring-3 `vfsd` обслуживает streaming I/O, seek/readdir,
create/delete/rename и resize через capability IPC/shared memory. Публичные
`std::fs`, `std::thread` и `std::process` проходят QEMU stress. RUNE resolver
в ring 3 разрешает interface imports/exports и ABI ranges, строит TLS, RELRO и
sealed shared RX. `std::process::Command` прозрачно запускает приложение
непосредственно с VaraniaFS через `rune-runner`; нативный `rustos-rune` уже
является системной утилитой. Persistent image автоматически расширяется до
1 ГиБ без уничтожения пользовательских данных.

Это всё ещё не нативный compiler, и документация намеренно не называет этап
self-hosted. Четыре оставшихся архитектурных барьера:

1. развернуть Rust/Cargo source и vendor store в масштабируемой VaraniaFS и
   добавить воспроизводимый package cache;
2. user-space `init`/supervisor и console service. После boot milestones уже
   остаётся persistent `vfsd`, а GUI-команда `RUN` запускает ring-3 RUNE и
   захватывает pipe; однако orchestration/terminal всё ещё живут в kernel;
3. RustOS-host `rust-lld`, dynamic loading proc-macro/codegen DLL и затем
   cross-built seed `rustc_driver`;
4. offline Cargo + source/vendor bundle и воспроизводимый native rebuild test.

Копирование host `rustc` в образ до этих пунктов запрещено критерием проекта:
такой файл либо не запустится, либо незаметно продолжит зависеть от macOS/Linux.

## Три разные платформы Rust bootstrap

В терминологии Rust нельзя смешивать:

- **build** — машина, на которой запускается `x.py` первого bootstrap;
- **host** — машина, на которой должен запускаться собранный `rustc`;
- **target** — машина, для которой `rustc` генерирует программу.

Текущий JSON target позволяет macOS/Linux генерировать ELF64 PIE intermediate,
который `rustos-rune` превращает в нативный container. Для compiler нужен
гораздо более сложный host
`x86_64-unknown-rustos`: собранные `rustc`, `cargo`, build scripts и proc
macros должны запускаться внутри RustOS. Значит до порта compiler обязательны
полноценные `std`, процессы, потоки, TLS, VFS, часы, environment, pipes,
виртуальная память и dynamic loader.

## Bootstrap без циклического обещания

Полностью удалить начальное доверенное звено невозможно: Rust сам собирается
предыдущей версией Rust. RustOS хранит его явно и воспроизводимо.

1. **S0 cross** — зафиксированный nightly macOS/Linux собирает kernel и
   минимальный user runtime.
2. **S1 target std** — cross-сборка `core`, `alloc`, `std` и SDK для
   `x86_64-unknown-rustos`.
3. **S2 native seed** — снаружи собираются `rustc`, `cargo`, linker и быстрый
   codegen backend, которые уже запускаются как RustOS host tools.
4. **S3 native rebuild** — S2 внутри VM собирает те же версии toolchain и
   всей RustOS из исходников на VaraniaFS.
5. **S4 reproducibility** — S3 повторяет сборку; нормализованные RUNE и disk
   image совпадают по digest. После этого штатный `make build` только просит
   RustOS build VM выполнить сборку, а host остаётся транспортом и QEMU.

Seed лежит в versioned toolchain bundle с SHA-256, исходниками, лицензиями и
точным bootstrap manifest. Обновление Rust — отдельный проверяемый change, а
не скрытое скачивание «latest».

## Что портируется в Rust

Порядок реализации host support:

1. встроенный target spec и cfg `target_os = "rustos"`;
2. `library/std/src/sys/pal/rustos`: files/CWD, args/env, time, allocator,
   thread/futex, process, pipe и stdio уже работают; остаются unwind/TLS
   destructors, sockets и host dynamic loading;
3. crates `libc`/`cc` и RustOS SDK headers/import libraries;
4. native `rust-lld`, `rustos-rune` и loader RUNE applications/DLL;
5. `rustc_driver`, `rustdoc`, proc-macro server;
6. Cargo с локальным registry/vendor store, без обязательной сети;
7. bootstrap tools `rustos-pack`, `rustos-image` и test runner как native
   программы;
8. сборка kernel, DLL, services и applications внутри системы.

Первый backend — тот, который позволяет корректно пройти compiler tests.
LLVM остаётся эталонным backend для release. Cranelift-профиль можно поставить
рядом как быстрый developer backend: он уменьшает задержку edit-build-run, но
не заменяет проверку release-кода LLVM.

## Размещение toolchain

```text
/system/bin/rustc
/system/bin/cargo
/system/bin/rustdoc
/system/bin/rust-lld
/system/lib/loader-1.rune
/system/lib/system-1.rune
/system/lib/vfs-1.rune
/system/rustlib/x86_64-unknown-rustos/lib/*.rlib
/system/rustlib/codegen-backends/*.dll
/system/src/rust/                 # опциональный source bundle
/var/cache/cargo/                 # registry/git cache
/src/rustos/                      # исходники самой ОС
/build/rustos/                    # отдельный build tree
```

`/system` меняется только package/update transaction. Исходники и build
artifacts никогда не смешиваются с установленными DLL.

## Память и диск

Обычная RustOS продолжит загружаться в 128 MiB. Это не означает, что полный
Rust compiler обязан помещаться в 128 MiB: frontend, linker и особенно LLVM
используют существенно больше памяти.

- runtime/обычные приложения: 128 MiB RAM, диск от 1 GiB;
- компиляция небольших программ быстрым backend: практический профиль от
  2 GiB RAM и 8 GiB диска;
- пересборка Rust toolchain и LLVM: developer VM от 8 GiB RAM и 32 GiB диска.

Ограничения задаются профилями образа, а адреса/offsets везде остаются `u64`.

## Критерий готовности

Self-hosting test выполняется одной командой внутри RustOS:

```text
build-system --clean --source /src/rustos --output /build/rustos
test-system /build/rustos/rustos.img
```

Тест обязан скомпилировать DLL и terminal отдельно, пересобрать kernel и ESP,
запустить вложенную QEMU либо передать image host harness'у, получить
`GUI_READY` и сравнить manifest/digest. До этого момента документация должна
называть сборку cross-hosted, а не self-hosted.

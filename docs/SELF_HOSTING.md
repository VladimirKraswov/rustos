# Self-hosting RustOS

Цель self-hosting достигнута не тогда, когда в образ скопирован файл `rustc`,
а когда запущенный внутри RustOS toolchain способен из исходников получить
новый kernel, системные DLL, сервисы и приложения, после чего этот образ
проходит boot-тест без участия компилятора macOS/Linux.

## Текущая граница реализации

Сейчас репозиторий находится на S0: target kernel cross-компилируется на
macOS/Linux. Обязательный процессный checkpoint уже пройден: отдельный ELF64
PIE запускается в CPL3 с process-local VFS capability, выполняет syscall и
завершается; fault второго ELF не останавливает kernel/GUI, а все кадры его
address space освобождаются. Это исполняемый фундамент, но ещё не нативный
compiler: host `std`, dynamic loader, persistent filesystem, полноценные
потоки и process spawning пока отсутствуют.

Следующая прямая зависимость self-hosting — вытесняющий scheduler и IPC/VFS
services. Только поверх них имеет смысл портировать `std::thread`,
`std::process`, файлы, pipes и dynamic loading, а затем native seed toolchain.

## Три разные платформы Rust bootstrap

В терминологии Rust нельзя смешивать:

- **build** — машина, на которой запускается `x.py` первого bootstrap;
- **host** — машина, на которой должен запускаться собранный `rustc`;
- **target** — машина, для которой `rustc` генерирует программу.

Текущий JSON target уже позволяет macOS/Linux генерировать freestanding ELF
ядра. Для native compiler нужен гораздо более сложный host
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
5. **S4 reproducibility** — S3 повторяет сборку; нормализованные ELF и disk
   image совпадают по digest. После этого штатный `make build` только просит
   RustOS build VM выполнить сборку, а host остаётся транспортом и QEMU.

Seed лежит в versioned toolchain bundle с SHA-256, исходниками, лицензиями и
точным bootstrap manifest. Обновление Rust — отдельный проверяемый change, а
не скрытое скачивание «latest».

## Что портируется в Rust

Порядок реализации host support:

1. встроенный target spec и cfg `target_os = "rustos"`;
2. `library/std/src/sys/pal/rustos`: файлы, сеть-заглушка, args/env, time,
   thread, mutex/condvar, process, pipe, dynamic loading;
3. crates `libc`/`cc` и RustOS SDK headers/import libraries;
4. native linker `rust-lld` и loader для ELF64 PIE/DLL;
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
/system/lib/loader-1.dll
/system/lib/system-1.dll
/system/lib/vfs-1.dll
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

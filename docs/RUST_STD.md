# Порт Rust `std` на RustOS

Репозиторий собирает настоящую upstream standard library для
`x86_64-unknown-rustos`. Это не локальный facade с именем `std`: Cargo
использует `-Zbuild-std=core,alloc,std,panic_abort`, после чего ring-3 программа
упаковывается в RUNE и запускается обычным process manager.

## Воспроизводимая сборка

Исходники привязаны к nightly из `rust-toolchain.toml`. Установленный rustup
toolchain никогда не патчится на месте. Скрипт
[`prepare-rustos-std.sh`](../scripts/prepare-rustos-std.sh) создаёт build-only
sysroot, копирует pinned `rust-src`, применяет небольшие routing patches и
накладывает RustOS PAL из [`ports/rust/std-overlay`](../ports/rust/std-overlay).

```bash
# Собрать std и конкретное приложение
bash scripts/build-std.sh build \
  -p rustos-bootstrap-apps --bin rustos-std-smoke --features std-port \
  --target targets/x86_64-unknown-rustos.json

# Полный образ и QEMU test
RUSTOS_BOOT_TEST=1 bash scripts/build.sh
bash scripts/test-boot.sh
```

Wrapper `rustc-rustos-std.sh` подменяет ответ `rustc --print sysroot` только
для этой Cargo-сборки. Host tools, UEFI loader и обычный rustup sysroot не
смешиваются с port overlay.

## Что уже работает

| Область | Реализация |
|---|---|
| `core`, `alloc`, `std` | собираются upstream Cargo graph |
| `std::alloc::System` | page-granular `vm_map/vm_unmap`, W^X policy kernel |
| collections/string | `Vec`, `String`, `BTreeMap`, `HashMap` проверены ring 3 |
| TLS | RUNE TLS template и AMD64 FS thread pointer |
| time | монотонные nanoseconds, `Instant`; boot-epoch `SystemTime` |
| synchronization | blocking futex wait/wake; contended Mutex/Barrier проверены |
| threads | native create/join/detach, отдельные stack и TLS image |
| process | spawn/wait/try_wait/kill, argv/env/CWD, capability inheritance |
| pipes/stdio | blocking streams; одновременный drain stdout/stderr без deadlock |
| `std::fs` | capability IPC к отдельному `vfsd`, 64-КиБ shared I/O window |
| execution | `Command` запускает RUNE с VaraniaFS через ring-3 runner |

`std::fs` поддерживает `File/OpenOptions`, read/write, vectored fallback,
seek/tell, flush/sync, metadata, create/remove directory, readdir, rename,
exists, copy, recursive remove, canonicalize, process-local CWD и `set_len`
со sparse grow/shrink. Permissions намеренно не являются security boundary: RustOS
авторизует доступ capabilities, поэтому `set_permissions` совместимо
принимается, но не создаёт Unix uid/mode policy. Symlinks, file timestamps и
file locks пока возвращают `Unsupported`.

Boot smoke проверяет эти операции через публичные `std::fs`/`std::io`, затем
создаёт конкурирующие threads и дочерние процессы, которые одновременно
заполняют stdout/stderr сильнее размера pipe. Нативный `rustos-rune` также
запускается как системная программа и проверяет DLL с диска. Serial marker:

```text
[std] allocator fs threads futex process pipes stdio native SDK and VFS executable verified in ring3 RUNE
```

## Почему `std::fs` не является драйвером

```text
application using std::fs
        |
        | normal Rust calls
        v
RustOS std PAL       path validation, OpenOptions, shared-window client
        |
        | capability IPC
        v
vfsd.rune            namespace, open descriptions, VaraniaFS
        |
        | block capability
        v
block transport
```

Приложение не получает block capability, а `std` не содержит disk parser.
Ошибка приложения или PAL не даёт прямого доступа к устройству; ошибка
`vfsd` не останавливает kernel. Большие данные не проходят через 64-байтный
IPC payload: передаются только handle, offset и length shared window.

## Граница текущего порта

Process/thread/memory/VFS/RUNE prerequisites для первого seed compiler уже
исполняются, но это ещё не готовый native `rustc`. Остаются host-tool части:

1. TLS destructors и выбранная политика unwind (`panic=abort` допустим для
   seed, но не должен молча считаться полным портом);
2. dynamic loading proc-macro server и codegen backend через RUNE DLL;
3. native `rust-lld`, затем cross-built RustOS-host `rustc_driver`;
4. offline Cargo vendor/index и запуск build scripts/proc macros;
5. масштабируемая VaraniaFS v2: v1 с 64 inode не вмещает исходники toolchain;
6. user-space supervisor и console IPC: GUI `RUN` уже создаёт настоящий
   ring-3 процесс и захватывает pipe, но terminal orchestration пока остаётся
   bootstrap-кодом ядра.

HashMap seed сейчас годится для защиты от обычных collision patterns, но не
является криптографическим RNG. `SystemTime` использует эпоху загрузки, пока
нет RTC/time service. Sockets/DNS не нужны обязательному offline профилю, но
понадобятся сетевому Cargo. Эти ограничения не маскируются под готовый порт.

## Рекомендации для переноса Linux/Rust ПО

- сначала собирать с `panic=abort` и без network features;
- использовать `std::fs`, `std::io`, `std::time`, а не прямые libc syscalls;
- platform-specific код держать в `cfg(target_os = "rustos")` PAL;
- экспорт DLL фиксировать через `extern "C"` и `#[repr(C)]`;
- не полагаться на `fork`, Unix signals, `/proc`, file modes и symlinks;
- передавать доступ к сервисам в process manifest, а не открывать глобальные
  устройства по пути.

Так большая часть platform-neutral crates переносится перекомпиляцией, а не
переписыванием под новый бинарный формат.

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
| synchronization | upstream futex algorithms; fast path `Mutex` проверен |
| `std::fs` | capability IPC к отдельному `vfsd`, 64-КиБ shared I/O window |

`std::fs` поддерживает `File/OpenOptions`, read/write, vectored fallback,
seek/tell, flush/sync, metadata, create/remove directory, readdir, rename,
exists и copy. Permissions намеренно не являются security boundary: RustOS
авторизует доступ capabilities, поэтому `set_permissions` совместимо
принимается, но не создаёт Unix uid/mode policy. Symlinks, file timestamps,
file locks и arbitrary truncate пока возвращают `Unsupported`.

Boot smoke создаёт persistent VaraniaFS-каталог, пишет и читает файл через
публичные `std::fs`/`std::io`, проверяет seek/metadata/readdir/rename, удаляет
его, синхронизирует и завершает `vfsd`. Ожидаемый serial marker:

```text
[std] collections allocator sync time and std::fs over vfsd verified in ring3 RUNE
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

Порт достаточен для collections, allocator, clocks и базового filesystem
кода, но ещё не для native `rustc`. Следующие обязательные части:

1. настоящий `std::thread::spawn/join` и blocking futex wait/wake вместо
   cooperative polling;
2. startup runtime с `argc/argv`, environment и TLS destructors;
3. `std::process`, pipes и stdio handles;
4. RUNE dynamic loader и загрузка RustOS DLL по interface ID;
5. sockets/DNS для Cargo либо полностью offline vendor workflow;
6. unwinding или документированная host-tool policy `panic=abort`;
7. allocator с sub-page arenas и alignment больше 4096.

HashMap seed сейчас годится для защиты от обычных collision patterns, но не
является криптографическим RNG. `SystemTime` использует эпоху загрузки, пока
нет RTC/time service. Эти ограничения явно не маскируются под готовую POSIX
семантику.

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

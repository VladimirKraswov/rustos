# RUNE: нативный формат программ RustOS

RUNE (`RustOS Universal Native Envelope`) — собственный контейнер приложений,
сервисов, драйверов и динамических библиотек RustOS. Расширение файла —
`.rune`; роль артефакта задаётся флагом `APPLICATION`, `LIBRARY`, `SERVICE`
или `DRIVER`, а не расширением и не каталогом.

Цель RUNE — сделать привилегированный loader маленьким и проверяемым, явно
описать capability-зависимости микроядерной программы и не потерять удобство
переноса Unix/Rust-кода. Формат не обещает бинарную совместимость с Linux:
Linux ELF нельзя просто скопировать и запустить. Он обещает короткий путь
перекомпиляции исходников через привычный compiler frontend.

## Почему не ELF или PE

ELF хорошо решает задачу Unix toolchain, но несёт несколько десятилетий
опциональных таблиц, platform conventions и неоднозначностей loader policy.
PE связывает формат с моделью Windows image/import table. RUNE оставляет
только то, что требуется RustOS:

- regions, relocations, TLS и RELRO представлены отдельными bounded records;
- capability requirements известны supervisor до запуска кода;
- библиотека выбирается по 128-битному ID интерфейса и диапазону ABI, а не по
  случайно найденному имени файла;
- один container может иметь AMD64 и AArch64 slices;
- read-only/RX regions помечаются как shareable и могут использовать одни
  физические кадры в разных процессах;
- hash охватывает весь container, а signature record можно проверять до map;
- parser работает без allocator и проверяет все ranges до первого изменения
  address space.

Это сокращает код trusted loader. Формат остаётся учебным: заголовок и все
элементы table имеют фиксированный размер, little-endian числа и читаемые
Rust-структуры в [`rune-format`](../rune-format/src/lib.rs).

## Как сохраняется переносимость Linux-программ

RUNE не требует форка `rustc`, LLVM или Clang:

```text
Rust/C source
    -> rustc/clang + lld: ELF64 PIE (только build intermediate)
    -> rustos-rune: normalize + validate + pack
    -> program.rune
    -> RUNE loader: verify + map + relocate + enter ring 3
```

ELF находится только на границе toolchain. `rustos-rune` переносит его
семантику в строгие RUNE records, отбрасывая ненужные sections. Для source
portability RustOS предоставляет привычные `std::fs`, `Read/Write/Seek`,
threads, clocks и process API, но реализует их capability IPC, а не Linux
syscalls. Для C ABI планируются небольшой POSIX source-compatibility facade и
SDK headers. Linux-specific `ioctl`, `/proc`, signals и fork semantics требуют
явного platform adapter.

## Общая структура v1

Все offsets абсолютны от начала файла, все RVA — от выбранной loader base.

### Header: 128 байт

| Offset | Size | Поле |
|---:|---:|---|
| 0 | 8 | magic `RUNE\r\n\x1a\n` |
| 8 | 2 | format version, сейчас `1` |
| 10 | 2 | header size, `128` |
| 12 | 4 | file flags |
| 16 | 8 | полный размер файла |
| 24 | 8 | offset таблицы records |
| 32 | 4 | количество records |
| 36 | 4 | размер TOC entry, `64` |
| 40 | 4 | индекс string table или `u32::MAX` |
| 44 | 4 | индекс manifest или `u32::MAX` |
| 48 | 16 | package ID |
| 64 | 16 | reproducible build ID |
| 80 | 32 | SHA-256 container; само поле считается нулевым |
| 112 | 16 | reserved, обязаны быть нулями |

`package_id` обозначает логический пакет, `build_id` — конкретную сборку.
Обновление реализации с сохранением ABI меняет build ID, но не interface ID.

### TOC entry: 64 байта

| Offset | Size | Поле |
|---:|---:|---|
| 0 | 2 | record kind |
| 2 | 2 | architecture (`ANY`, `X86_64`, `AARCH64`) |
| 4 | 4 | flags |
| 8 | 8 | payload offset |
| 16 | 8 | payload/file size |
| 24 | 8 | virtual address/RVA |
| 32 | 8 | memory size |
| 40 | 8 | alignment |
| 48 | 4 | name offset в string table |
| 52 | 2 | name length, без NUL |
| 54 | 2 | record ABI version |
| 56 | 4 | link на связанный record |
| 60 | 4 | reserved |

Неизвестный optional record можно пропустить по размеру. Неизвестный record,
помеченный будущим флагом `REQUIRED`, обязан давать отказ загрузки.

## Records

| Kind | Назначение |
|---|---|
| `SLICE` | entry RVA и корень records одной ISA |
| `REGION` | file-backed code, rodata, data или zero-filled tail |
| `RELOCATIONS` | нормализованные RUNE relocations |
| `IMPORTS` / `EXPORTS` | symbols стабильных interface ABI |
| `DEPENDENCIES` | допустимые providers и диапазоны ABI |
| `TLS` | template, размер и alignment thread-local блока |
| `RELRO` | диапазон, закрываемый для записи после relocation |
| `CAPABILITIES` | запрашиваемые сервисы и минимальные rights |
| `STRINGS` | UTF-8 diagnostic names |
| `DEBUG` | отделяемая debug metadata |
| `SIGNATURE` | подпись hash + package policy |

`REGION` никогда не может быть одновременно writable и executable. Loader
сначала создаёт private writable staging только там, где нужна relocation,
затем устанавливает конечные права и RELRO. RX/RO страницы sealed library
после этого допускают физическое разделение между процессами.

## Динамические библиотеки и ABI

DLL в терминологии RustOS — RUNE container с флагом `LIBRARY`. Её имя файла
не является ABI. Связь строится из четырёх сущностей:

1. `InterfaceId` — первые 128 бит SHA-256 с domain separation от канонического
   имени, например `org.rustos.vfs/1`;
2. диапазон совместимых ABI dependency: `minimum_abi..=maximum_abi`;
3. `SymbolId` — 128-битный hash interface ID и канонической C signature;
4. diagnostic UTF-8 name, который нужен инструментам и проверке collision,
   но не участвует в поиске по каталогу.

Rust ABI не экспортируется: между версиями compiler он нестабилен. Публичная
граница использует `extern "C"`, `#[repr(C)]`, integer фиксированной ширины и
явное владение buffers/handles. Безопасный Rust crate остаётся тонкой
обёрткой поверх этого ABI.

В v1 зарезервированы фиксированные wire records:

- `Import` — 48 байт: interface, symbol, ABI range, flags, diagnostic name;
- `Export` — 56 байт: interface, symbol, RVA, ABI version, flags, name;
- `Dependency` — 48 байт: interface, optional package ID, ABI range, policy;
- `CapabilityRequest` — 32 байта: service interface, rights, ABI, slot hint.

Relocation ссылается на import по индексу. Resolver выбирает provider из
подписанного package graph, проверяет ABI и тип symbol, eagerly применяет
relocations, закрывает RELRO и только затем запускает код. В hot path вызов
DLL — обычный непрямой вызов без IPC. IPC используется самой DLL только если
она является client stub системного сервиса, например `vfs.dll -> vfsd`.

## Capability manifest

Manifest не выдаёт права сам. Он описывает минимальные требования программы:
service interface, ABI, rights и optional/required policy. Supervisor
сопоставляет запрос со своей policy, создаёт только разрешённые производные
capabilities и передаёт их через process startup table. Поэтому подмена RUNE
файла не может самостоятельно получить block device или admin endpoint.

## Проверка loader'ом

До map первой страницы loader обязан проверить:

1. magic/version/header sizes/reserved bytes;
2. `file_size`, TOC multiplication и каждый `offset + size` без overflow;
3. architecture slice и отсутствие пересекающихся virtual regions;
4. power-of-two alignment, page limits и W^X;
5. SHA-256, затем signature/package policy при её наличии;
6. relocation targets, import indices, TLS и RELRO ranges;
7. capability manifest против supervisor policy.

Любая ошибка завершает только создаваемый процесс и освобождает уже
зарезервированные frames. Она не должна panic'овать kernel.

## Инструменты

```bash
# ELF64 PIE -> RUNE
cargo run -p rustos-rune -- input.elf output.rune

# DLL с декларативным interface ABI
cargo run -p rustos-rune -- pack-manifest \
  input.dll output.rune sdk/abi/my-library.rune-abi

# Структурная и hash-проверка
cargo run -p rustos-rune -- verify output.rune

# Читаемый список records
cargo run -p rustos-rune -- inspect output.rune
```

Build автоматически конвертирует все запускаемые system programs и кладёт в
initramfs только `.rune`. Kernel остаётся фиксированным ELF64: его читает GRUB
до запуска микроядра, и это отдельная trust boundary.

## Статус реализации

Работают и проверяются QEMU boot-test'ом:

- no-alloc parser + SHA-256;
- ELF64 PIE conversion для AMD64/AArch64;
- application/library regions, normalized relative/import/PC32/TLS relocations;
- kernel dispatch по magic, W^X и frame cleanup;
- manifest parser проверяет declared imports/exports против ELF `.dynsym`;
- ring-3 resolver находит provider по interface ID/ABI range, применяет
  relocations, строит combined TLS, закрывает RELRO и разделяет sealed RX;
- `rune-runner` читает application и dependency closure непосредственно из
  VaraniaFS, затем передаёт target argv/env/capabilities и новый stack;
- все system ring-3 программы в `.rune`, включая upstream Rust `std` и сам
  нативный `rustos-rune` verifier.

ELF остаётся build intermediate и migration fallback kernel loader'а, но в
штатный system image пользовательские ELF/DLL больше не устанавливаются.
Большая read-only region, не помещающаяся в ранний лимит shared-memory object,
получает private RX mapping с сохранением W^X; после масштабирования shared
objects этот fallback можно сделать разделяемым без смены формата.

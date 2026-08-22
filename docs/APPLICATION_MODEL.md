# Модель приложений, DLL и запуска RustOS

Этот документ фиксирует долгоживущую границу между системой и прикладным
кодом. Текущие bootstrap-приложения разрешено переписать при переходе в ring 3,
но новые приложения после этого не должны зависеть от способа рендеринга,
драйвера GPU, протокола VFS, номеров syscall или внутреннего устройства ядра.

## Неподвижные правила

1. Запускаемый артефакт RustOS — проверяемый `.rune`, а ELF64 остаётся только
   промежуточным форматом rustc/lld и форматом kernel для firmware/GRUB.
2. Обычное приложение видит `std` и типизированные системные DLL. Оно не видит
   syscall numbers, IPC wire records, framebuffer, MMIO, `GraphicsBuffer` или
   конкретный GPU backend.
3. Системные функции подключаются по `InterfaceId` и диапазону ABI, а не по
   имени файла и не через глобальный поиск символов.
4. Внешняя ABI-граница не использует Rust ABI. Raw слой — `extern "C"`,
   fixed-width integers, opaque handles и явные buffer ownership rules.
   Удобный Rust API генерируется поверх него.
5. Manifest описывает потребности, но не выдаёт права. Supervisor передаёт
   только разрешённые capabilities при создании процесса.
6. CPU/GPU, Mesa/VirGL/native driver и software fallback являются реализацией
   системного graphics provider. Один и тот же RUNE приложения работает с
   каждым backend.
7. Crash, зависание или повреждённый RUNE завершают только экземпляр процесса.
   Loader и supervisor освобождают address space, threads, handles и mappings.

## Четыре разные сущности

| Сущность | Что содержит | Кто владеет состоянием |
|---|---|---|
| RUNE application | entry point, private code/data/TLS, imports, metadata, icons и resources | отдельный process instance |
| RUNE DLL | sealed RX/RO code, process-local data/TLS, exports и interface schema | каждый подключивший процесс |
| RUNE service | protocol implementation и mutable shared state | изолированный supervisor process |
| RUNE driver | device protocol и минимальные device capabilities | изолированный driver process |

DLL нужна для быстрого локального вызова и удобного API. Service нужен для
изоляции привилегий и общего mutable state. Например, `vfs.dll` проверяет
аргументы и предоставляет `File`, но filesystem cache, namespace и block
device остаются в `vfsd`:

```text
application -> safe Rust facade -> RUNE DLL stub -> capability IPC -> vfsd
                    normal call              shared memory for bulk I/O
```

## `.rune` как нормальный запускаемый файл

Фиксированный 128-байтный header остаётся маленьким. Расширение выполняется
через bounded TOC records. Благодаря этому loader может пропустить неизвестный
optional record и обязан отклонить неизвестный `REQUIRED` record.

Корневой `MANIFEST` record содержит:

- роль артефакта и lifecycle;
- допустимый диапазон startup/runtime ABI;
- semantic version приложения;
- ссылки на metadata и default icon;
- restart/background policy.

Дополнительные records:

- `METADATA` — UTF-8 display name, summary, vendor, category, homepage и
  namespaced custom fields; локализованные строки используют BCP 47 locale;
- `ICON` — несколько размеров, scale factors, light/dark/high-contrast themes
  и назначений; поддержаны premultiplied RGBA8, PNG и UTF-8 SVG;
- `RESOURCE` — именованные immutable assets с content type и явным encoding;
- `INTERFACE_SCHEMA` — встроенная каноническая схема публичного DLL API;
- `SDK_BINDINGS` — необязательные производные Rust/C bindings и документация;
- `SLICE/REGION/TLS/RELRO` — машинный код и память для AMD64/AArch64;
- `IMPORTS/EXPORTS/DEPENDENCIES/CAPABILITIES` — link и capability graph;
- `DEBUG` и `SIGNATURE` — отделяемая диагностика и проверка publisher policy.

Иконка не имеет одного «favicon-размера». Shell выбирает лучший вариант по
назначению, физическому размеру, device scale и теме, затем декодирует его один
раз в системный atlas. Код приложения для показа иконки не запускается.

Ресурс имеет каноническое относительное имя, например `ui/main.rui` или
`textures/brick-normal.ktx2`. Пути с `/`, пустым компонентом или `..` packer
отклоняет. Payload входит в общий SHA-256 container и не может быть незаметно
подменён.

### Что означает «самодостаточный»

Один RUNE содержит весь собственный код, данные, TLS, metadata, icons и assets
простого приложения. Он не копирует внутрь SystemUI, VFS, Mesa и другие
системные реализации: они разрешаются как системные interfaces. Private код
можно статически включить либо поставить как hash-pinned private dependency в
том же атомарном package closure. Package store дедуплицирует одинаковые
объекты, поэтому сто программ не создают сто копий одной DLL на диске.

Файл остаётся запускаемым независимо от каталога. Расширение `.rune` удобно
пользователю, но launcher определяет тип по magic, file flags и manifest. В
RustOS нет Unix execute bit: право запуска даёт capability на namespace, а
роль артефакта определяет формат.

## Manifest разработчика

Текущий packer уже принимает UTF-8 и quoted values:

```text
RUNE-ABI 1
package org.rustos.apps.explorer
kind application
runtime-abi 1 1
version 1 0 0
lifecycle multi-instance
name default "Files"
name ru-RU "Проводник"
summary ru-RU "Просмотр и изменение файлов"
vendor "RustOS Project"
category system.files
capability required org.rustos.vfs/1 1 0x3 4
icon 64 64 100 svg any application assets/explorer.svg
resource ui/main application/rui assets/explorer.rui
```

`multi-instance` означает новый процесс и чистое instance state при каждом
запуске. `single-instance` разрешает launcher передать activation существующему
процессу, но не позволяет системе молча восстанавливать закрытое окно. Сервис и
драйвер используют отдельные managed lifecycle и restart policy.

Manifest является декларацией. Если приложение запросило graphics/VFS API,
но supervisor policy не проложила соответствующий capability route, required
dependency останавливает запуск до entry point, optional dependency возвращает
типизированное `Unavailable`.

## DLL и «заголовки» для Rust

Копировать `.rs`-заголовки по проектам не нужно. Публичная DLL несёт
`INTERFACE_SCHEMA` — каноническую language-neutral RUIDL-схему:

- типы с точным layout;
- функции и canonical signatures;
- ownership/borrowing buffers и handles;
- sync/async semantics;
- errors и допустимые ABI versions;
- capability/service requirements.

Первый реализованный шаг уже встраивает проверенный исходник `RUNE-ABI 1` в
этот record. Следующий SDK-этап заменяет ручное дублирование единым RUIDL
generator:

```text
library.rune / installed SDK registry
                 |
                 v
        rustos sdk resolve
                 |
        schema hash + target ABI
                 v
  shared SDK cache/generated Rust crate
      | raw extern "C" declarations
      | safe handles, Result, slices, RAII
      ` documentation
                 |
                 v
             cargo build
```

Приложение подключает обычный crate и пишет идиоматический Rust:

```rust,ignore
use rustos_vfs::{OpenOptions, Vfs};

fn save(vfs: &Vfs, path: &str, bytes: &[u8]) -> rustos_vfs::Result<()> {
    OpenOptions::new().create(true).truncate(true).open(vfs, path)?.write_all(bytes)
}
```

Generated crate хранится один раз в content-addressed SDK cache. Он не
копируется в каталог каждого проекта. В final RUNE не попадают schema source,
документация и весь wrapper package: остаются вызванные функции wrapper и
stable import records. Там, где нужен строго нулевой overhead, safe wrapper
инлайнится до одного проверяемого indirect call в DLL.

Готовые `SDK_BINDINGS` допустимы как кэш и для offline self-hosting, но не
являются источником истины: версия rustc меняется, а RUIDL остаётся стабильным.
Позднее из той же схемы можно генерировать C headers, Node.js bindings и
документацию без смены DLL ABI.

### Зависимость DLL от DLL

Каждый модуль объявляет только прямые dependencies. Resolver строит полный
ориентированный граф, обнаруживает cycles, выбирает provider по
`InterfaceId + ABI range + optional PackageId`, проверяет весь closure и лишь
потом отображает страницы. Транзитивные зависимости не копируются в manifest
приложения вручную.

Нет `PATH`, `LD_LIBRARY_PATH`, current-directory lookup и общего namespace
символов. Две private реализации одного интерфейса могут сосуществовать, если
package graph однозначно закрепляет provider. System provider выбирает
supervisor policy, а приложение не может подменить его файлом рядом с собой.

После eager resolution вызов in-process DLL — обычный непрямой вызов. Sealed
RX/RO pages физически разделяются процессами; GOT, writable data и TLS у
каждого процесса свои. System DLL stub может внутри перейти на IPC или shared
memory, не меняя API приложения.

## Запуск процесса

Целевой путь запуска состоит из транзакции:

1. launcher получает capability на RUNE, а не произвольный kernel path;
2. parser проверяет header, TOC, hash/signature, W^X и все cross-references;
3. resolver выбирает ISA slice и полностью разрешает dependency graph;
4. supervisor сопоставляет capability requests с policy и строит startup
   namespace;
5. loader резервирует address space, отображает immutable shared pages и
   private data, применяет relocations, собирает TLS и закрывает RELRO;
6. CRT получает typed `ProcessStartInfo`: argv, environment, clocks, random,
   stdio и capability slots;
7. управление передаётся `_start`, который вызывает обычный Rust `fn main`;
8. любая ошибка до commit освобождает staging целиком; после fault process
   manager закрывает handles, будит waiters и возвращает frames allocator'у.

Ядро отвечает только за процессы, threads, virtual memory, scheduler, IPC,
capabilities, interrupts и минимальные graphics/sync objects. VFS parser,
package policy, DLL resolver, GUI runtime и driver policy должны завершить
переезд из bootstrap kernel-кода в изолированные user-space services.

## Системные API приложения

Публичный SDK группируется по задачам, а не по внутренним сервисам:

| Facade | Что видит приложение | Что скрыто системой |
|---|---|---|
| `std` / `vfs.dll` | files, directories, streams, async I/O | VFS IPC, shared windows, filesystem driver |
| `process.dll` | spawn, wait, child handles, workers | scheduler, address spaces, raw process syscall |
| `window.dll` | independent windows, lifecycle и typed events | surface protocol, z-order, input routing |
| `system-ui.dll` | components, layout, styles, accessibility | display list, atlas, damage, CPU/GPU backend |
| `graphics.dll` | Canvas2D, Canvas3D, images, fonts | GraphicsBuffer, timelines, renderd protocol |
| Mesa/EGL | OpenGL/OpenGL ES contexts bound to a window region | VirGL/native driver/software rasterizer |
| `time.dll` | monotonic/wall clocks and timers | hardware timer and timer queue |

Приложение может запросить Canvas3D/OpenGL, но не «Virtio GPU» или «Metal».
EGL является platform binding между OpenGL ES и native window surface; в
RustOS этот native object выдаёт `graphics.dll`. При отсутствии подходящего
GPU тот же контракт обслуживает software backend. Standard SystemUI controls
восстанавливаются системой после reset GPU; custom 3D surface получает
`ContextLost` и пересоздаёт только собственные GPU resources.

## Instance, окна и состояние

Application package, process instance и window — разные объекты:

- один package можно запустить несколько раз;
- один process может создать несколько независимых окон;
- закрытие окна уничтожает его surface и state;
- закрытие последнего окна по умолчанию завершает process;
- сохранение документа или session state происходит только явным API;
- crash одного instance не влияет на другой и не останавливает window server.

Taskbar и Start menu получают имя/иконку из manifest, а состояние окон — из
window server. Они не вызывают код приложения для построения списка программ.

## Сборка и self-hosting

Один pipeline используется на macOS/Linux и внутри RustOS:

```text
Cargo.lock + source + RUIDL dependencies
      -> rustc + rust-lld -> ELF64 PIE intermediate
      -> rustos-rune pack-manifest
      -> verify dependency/capability graph
      -> deterministic .rune + optional detached debug package
      -> atomic install into VaraniaFS package store
```

Для universal release packer объединяет AMD64/AArch64 slices только когда
package ID, manifest, interface graph и resources совпадают. `BuildId` зависит
от нормализованного содержимого, timestamps не входят. Host и native build
должны выдавать одинаковый digest при одинаковом toolchain manifest.

Минимальные release gates:

- malformed/truncated/overflow/unknown-required RUNE tests;
- ABI compatibility и missing/cyclic dependency tests;
- W^X, RELRO, TLS и shared RX tests на обеих ISA;
- capability denial до entry point;
- process fault/kill/reap и полный resource reclaim;
- один application RUNE в forced-software и accelerated graphics profiles;
- reproducible host build и повторная native self-hosted build.

## Статус и следующие рубежи

Уже реализованы fixed RUNE header/TOC, AMD64/AArch64 slices, imports/exports,
dependency ABI ranges, relocations, TLS, RELRO, shared RX, content hash,
ring-3 resolver, typed manifest, UTF-8 metadata, icon/resource records и
embedded interface schema. `hello.rune` собирается с локализованным manifest и
SVG icon.

До зрелой границы ещё обязательны:

1. RUIDL parser/code generator и content-addressed SDK cache;
2. capability requests из manifest и user-space supervisor/resolver policy;
3. атомарный package closure с private dependencies и signatures;
4. перенос оставшейся loader/policy логики из kernel bootstrap в ring 3;
5. публичные `window/system-ui/graphics` DLL и независимый ring-3 sample;
6. fault-injection всего запуска и unload/restart lifecycle;
7. перевод desktop/Terminal/Проводника на эту границу;
8. переключение системных UI/graphics providers на GPU без второго публичного
   API и без прямой зависимости приложений от renderer crates.

## Технические ориентиры

Архитектура не копирует чужой формат, но использует проверенные разделения:

- capability routing и отдельный lifecycle компонента — официальный
  [Fuchsia Component Framework](https://fuchsia.dev/fuchsia-src/concepts/components/v2/introduction);
- ELF64 dynamic sections/relocations остаются только на toolchain boundary —
  [System V ELF gABI](https://refspecs.linuxfoundation.org/elf/gabi4%2B/ch5.dynamic.html);
- EGL связывает graphics API с native window surface, не раскрывая приложению
  драйвер — [Khronos EGL Registry](https://registry.khronos.org/EGL/).

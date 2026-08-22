# Динамические библиотеки RustOS

Финальный формат DLL — RUNE container с флагом `LIBRARY`. Слово DLL описывает
семантику разделяемого модуля, но библиотека не является PE/Windows DLL и не
обязана иметь расширение `.dll`. Рекомендуемое имя пакета —
`/system/lib/vfs@1.rune`; приложения зависят от interface ID, не от пути.

Полная спецификация container, imports/exports и ABI ID находится в
[`RUNE.md`](RUNE.md).

## Вызов и производительность

Loader один раз проверяет библиотеку, применяет eager relocations и закрывает
RELRO. Sealed RX/RO regions используют одни физические страницы во всех
процессах; writable data, GOT и TLS у каждого процесса свои. После resolution
вызов DLL — обычный непрямой вызов функции без syscall, IPC и сериализации.

Публичный ABI использует `extern "C"`, `#[repr(C)]` и типы фиксированной
ширины. Rust ABI не стабилен между версиями compiler и наружу не выходит.
Безопасные Rust crates являются тонкими wrappers поверх C ABI.

Копировать Rust declarations в каждый проект не требуется. Каждая публичная
DLL несёт `INTERFACE_SCHEMA`: канонический RUIDL-контракт функций, layouts,
ownership/slices, linear handles, bounds и наборов ошибок. `rustos-ruidl`
генерирует из текущего контракта raw `-sys`
и safe Rust crates и кэширует по hash схемы/target ABI. Необязательный
`SDK_BINDINGS` может ускорять offline build,
но остаётся производным артефактом, а не вторым источником истины.

## DLL не заменяет системный сервис

```text
application
   |
   | normal call
   v
vfs client DLL       validation, batching, shared-window management
   |
   | capability IPC
   v
vfsd -> filesystem service -> block service -> device
```

DLL не содержит VaraniaFS parser и не получает block capability. Код client
stub разделяется и вызывается быстро, а привилегии, cache и mutable filesystem
state остаются у одного изолированного, перезапускаемого сервиса.

## Поиск и версии

RUNE dependency содержит `InterfaceId`, допустимый диапазон ABI и optional
package ID. Supervisor формирует доверенный package graph; loader не ищет
«первый подходящий файл» в current directory. Поэтому private и system
providers могут сосуществовать без DLL search-order hijacking.

Major несовместимость получает новый interface ID. Совместимое расширение
повышает ABI version внутри разрешённого диапазона. `SymbolId` строится из
interface ID и канонической C signature; readable name сохраняется для
диагностики и проверки collision.

Unload допускается только после завершения вызовов и TLS destructors. Первая
реализация не выгружает system DLL до выхода процесса: это проще, быстрее и
исключает use-after-unload.

Зависимости транзитивны: DLL объявляет собственные прямые dependencies, а
resolver строит и проверяет весь graph до map. Приложение не копирует
транзитивный список и не получает общий symbol namespace. Это позволяет DLL
использовать другую DLL без `PATH`/current-directory lookup и search-order
hijacking.

## Исполняемый RUNE resolver

User-space crate `rustos-rune-loader` уже загружает нативные RUNE libraries:
dependency discovery, interface/symbol IDs, ABI ranges, eager relocations,
combined static TLS, RELRO и sealed shared RX выполняются в ring 3. Fixtures
`loader-root.rune` и `fixture-1.rune` читаются из VaraniaFS; system image не
содержит их прежние ELF DLL.

Boot-test защищает следующие свойства:

- транзитивной зависимости и отсутствующего обязательного symbol;
- несовместимого ABI range;
- отдельного TLS каждого процесса;
- RELRO write fault;
- одной физической sealed RX страницы у двух процессов;
- падения loader/service без остановки kernel.

Прежний `rustos-elf-loader` пока сохранён как читаемая migration/reference
реализация и для импорта сторонних toolchain artifacts. Публичным ABI системы
являются только RUNE records и SDK manifest, а не `DT_NEEDED`/GNU symbol rules.

Подробности прежнего test loader сохранены в [`ELF_LOADER.md`](ELF_LOADER.md).

## Пользовательские библиотеки

SDK использует декларативный `RUNE-ABI 1` manifest из `sdk/abi`. Команда
`rustos-rune pack-manifest` запрещает необъявленные undefined imports,
проверяет exports против `.dynsym`, формирует import/export/dependency records
и встраивает исходную схему. `rustos-ruidl resolve` извлекает этот же record и
атомарно публикует generated crates в общем SDK cache; подробности —
[`RUIDL.md`](RUIDL.md).

Сейчас embedded источник уже можно проверить и прочитать без ELF sections:

```bash
cargo run -p rustos-rune -- schema /system/lib/example.rune
```

Полная модель executable/DLL/package boundary описана в
[`APPLICATION_MODEL.md`](APPLICATION_MODEL.md).

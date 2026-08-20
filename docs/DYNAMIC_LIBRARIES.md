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
проверяет exports против `.dynsym` и формирует import/export/dependency
records. Следующее расширение генератора создаст из того же manifest C header
и safe Rust wrapper, чтобы документация и бинарный контракт не расходились.

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

## Переходный ELF loader

Существующий user-space loader уже исполняет ELF64 `ET_DYN`, `DT_NEEDED`,
GNU symbols, основные AMD64 RELA, initial-exec TLS, RELRO и shared RX. Он
проверяется fixtures `root.elf` и `fixture-1.dll` из VaraniaFS и остаётся
эталоном поведения при миграции.

Новые запускаемые system applications уже являются `.rune`. Но нативный
RUNE resolver imports/exports/dependencies ещё не подключён, поэтому ELF DLL
fixtures временно остаются в образе. Это не финальный публичный ABI. Удаление
ELF пути разрешено только после эквивалентных тестов RUNE для:

- транзитивной зависимости и отсутствующего обязательного symbol;
- несовместимого ABI range;
- отдельного TLS каждого процесса;
- RELRO write fault;
- одной физической sealed RX страницы у двух процессов;
- падения loader/service без остановки kernel.

Подробности прежнего test loader сохранены в [`ELF_LOADER.md`](ELF_LOADER.md).

## Пользовательские библиотеки

SDK должен генерировать interface manifest из декларативного ABI-файла,
проверять layouts, запрещать случайные Rust-mangled exports и вызывать
`rustos-rune` после linker. Один manifest используется для C header, Rust
wrapper, import records и ABI compatibility test, чтобы документация и
бинарный контракт не расходились.

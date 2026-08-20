# DLL в RustOS

RustOS использует расширение `.dll`, но не изобретает ещё один бинарный
формат: DLL является ELF64 `ET_DYN`. Это даёт готовые program headers,
`.dynsym`, GNU hash, `DT_NEEDED`, TLS и RELA relocations. Дополнительная
секция `.rustos.module` содержит [`ModuleDescriptor`](../abi/src/dll.rs).

## Статус

Формат `ModuleDescriptor`, правила ABI, именования и relocations уже
зафиксированы. Kernel process loader отображает ELF64 `ET_DYN` PIE,
обрабатывает `R_X86_64_RELATIVE` и запускает entry в отдельном CPL3 address
space. Первая системная библиотека
[`libs/vfs-dll`](../libs/vfs-dll/src/lib.rs) действительно собирается как
`/system/lib/vfs-1.dll`: это ELF64 `ET_DYN` с `DT_SONAME=vfs-1.dll` и
unmangled `rustos_vfs_*` C exports, а не текстовый manifest или заглушка.

User-space loader [`rustos-elf-loader`](../libs/elf-loader/src/lib.rs) уже
разрешает `.dynsym`, транзитивные `DT_NEEDED`, eager RELA/PLT, static TLS и
GNU RELRO. Неизменяемые сегменты создаются как shared objects: временный RW
mapping заполняется, снимается, object необратимо запечатывается в R/RX и
после этого может отображаться в несколько процессов.

Boot-test загружает `/apps/loader-test/root.elf` из VaraniaFS. Его настоящий
`DT_NEEDED=fixture-1.dll` находится по system search path, импорт
`fixture_answer` разрешается, TLS template получает отдельный экземпляр,
GOT становится read-only, а вызов возвращает 42. Второй процесс получает
только capability sealed RX-сегмента dependency и исполняет ту же физическую
страницу. Подробный контракт — в [`docs/ELF_LOADER.md`](ELF_LOADER.md).

## Быстрый локальный вызов

Dynamic loader один раз отображает RX-страницы DLL во все использующие её
процессы одними физическими кадрами. Writable data/GOT выдаются процессу
отдельно (copy-on-write), RELRO после relocation становится read-only.

После eager/lazy symbol resolution вызов идёт через PLT/GOT и стоит почти как
обычный непрямой вызов функции. Здесь нет IPC, сериализации и переключения в
kernel. Поэтому `math.dll`, allocator, widget client logic и parser удобно
делать настоящими DLL.

Публичный ABI — только `extern "C"` и структуры `#[repr(C)]` фиксированной
ширины. Rust ABI между разными версиями compiler не считается стабильным.
Безопасная Rust-обёртка может быть отдельным тонким crate поверх C ABI.

## DLL не заменяет системный сервис

```text
application
   |
   | normal call
   v
vfs-1.dll (валидация, batching, shared-buffer management)
   |
   | capability IPC
   v
vfsd -> fs driver -> block driver -> device
```

`vfs-1.dll` не содержит filesystem driver. Если поместить драйвер в каждый
процесс, код начнёт дублироваться, cache/journal разойдутся, а повреждение
памяти приложения даст прямой доступ к диску. DLL является быстрым client
stub, а состояние и привилегии принадлежат одному перезапускаемому сервису.

Для больших `read/write` DLL регистрирует shared-memory window. IPC переносит
только handle, offset и length; данные не копируются через inline message.
Мелкие операции объединяются в batch. Так издержки границы сервиса остаются
предсказуемыми.

## Loader и совместимость

Loader ищет зависимости в следующем порядке:

1. каталог приложения и его manifest;
2. `/apps/<id>/lib` для приватных зависимостей;
3. `/system/lib` для system ABI.

`SONAME` содержит major ABI: `vfs-1.dll`, `ui-1.dll`. Minor/patch не ломают
существующие exports. Две несовместимые major-версии могут быть отображены
одновременно. Поиск «любой DLL с подходящим именем» из текущего каталога
запрещён: зависимости фиксирует manifest, иначе сборка невоспроизводима.

Поддерживаются основные x86-64 RELA relocations:

- `R_X86_64_RELATIVE`;
- `R_X86_64_64`;
- `R_X86_64_PC32`, `R_X86_64_32`, `R_X86_64_32S`;
- `R_X86_64_GLOB_DAT`;
- `R_X86_64_JUMP_SLOT`;
- `R_X86_64_DTPMOD64`, `R_X86_64_DTPOFF64`, `R_X86_64_TPOFF64`.

Используется eager binding: он проще, детерминированнее и переносит ошибку
отсутствующего symbol на запуск. TLS fixture собирается в initial-exec модели;
general-dynamic `__tls_get_addr` и lazy PLT будут добавлены после
thread-safe resolver/DTV ABI.

## Пользовательские DLL

SDK предоставляет macro/build tool, который:

- генерирует `.rustos.module` и export map;
- запрещает случайный экспорт Rust-mangled symbols;
- создаёт import metadata для linker;
- проверяет ABI layout и major version;
- упаковывает DLL, license и manifest в application bundle.

Unload разрешается только после обнуления reference count и завершения всех
вызовов/TLS destructors. На первом этапе system DLL не выгружаются до выхода
процесса — это быстрее и исключает класс use-after-unload.

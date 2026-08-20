# DLL в RustOS

RustOS использует расширение `.dll`, но не изобретает ещё один бинарный
формат: DLL является ELF64 `ET_DYN`. Это даёт готовые program headers,
`.dynsym`, GNU hash, `DT_NEEDED`, TLS и RELA relocations. Дополнительная
секция `.rustos.module` содержит [`ModuleDescriptor`](../abi/src/dll.rs).

## Статус

Формат `ModuleDescriptor`, правила ABI, именования и relocations уже
зафиксированы. ELF64 dynamic loader и отображение DLL в процессы — следующий
user-space milestone; файлы из bootstrap manifest пока являются планом, а не
ложными заглушками с именем `*.dll`.

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

Минимально поддерживаются relocations x86-64:

- `R_X86_64_RELATIVE`;
- `R_X86_64_64`;
- `R_X86_64_GLOB_DAT`;
- `R_X86_64_JUMP_SLOT`;
- TLS relocations выбранной initial-exec/general-dynamic модели.

Сначала используется eager binding: он проще, детерминированнее и переносит
ошибку отсутствующего symbol на запуск. Lazy PLT можно включить позже только
после thread-safe resolver.

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

# Пользовательский ELF64/DLL loader

## Граница ответственности

Kernel не должен разбирать граф зависимостей и доверять строкам/таблицам из
произвольных DLL. Он запускает минимальный ring-3 bootstrap, предоставляет VM,
TLS и capabilities. Crate
[`libs/elf-loader`](../libs/elf-loader/src/lib.rs) выполняет динамическую
линковку в address space процесса.

```text
kernel: create process + bootstrap mapping
                 |
                 v
ld-rustos / rustos-elf-loader (ring 3)
  |-- read root ELF through vfs-1.dll
  |-- breadth-first DT_NEEDED search
  |-- map PT_LOAD under W^X
  |-- resolve dynsym and eager RELA/PLT
  |-- build per-thread TLS image
  |-- protect PT_GNU_RELRO
  `-- enter executable / return DLL exports
```

Parser и linker bounded: максимум 8 модулей, 8 `PT_LOAD` на модуль, 8
`DT_NEEDED` и 8 mappings на модуль. Heap не нужен. Все file offsets, размеры и
арифметика адресов проверяются до отображения; W+X сегменты и relocation в
RX/RO страницу отклоняются.

## Поиск `DT_NEEDED`

Dependencies обходятся breadth-first, поэтому global symbol scope
детерминирован: root, его прямые зависимости, затем следующий уровень. Для
каждого безопасного SONAME без slash проверяются:

1. каталог приложения;
2. приватный `/apps/<id>/lib`, если он задан manifest;
3. `/system/lib`.

Файл дедуплицируется по `DT_SONAME`. Относительные обходы `..`, абсолютный
путь внутри `DT_NEEDED` и «поиск в текущем каталоге shell» не допускаются.
Текущий `ModuleSource` возвращает immutable image; VFS adapter заранее читает
root/dependencies потоково в bounded buffers.

## Символы и relocations

Loader понимает SysV `DT_HASH` и GNU hash для определения границ `.dynsym`.
Global scope предпочитает strong definition, затем weak; local и hidden
symbols остаются внутри модуля. Undefined strong import останавливает запуск,
undefined weak получает ноль.

Для AMD64 реализованы:

| Группа | Relocations |
|---|---|
| адреса | `R_X86_64_RELATIVE`, `R_X86_64_64` |
| imports/PLT | `R_X86_64_GLOB_DAT`, `R_X86_64_JUMP_SLOT` |
| compact/PC-relative | `R_X86_64_PC32`, `R_X86_64_32`, `R_X86_64_32S` |
| static TLS | `R_X86_64_DTPMOD64`, `R_X86_64_DTPOFF64`, `R_X86_64_TPOFF64` |

Binding eager: все ошибки видны до передачи управления приложению, GOT не
приходится временно открывать после старта нескольких потоков. COPY, IFUNC,
TLSDESC и lazy binding пока намеренно возвращают `UnsupportedRelocation`.

## TLS

Каждый `PT_TLS` получает module id и выровненный диапазон в общем static TLS
block. На AMD64 применяется SysV variant II: thread pointer указывает на TCB
после templates/BSS, `%fs:0` содержит self pointer, `TPOFF64` отрицателен.
`initialize_tls` создаёт отдельную копию для каждого потока; затем runtime
вызывает `thread_set_tls`.

Исполняемый fixture использует настоящий `#[thread_local]` и initial-exec
`R_X86_64_TPOFF64`, так что boot-test проверяет не только наличие структур,
но и чтение TLS после установки FS base. General-dynamic модель потребует DTV
и `__tls_get_addr`; она отмечена как следующий совместимый этап.

## RELRO и W^X

Writable `PT_LOAD` сначала отображается RW. После всех eager relocations
page-aligned `PT_GNU_RELRO` переводится в R через `vm_protect`. Попытка
релокации в неизменяемый сегмент считается text relocation и отклоняется.
Private `.data`/BSS/TLS никогда не разделяются между процессами.

RO/RX сегмент создаётся иначе:

1. loader создаёт RW shared-memory object;
2. отображает его во временное окно и копирует file bytes;
3. удаляет единственный RW mapping;
4. `shared_memory_seal` необратимо меняет maximum rights на R либо RX;
5. loader отображает sealed object по конечному адресу.

Seal разрешён только при одном capability reference и нуле mappings. После
него kernel заменяет authority handle на `READ [| EXECUTE] | MAP | TRANSFER`;
вернуть WRITE невозможно. Поэтому одна физическая code page безопасно
отображается во многие процессы и остаётся совместимой с W^X.

## Исполняемый тест

Build создаёт две настоящие DLL:

- `/apps/loader-test/root.elf` имеет настоящий `e_entry=linked_answer` и
  `DT_NEEDED=fixture-1.dll`;
- `/system/lib/fixture-1.dll` экспортирует TLS-функцию `fixture_answer` и
  независимую `fixture_shared_answer`.

`loader-test.elf` читает их через `vfsd`, загружает оба модуля, устанавливает
TLS и вызывает `linked_answer == 42`. Затем он передаёт capability sealed RX
сегмента новому `loader-child.elf`; ребёнок отображает ту же физическую
страницу по другому адресу и получает 41. После wait все private/shared frames
должны вернуться allocator'у.

Serial marker успешного теста:

```text
[loader] DT_NEEDED symbols RELA TLS RELRO and cross-process shared RX verified
```

## Следующий шаг

Нужно сделать loader настоящей точкой входа каждого динамического процесса:
kernel создаёт address space с маленьким `ld-rustos`, а supervisor передаёт
root executable/VFS handles и argv/env. Затем добавляются constructors,
general-dynamic TLS, `dlopen`/`dlclose`, versioned symbols, GNU RELRO audit и
общесистемный read-only page cache service.

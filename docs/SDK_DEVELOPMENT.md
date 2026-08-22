# Приложения, утилиты, сервисы и DLL RustOS

Это практическое руководство дополняет архитектурные документы. Оно показывает,
какой маленький артефакт создавать и на какой границе остановиться.

## Выбор типа артефакта

| Требование | Артефакт | Где начать |
|---|---|---|
| Команда/непривилегированная программа | RUNE application | `sdk/examples/hello` |
| Переиспользуемая локальная функция | RUNE DLL + safe facade | `libs/vfs-dll`, `libs/vfs` |
| Владелец устройства/общего mutable state | RUNE service | `userspace/bootstrap/src/vfsd.rs` |
| Стабильные records между процессами | ABI module | `abi/src` |
| Новый control/layout/event | System UI component | `system-ui/src` |
| Image/format builder для macOS/Linux | host tool | `tools/` |

Не создавай service для чистой функции и не помещай драйвер/parser внутрь DLL.
DLL делит read-only code и выполняет обычный вызов; service изолирует state и
привилегии за capability IPC.

## Малое ring-3 приложение

Минимальная структура повторяет `sdk/examples/hello`:

```text
sdk/examples/my-tool/
  Cargo.toml
  src/main.rs
```

`main.rs` содержит обычный `fn main`. Для RustOS target CRT подключается как
unused import, потому что именно он предоставляет `_start`:

```rust
#![cfg_attr(target_os = "rustos", feature(restricted_std))]

#[cfg(target_os = "rustos")]
use rustos_crt as _;

#[cfg(target_os = "rustos")]
fn main() {
    println!("Небольшая утилита RustOS");
}

#[cfg(not(target_os = "rustos"))]
fn main() {
    eprintln!("Эта программа предназначена для RustOS");
}
```

Добавь package в workspace только когда он действительно собирается. Не пиши
свой `_start`, syscall asm или parser RUNE/VFS. Files/process/time/thread API
бери из `std`; специфическую capability-функцию — из safe DLL facade.

Сборка приложения:

```bash
bash scripts/build-std.sh build -p <package> \
  --target targets/x86_64-unknown-rustos.json
```

ELF64 после linker — промежуточный файл. Устанавливается только проверенный
`.rune`, созданный `rustos-rune`; расположение в system image добавляется
отдельным небольшим change в `scripts/build.sh`/image manifest.

Для устанавливаемого приложения добавь рядом UTF-8 manifest, как в
`sdk/examples/hello/hello.rune-abi`. Он задаёт stable package ID, runtime ABI,
lifecycle, version, локализованное имя, icons и resources. Packer встраивает
их в один RUNE и отклоняет абсолютные/родительские resource paths.

## Небольшая системная утилита

Утилита остаётся обычным приложением. Она не получает системные права только
из-за пути `/system/bin`. Передавай ей минимальные capabilities через manifest
и supervisor policy. Команды файловой системы должны использовать `std::fs`
либо `vfs` facade; не копируй IPC protocol в каждую `ls`/`cp`/editor.

Хорошая первая версия делает одну операцию и имеет:

- `--help` и понятный ненулевой exit status;
- bounded/streaming I/O вместо чтения всего файла;
- корректный short read/write;
- UTF-8 diagnostic без предположения о Unix path bytes;
- unit-тест чистого parser/operation и один запуск RUNE.

Набор маленьких программ предпочтительнее одной привилегированной «всё умеет»
утилиты, если операции имеют независимый lifecycle. Общий CLI parser или file
copy loop выносится в DLL только после появления минимум двух потребителей.

## DLL: manifest, raw ABI и safe facade

Источник истины — `sdk/abi/<name>-<major>.rune-abi`. Пример синтаксиса:

```text
RUNE-ABI 1
package org.rustos.example.text
kind library
interface org.rustos.text/1
abi 1
export rustos_text_measure measure(*const_u8,usize,*mut_u64)->i32 function
```

Реализация делится на три маленьких слоя:

1. raw `no_std` export crate: только `#[no_mangle] unsafe extern "C"`, проверка
   pointers/lengths и panic boundary;
2. safe Rust facade: slices, typed errors, RAII handles; все `unsafe` спрятаны;
3. consumer example, который импортирует facade, а не raw symbol.

Исходная ABI-схема встраивается в DLL как `INTERFACE_SCHEMA`. Команда
`rustos-ruidl resolve` генерирует из неё raw и safe Rust crates в общий
content-addressed cache, поэтому
разработчик не копирует declarations в каждый проект. Готовые Rust bindings
могут поставляться как оптимизация, но schema остаётся единственным источником
истины и позднее генерирует также C/Node.js bindings.

Имена функций, canonical signatures и manifest должны совпадать. Не используй
в публичной сигнатуре `bool`, Rust enum, `char`, `String`, slice, reference,
trait object, `usize` без явно зафиксированного target ABI или generic type.
Для текущего 64-bit ABI manifest уже поддерживает `usize`, однако disk/wire
длины всё равно предпочтительно задавать `u64`, если значение хранится.

Указатель валиден только на время локального вызова в одном address space.
Если facade обращается к service, он копирует маленький request в IPC, а buffer
передаёт через shared memory. Provider не сохраняет пользовательский pointer.

Сборка и упаковка library:

```bash
# Сначала target ELF64 ET_DYN, затем:
cargo run -p rustos-rune -- pack-manifest \
  <library.elf> <library.rune> sdk/abi/<name>-1.rune-abi
cargo run -p rustos-rune -- verify <library.rune>
cargo run -p rustos-rune -- inspect <library.rune>
```

Acceptance для одной DLL-функции: manifest validation, success, null/truncated/
capacity error, ABI mismatch, два независимых callers и отсутствие IPC в pure
local hot path.

## Service и client facade

Сначала добавь один versioned request/reply в `abi`, затем:

```text
application -> safe facade -> DLL stub -> bounded IPC/shared memory -> service
```

Каждый слой проверяется отдельно. Service получает device/admin capability от
supervisor, клиент — только endpoint с нужными rights. Обработчик выполняет:

1. ABI version/opcode/payload length/rights validation;
2. безопасное отображение или копирование shared buffer;
3. операцию без удержания чужого pointer;
4. typed reply и обязательное закрытие transferred handles на любом пути.

Fault/restart и recovery являются отдельной задачей после рабочего protocol
slice. Они обязательны до объявления service production-ready.

## GUI сейчас и позже

`rustos-system-ui` уже является общей component library, но GUI applications
в `kernel/src/apps` пока bootstrap-объекты. Публичная ring-3 граница будет
состоять только из Window/SystemUI/Canvas/Graphics facades. При их доработке:

- state принадлежит экземпляру приложения;
- retained tree строится `UiBuilder`;
- приложение получает `CommandId`/events;
- только backend adapter знает `Framebuffer`/fonts;
- runtime damage передаётся compositor;
- стандартный control не рисуется вручную.
- приложение не зависит от `rustos-abi`, `rustos-runtime`, surface/ui-gpu,
  VirGL, video или Mesa platform crates;
- один application RUNE работает с forced software и GPU provider.

Не добавляй выдуманный ring-3 API до реализации `uid`/UI session ABI. Перенос
bootstrap-приложения на компоненты и последующий перенос его process boundary —
два разных changes. Для Terminal действует
`docs/TERMINAL_SYSTEM_UI_MIGRATION.md`.

Полный стабильный контракт описан в `docs/APPLICATION_MODEL.md`. CI запускает
`scripts/check-sdk-boundaries.sh`, чтобы пример приложения не протащил
внутренний transport напрямую.

## Definition of done малой задачи

- Реализован один заявленный результат, без соседней архитектуры «заодно».
- Публичная граница документирована на русском; `unsafe` имеет `SAFETY`.
- Есть success и минимум один relevant failure/capacity/lifecycle test.
- Пройдены команды ближайшего `AGENTS.md` и матрицы
  `docs/AGENT_WORKFLOW.md`.
- Устанавливаемый artifact — проверенный RUNE; статус bootstrap/ring 3 указан
  честно.
- В change отсутствуют чужие незакоммиченные файлы и generated `build/target`.

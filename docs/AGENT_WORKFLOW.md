# Разработка RustOS небольшими модулями

Этот документ превращает архитектуру RustOS в рабочий маршрут для coding-agent.
Он особенно рассчитан на небольшую модель: одна задача, одна граница, короткий
цикл проверки. Правила конкретного каталога находятся в его `AGENTS.md`.

## Что в системе является рабочим сейчас

| Возможность | Текущая реализация | Нельзя утверждать |
|---|---|---|
| CLI ring 3 | `std`, CRT, RUNE runner, capability startup и VFS работают | что native `rustc` уже работает |
| GUI | component runtime и CPU backend работают; desktop-приложения пока bootstrap-объекты kernel | что GUI-приложения уже изолированы в ring 3 |
| DLL | RUNE library resolver, imports/exports, TLS и RELRO работают | что Rust ABI стабилен или DLL равна service |
| VFS | изолированный `vfsd`, shared-memory streaming, COW B+tree, checksums, snapshots, scrub/fsck | что исчерпывающее fault-injection тестирование равно production-аудиту |
| SMP | AP startup и вытеснение CPU0 проверены, AP пока безопасно parked | что scheduler распределяет процессы по всем CPU |
| Display | virtio 2D/firmware fallback, CPU compositor и bounded ring-3 VirGL command path | что upstream Mesa и native Intel/AMD/Apple/V3D drivers уже портированы |

Перед задачей сверяй таблицу с `docs/ARCHITECTURE.md`: статус может измениться.

## Карта ответственности

| Каталог | Что там меняют | Первый документ |
|---|---|---|
| `abi/` | стабильные syscall/IPC/wire структуры | `docs/PROCESS_MEMORY_ABI.md`, `docs/IPC.md` |
| `microkernel/` | переносимые lifecycle, scheduler, IPC, capabilities | `docs/MICROKERNEL.md` |
| `kernel/` | privileged orchestration и platform adapters | `docs/ARCHITECTURE.md` |
| `runtime/`, `userspace/` | PAL/CRT, ring-3 программы и сервисы | `docs/RUST_STD.md`, `docs/VFS.md` |
| `system-ui/` | дерево компонентов, layout, events, display list | `docs/SYSTEM_UI.md` |
| `video/` | pixel/surface/compositor primitives | `docs/VIDEO.md` |
| `system-assets/` | версии и пакеты шрифтов/иконок/cursors/wallpaper | `docs/SYSTEM_ASSETS.md` |
| `libs/` | safe DLL facades и loaders | `docs/DYNAMIC_LIBRARIES.md`, `docs/RUNE.md` |
| `rune-format/` | on-disk executable container | `docs/RUNE.md` |
| `varaniafs/` | on-disk filesystem implementation | `docs/VARANIAFS.md` |
| `sdk/` | примеры и публичные manifests для разработчика | `sdk/README.md` |
| `tools/` | программы macOS/Linux для image/pack/verify | локальный `tools/AGENTS.md` |

Если изменение не помещается в одну строку таблицы, сначала раздели его на
контракт и реализацию. Например: ABI record, host unit test, service handler и
client facade — четыре последовательные задачи, а не одна «сделать VFS API».

## Как формулировать небольшой модуль

Описание задачи обязано содержать:

- один результат, видимый тесту или пользователю;
- точный каталог и перечень разрешённых соседних adapters;
- явно запрещённые изменения;
- существующий файл-образец;
- критерии приёмки и команды проверки;
- требование сохранить чужой dirty worktree.

Подходящие задачи: новый pure parser record с тестами; один system-ui control;
одна команда существующего service; один safe wrapper над готовым ABI; перенос
одного приложения на готовый UI API. Неподходящие: «доделай микроядро»,
«портируй Rust», «напиши GPU driver и compositor».

## Рабочий цикл агента

1. Прочитать root и ближайший `AGENTS.md` полностью.
2. Выполнить `git status --short --branch`, затем назвать чужие изменения,
   которые он не будет трогать.
3. Прочитать только документы и API текущей подсистемы. Через `rg` найти
   аналог и тест, который доказывает требуемое поведение.
4. В двух-трёх предложениях зафиксировать границу: что меняется, что остаётся.
5. Реализовать малый вертикальный срез, не делая параллельную архитектуру.
6. Запустить узкие тесты, затем обязательный gate из матрицы ниже.
7. Проверить `git diff --check`, `git diff --name-only` и убедиться, что чужие
   файлы не попали в change.
8. В отчёте не скрывать skipped tests и bootstrap-ограничения.

## Матрица проверок

Запускай только строки, относящиеся к изменению, но не заменяй обязательный gate
одним `cargo check`.

| Область | Быстрый цикл | Обязательный gate |
|---|---|---|
| `system-ui/` | `cargo test -p rustos-system-ui` | `cargo test -p rustos-system-ui -p rustos-rui -p rustos-abi` |
| `video/` | `cargo test -p rustos-video` | `cargo test -p rustos-video -p rustos-gui-check` |
| `compositor/` | `cargo test -p rustos-compositor` | `make test-host` и `make test-arch` |
| `microkernel/` | `cargo test -p rustos-microkernel` | `make test-host` |
| `abi/` | `cargo test -p rustos-abi` | `make test-host` и `make test-arch` |
| kernel/ring 3 compile | x86-64 target build | `make test-arch` |
| desktop/terminal input/render | соответствующий host unit test | `make test-gui` |
| process/VM/IPC/loader/VFS | узкий crate test | `make test-boot` либо полный `make test` |
| RUNE/RIFS/VaraniaFS format | crate unit + invalid fixtures | `make test-host` и совместимый image verify |
| SDK example | `bash scripts/build-std.sh build -p rustos-sdk-hello --target targets/x86_64-unknown-rustos.json` | запуск RUNE в boot-test |

Для проверки только компиляции kernel используй тот же freestanding target,
что build scripts, а не host target:

```bash
cargo -Zjson-target-spec -Zbuild-std=core,alloc build \
  -p rustos-kernel --target targets/x86_64-unknown-rustos.json
```

Любой change завершает:

```bash
cargo fmt --all -- --check
git diff --check
git status --short
```

`make test-gui` запускает QEMU и может занимать несколько минут на Apple
Silicon/TCG. Это нормальный integration gate, а не быстрый цикл.

## Контракты приложений и библиотек

### Обычное ring-3 приложение

Начинай с `sdk/examples/hello`. Приложение получает argv, environment и
capability slots через `rustos-crt`, использует `std` и safe client facade. Оно
не знает номера syscall, layout диска или адрес framebuffer. Устанавливаемый
артефакт — `.rune`; ELF является только промежуточным файлом toolchain.

### Системный service

Service владеет ресурсом и принимает bounded IPC. Запрос сначала целиком
валидируется, затем меняет состояние. Capability проверяется и ослабляется до
операции; malformed request завершает клиента или возвращает protocol error,
но не паникует. Restart/recovery тестируются отдельно от happy path.

### DLL

DLL — RUNE library с обычным локальным вызовом. Manifest в `sdk/abi` является
источником истины; наружу выходит стабильный C ABI, а safe Rust crate лишь
проверяет типы и владение. DLL facade системного сервиса скрывает IPC, но не
забирает себе его привилегии или mutable global state.

### GUI-приложение

Сейчас есть два разных этапа, их нельзя смешивать:

1. bootstrap GUI-приложение хранится в `kernel/src/apps`, но использует
   `rustos-system-ui` и не содержит input hit-test/layout policy вручную;
2. целевая ring-3 версия будет открывать UI session через stable ABI и `uid`.

До готовности второго этапа перенос существующего GUI должен улучшать первый,
не выдавая kernel object за изолированный process. Новый component API сначала
появляется и тестируется в `system-ui`, затем применяется приложением.

## Формат отчёта

```text
Результат: <одно проверяемое предложение>
Граница: <что намеренно не менялось>
Проверки:
- <команда> — PASS/FAIL/SKIPPED: причина
Изменённые файлы:
- <path>
Осталось: <честное ограничение или «нет»>
```

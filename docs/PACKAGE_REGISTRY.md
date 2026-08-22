# Подписанный package registry и supervisor

RUNE content hash отвечает на вопрос «файл целый?», а registry — «имеет ли
этот publisher право объявить именно этот package текущим?». Эти проверки не
заменяют друг друга. `rune-runner` принимает executable/dependency только если
совпали canonical path, `PackageId`, `BuildId`, SHA-256, размер, версия и роль
manifest из подписанной записи.

## Формат и trust

`RPKGIDX` — маленький fixed-layout little-endian индекс:

- bounded header 160 bytes и entries по 112 bytes;
- общий runtime-предел индекса 4 МиБ одинаков для loader и host tools;
- монотонный `generation` против rollback;
- sorted canonical UTF-8 paths для детерминированного binary search;
- SHA-256 всего entries/strings payload;
- 128-bit key ID и Ed25519 signature domain `RPKG-SIG`;
- нулевые reserved bytes и запрет неизвестных flags.

Runtime crate работает без `std` и `alloc`, проверяет весь индекс до выдачи
`Registry` view и использует strict verification. Закрытых ключей в образе ОС
нет. Локальная сборка содержит отдельный публичный development trust anchor;
флаг индекса заставляет policy явно разрешить его и не даёт принять такой
индекс production-ключом по ошибке.

## Host workflow

```bash
cargo run -p rustos-package-registry-tool --bin rustos-package -- \
  build --generation 7 --development-key --output current.ridx \
  /apps/demo.rune=build/demo.rune \
  /system/lib/ui-1.rune=build/ui-1.rune

cargo run -p rustos-package-registry-tool --bin rustos-package -- \
  verify --minimum-generation 7 --development-key current.ridx \
  /apps/demo.rune=build/demo.rune \
  /system/lib/ui-1.rune=build/ui-1.rune

cargo run -p rustos-package-registry-tool --bin rustos-package -- \
  activate --store package-store --minimum-generation 7 \
  --development-key current.ridx \
  /apps/demo.rune=build/demo.rune \
  /system/lib/ui-1.rune=build/ui-1.rune
```

Production использует `--secret-key-file` при `build` и соответствующий
`--public-key-file` при `verify/activate`. Файл ключа — 32 raw bytes либо 64
hex symbols. Secret не передаётся в аргументах процесса.

`activate` сначала полностью проверяет registry и каждый RUNE, затем пишет
objects и индекс по content hash с `fsync`. Только после этого одним atomic
rename публикуется маленький указатель `current`. Ошибка, отсутствующий object,
collision либо downgrade сохраняют прежний активный набор.

## Постоянный ring-3 supervisor

Kernel bootstrap один раз передаёт `supervisor.rune` минимальный namespace:

- READ/EXECUTE namespace доверенного `rune-runner`;
- VFS request и временно передаваемый private reply endpoint;
- один persistent stdout/stderr pipe;
- launch RECEIVE и lifecycle SEND endpoints.

Launch request — pointer-free 64-byte IPC record с абсолютным RUNE path,
bounded argv, непривилегированным priority и максимум тремя дополнительными
перезапусками после первой попытки.
Supervisor создаёт runner через обычный `process_spawn`, передаёт только
ослабленные capabilities, выполняет `process_wait`, закрывает stale process
handle и возвращает точный `ExitReason`. Kernel adapter не разбирает package
и restart policy; он только доставляет bounded message и читает pipe/reply.

Boot-тест дважды запускает один зарегистрированный package через тот же живой
supervisor, намеренно завершает root service, проверяет bounded cleanup/restart
и выполняет третий запуск новым экземпляром. Это защищает постоянство сервиса,
восстановление VFS reply ownership после reap и отсутствие одноразового kernel
launcher.

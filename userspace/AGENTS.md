# Правила `userspace/`

Здесь работают настоящие ring-3 программы и сервисы. Они не доверены kernel.

- Получай только startup capabilities; не открывай hardware или service по
  глобальному имени в обход supervisor policy.
- Каждый request целиком validate до mutation. Bounds/rights/protocol errors
  возвращаются клиенту, а panic/fault должен быть локализуем process manager.
- Service не хранит process-local pointer клиента. Streaming/bulk I/O использует
  shared memory с явными offsets, длинами и состоянием producer/consumer.
- Не дублируй filesystem/loader/runtime protocol structs: импортируй `abi` и
  используй safe facade из `libs`.
- Bootstrap binaries маленькие и с одним назначением. Orchestration принадлежит
  `init`/supervisor, parsing filesystem — `vfsd`, client convenience — DLL.
- Новое приложение устанавливается как RUNE; ELF остаётся промежуточным build
  artifact и не становится публичным форматом RustOS.

Проверки: target build на обе архитектуры; service protocol/lifecycle требует
host unit tests и `make test-boot`.

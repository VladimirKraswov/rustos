# Правила `varaniafs/`

Это библиотека on-disk filesystem. VFS policy, IPC transport и block driver
находятся в других слоях.

- Все disk offsets, block numbers и file sizes — `u64`; арифметика checked.
- Никогда не доверяй superblock/inode/extent/directory lengths до проверки
  границ device, alignment, version, checksum и отсутствия overlap.
- Mutation имеет crash-consistent порядок: data/metadata preparation, durable
  commit, затем reclamation. Тестируй power loss в каждой точке записи.
- Streaming read/write не требует buffer размером с файл. SSD/HDD hints являются
  оптимизацией и не меняют корректность.
- Parser не использует права пользователя как защиту памяти: capabilities и
  service boundary остаются обязательны даже без Unix permissions.
- Формат не меняется без version/migration и host tool compatibility test.

Проверки: `cargo test -p varaniafs -p rustos-vfs-image`, затем persistence
boot-test для изменения commit/recovery.

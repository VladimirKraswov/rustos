# VaraniaFS v1: формат и восстановление

VaraniaFS — собственный постоянный формат RustOS. Его структуры вынесены в
отдельный `no_std` crate [`varaniafs`](../varaniafs/src/lib.rs): один и тот же
код использует ring-3 `vfsd` и host-утилита `rustos-vfs-image` на macOS/Linux.
Kernel формат не разбирает.

## Разметка тома

Логический блок равен 4096 байтам, все номера блоков и размеры тома имеют тип
`u64`.

| Блоки | Назначение |
|---|---|
| `0..2` | две копии superblock |
| `2..18` | metadata slot A, 16 блоков |
| `18..34` | metadata slot B, 16 блоков |
| `34..` | данные файлов и свободные extent'ы |

Superblock содержит magic `VARNFS1`, версию, размер блока, полный размер
тома, номер активного metadata slot, sequence и CRC32 snapshot. Metadata
содержит inode, пути, extent maps и allocator state. Все структуры имеют
compile-time проверки размера.

## Commit без повреждения старой версии

Изменение метаданных фиксируется в строгом порядке:

1. записать изменённые data blocks;
2. выполнить device flush;
3. сериализовать полный snapshot в неактивный metadata slot;
4. выполнить device flush;
5. записать новый superblock с увеличенным sequence и CRC snapshot;
6. выполнить device flush.

Mount проверяет обе копии superblock и CRC соответствующего metadata slot,
после чего выбирает валидную пару с наибольшим sequence. Если последний
commit оборван, предыдущая пара остаётся доступной. Это обеспечивает
атомарность метаданных без журнала и хорошо видно в учебном коде.

## Allocator и операции

Файлы представлены extent'ами `(logical, physical, blocks)`. Последовательная
потоковая запись обычно добавляет один большой extent и хорошо подходит SSD и
современным HDD. Удаление возвращает диапазоны в bounded free-extent table;
новая запись сначала повторно использует их, затем растит линейный cursor.
Sparse layout уже выражается полем `logical`, хотя публичный API пока не
создаёт holes.

Путь inode нормализован и абсолютен. Для учебного v1 это делает lookup и
проверку rename очень прозрачными. Переименование каталога обновляет пути всех
потомков в одном metadata snapshot.

## Ограничения v1

64-битная адресация не означает, что текущая metadata table уже эффективна
на терабайтном рабочем томе. Версия 1 намеренно ограничена:

- 64 inode;
- 32 extent'а на inode;
- 64 записи свободных extent'ов;
- 192 байта на полный путь;
- один writer (`vfsd`) и синхронные block requests;
- CRC защищает метаданные, но не содержимое data blocks.

Формат v2 заменит линейные bounded таблицы checksummed copy-on-write B-деревьями
для directory index, inode table, extent map и free space. Данные получат
optional checksums; allocator — SSD-aware discard и HDD-friendly placement.
Номер версии на диске запрещает молча интерпретировать v2 как v1.

## Host-инструмент

Создать том или проверить существующий:

```bash
cargo run -p rustos-vfs-image -- build/system.vfs 64
cargo run -p rustos-vfs-image -- --verify build/system.vfs
cargo run -p rustos-vfs-image -- --put build/system.vfs ./app.dll /system/lib/app.dll
cargo run -p rustos-vfs-image -- --force build/system.vfs 64
```

Последняя команда уничтожает содержимое указанного образа и предназначена
только для воспроизводимых тестов. `--put` выполняет copy-on-write замену:
старые extent'ы освобождаются в новой metadata snapshot только после записи
новых data blocks. `ls/get/fsck` будут следующим расширением этой же утилиты;
mounted образ нельзя менять с host одновременно с запущенной VM.

# VaraniaFS

VaraniaFS — единственная поддерживаемая постоянная файловая система RustOS.
Она рассчитана на SSD и флеш-накопители, но сохраняет последовательный I/O и
крупные extents, полезные для HDD. Все адреса и размеры 64-битные; формат не
накладывает практического ограничения в несколько TiB.

## Гарантии

- metadata хранится в checksummed copy-on-write B+tree;
- каждый metadata-узел имеет физическую зеркальную копию;
- пользовательские данные проверяются detached CRC32C checksums;
- два superblock сохраняют последнюю и предыдущую recovery point;
- короткий mirrored intent log делает `fsync` durable до фонового checkpoint;
- torn write не публикует частично готовое поколение;
- `scrub` восстанавливает одну повреждённую metadata-копию из второй;
- offline `fsck` ничего не записывает и проверяет деревья, ссылки и данные;
- snapshots фиксируют набор immutable корней без копирования data blocks;
- sparse-файлы читаются нулями и занимают блоки только после записи.

Filesystem намеренно не содержит Unix permissions. Доступ выдаёт capability
слой микроядра и `vfsd`; отсутствие uid/mode не означает отсутствие изоляции.

## Дисковая схема

Логический блок равен 4096 байтам. В начале тома находятся:

1. две копии superblock;
2. 16 mirrored слотов intent log;
3. шесть COW-деревьев: inode, directory, extent, free-space/reverse-map,
   checksum и snapshot;
4. metadata/data allocation area.

Metadata allocator выдаёт чётные пары `(primary, mirror)`. Копия содержит тот
же logical `self_block`, поэтому parent pointer стабилен и recovery может
переключиться на зеркало без переписывания дерева.

Ключи числовых объектов кодируются big-endian, поля значений — little-endian.
Parser не делает pointer cast дисковых байтов в Rust-структуры и строго
проверяет длины, ordering, ranges, reserved bits, UUID и CRC32C.

## Порядок транзакции

```text
новые data blocks
flush (если были data)
новые primary+mirror metadata nodes
flush
primary+mirror intent record
flush                 <- fsync уже можно подтвердить
новый superblock
flush                 <- checkpoint завершён
```

До последнего шага предыдущий superblock остаётся валидным. Mount выбирает
самую новую полностью проверяемую точку среди superblock и intent log. Запись
журнала с отсутствующим, torn или повреждённым корнем отбрасывается целиком.

## B+tree и allocator

`varaniafs::tree::Transaction` реализует exact lookup, ordered traversal,
insert/upsert/remove, split, cascade merge и collapse корня. Узлы immutable;
изменение строится снизу вверх и публикуется одной сменой RootSet.
Большой bounded набор ещё не опубликованных блоков вынесен в переиспользуемый
`TransactionWorkspace`: `vfsd` хранит его в BSS, а не на ring-3 stack, и не
делает heap allocation на каждый запрос.

`varaniafs::allocator::BlockAllocator` хранит bounded cache свободных extents,
объединяет соседей, выбирает best-fit с alignment и не теряет alignment gaps.
Полная карта пространства хранится в space tree. Старые COW-блоки нельзя
переиспользовать, пока superblock нового поколения не стал durable и пока на
них ссылается snapshot.

## Файлы и каталоги

Inode не хранит абсолютный путь. Directory tree связывает `(parent, name)` с
object id, поэтому rename каталога меняет одну запись, а не всех потомков.
Имена — произвольные bytes без NUL и `/`; UI и стандартная библиотека используют
UTF-8. Это упрощает портирование Unix-программ без привязки формата к locale.

`varaniafs::file` выполняет потоковые partial/multi-block read/write. Partial
write сначала собирает полный новый блок, затем меняет extent и checksum COW.
После shrink хвост последнего блока обнуляется, поэтому shrink→grow не раскрывает
старые данные.

## Host-команды

```bash
cargo run -p rustos-vfs-image -- build/system.vfs 1024
cargo run -p rustos-vfs-image -- --grow build/system.vfs 2048
cargo run -p rustos-vfs-image -- --put build/system.vfs ./app.rune /system/bin/app.rune
cargo run -p rustos-vfs-image -- --verify build/system.vfs
cargo run -p rustos-vfs-image -- --fsck build/system.vfs
cargo run -p rustos-vfs-image -- --scrub build/system.vfs
```

Создание использует temporary file, `sync_all`, проверку нового образа, atomic
rename и sync parent directory. Существующий экспериментальный developer-образ
автоматически импортируется с сохранением recoverable backup; новые компоненты
не монтируют экспериментальный layout.

## Изоляция

Только ring-3 `vfsd` получает block capability. Клиенты вызывают `vfs.dll`,
передавая control plane через capability IPC, а данные — через shared memory.
Падение или отказ parser завершает/перезапускает сервис, но не микроядро.

## Проверки

```bash
cargo test -p varaniafs -p rustos-vfs-image
cargo clippy -p varaniafs -p rustos-vfs-image --all-targets -- -D warnings
cargo run -p rustos-vfs-image -- --fsck build/system.vfs
```

Unit-набор покрывает split/merge после сотен ключей, зеркальное восстановление,
intent recovery до checkpoint, sparse streaming, checksum corruption, snapshots,
namespace rename/unlink и allocator coalescing. Stress-набор дополнительно
перебирает точки потери питания и массовые случайные повреждения копий.

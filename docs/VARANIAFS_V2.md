# VaraniaFS v2: надёжная файловая система RustOS

## Цель и честная гарантия

VaraniaFS v2 предназначена для систем от Raspberry Pi с 128 МиБ RAM до
рабочих станций и многотерабайтных SSD. Размер диска, число inode и размер
файла не определяют объём обязательной оперативной памяти: cache и transaction
budgets ограничены выбранным runtime profile.

Файловая система гарантирует, что после подтверждённого `fsync` и внезапного
отключения питания mount увидит целиком старое либо целиком новое поколение.
Она не возвращает данные с неверной checksum как корректные. При единственной
физической копии и необратимом отказе носителя восстановить каждый байт
математически невозможно; для автоматического ремонта нужны metadata mirror,
data replica либо второй device.

Capability policy остаётся над filesystem. On-disk inode не содержит Unix
owner/group/mode и executable bit. Loader определяет RUNE/DLL по проверенному
заголовку, ABI и подписи контейнера; расширение файла служит только подсказкой
UI. Это не мешает хранить обычные byte-oriented имена, symbolic links и
POSIX-подобную семантику I/O, облегчающую портирование Linux software.

## Текущий статус реализации

Первый безопасный слой уже находится в crate `varaniafs::v2`: явный endian
codec superblock/node, CRC32C, UUID/address/generation validation, slotted
bounded nodes, шесть typed roots, detached checksum и reverse-map records,
выбор последнего целого поколения и rollback при torn/missing root. Unit tests
перебирают power cut перед публикацией каждого root.

Host tool умеет создать и проверить отдельный экспериментальный образ:

```bash
cargo run -p rustos-vfs-image -- \
  --create-v2 build/varania-v2.vfs 1024 00112233445566778899aabbccddeeff
cargo run -p rustos-vfs-image -- --verify-v2 build/varania-v2.vfs
```

Команда требует UUID явно, поэтому одинаковые входы дают детерминированный
образ, а владелец image контролирует identity тома. Runtime `vfsd` пока
монтирует стабильный v1: tree mutation/split/merge, metadata mirrors, intent
log, scrub и migration ещё не объявлены готовыми.

## Почему не копия ZFS, ext4 или F2FS

- Полный in-place metadata journal, как JBD2, хорошо защищает согласованность,
  но удваивает metadata writes и сам требует сложного replay.
- Чистый log-structured layout хорошо подходит flash, но может создавать
  непредсказуемый foreground GC и высокую write amplification при заполнении.
- ZFS-подобный COW и end-to-end checksums дают нужную цепочку доверия, однако
  VaraniaFS не копирует ARC и не связывает размер cache с размером диска.
- Подход littlefs с power-loss-safe metadata и bounded RAM полезен для SD/eMMC,
  но его структуры рассчитаны на существенно меньшие носители и workloads.

Поэтому v2 использует COW B+tree, небольшой optional intent log для sync
latency и flash-aware extent allocator. Journal ускоряет `fsync`, но не
является единственной копией metadata и не нужен для восстановления
checkpoint.

## Дисковая модель

Логический блок равен 4096 байтам. Все физические номера, object id, logical
offsets, sizes и generations имеют тип `u64`. Числовые части B+tree keys
записаны big endian, чтобы byte ordering совпадал с числовым; остальные поля
кодируются little endian явно, без pointer cast структуры с диска.

Superblock публикует одно поколение корней:

1. **inode tree** — type, size, timestamps и policy hints объекта;
2. **directory tree** — `(parent object, byte name) -> child object`;
3. **extent tree** — `(object, logical block) -> physical extent/replica`;
4. **checksum tree** — checksums небольших диапазонов data blocks;
5. **space tree** — свободные диапазоны, reference count и reverse mapping;
6. **snapshot tree** — именованные и automatic safety checkpoints.

Metadata block является self-describing: содержит magic/version, filesystem
UUID, собственный физический адрес, tree kind, generation, owner и checksum.
Это обнаруживает не только torn write, но и запись корректного блока не по тому
адресу. Parser проверяет slot ranges, отсутствие overlap, строгий порядок keys,
child bounds и record invariants до публикации node в cache.

Metadata по умолчанию имеет две копии, размещённые в разных allocation groups.
Для data доступны три reliability class без связи с правами доступа:

- `checksummed` — одна копия и обязательная checksum;
- `replicated` — две копии на разных failure domains, если device topology это
  позволяет;
- `ephemeral` — одна копия без долгой snapshot retention для cache/build
  artifacts, но metadata всё равно checksummed.

System binaries, DLL и filesystem metadata используют `replicated` по
умолчанию. Потеря одной копии запускает bounded repair и health event.

## Атомарная транзакция

Обычный checkpoint выполняется только в таком порядке:

1. выделить новые blocks, не переиспользуя достижимые из старого поколения;
2. записать новые data blocks и их checksums;
3. выполнить flush либо FUA и проверить completion всех requests;
4. записать новые COW metadata nodes и зеркала;
5. выполнить flush;
6. одним checksummed block опубликовать следующую копию superblock;
7. выполнить flush;
8. только теперь разрешить reclamation блоков, недостижимых из retained roots.

При ошибке любого шага новый superblock не публикуется. Mount проверяет обе
копии и выбирает наибольшее поколение, у которого валидны root nodes. Sequence
сравнивается с учётом переполнения. Устройство, которое ложно подтверждает
flush/FUA, должно помечаться degraded: программно доказать durability поверх
такого hardware невозможно.

Intent log хранит только checksummed, idempotent операции для малой задержки
`fsync`: create/link/rename/truncate и привязку уже записанного data extent.
После group checkpoint log можно отбросить. Повреждённый хвост журнала
игнорируется; последняя валидная запись никогда не отменяет предыдущий
checkpoint. Полный data journaling по умолчанию не используется.

## Защита от тихой порчи

- Metadata всегда имеет checksum, UUID, address, owner и generation.
- Data checksums включены по умолчанию и хранятся отдельно от проверяемого
  data block. Формат поддерживает алгоритм и длину digest; baseline использует
  CRC32C для metadata, strong digest — для пользовательских данных.
- Tree checker работает перед записью и после чтения. Ошибка на write path не
  попадает на диск; непоправимая metadata ошибка переводит volume в read-only.
- Online scrub обходит data/metadata с ограничением bandwidth. При наличии
  корректной replica он ремонтирует повреждённую копию COW-записью.
- Space/reverse map сверяется с extent tree. Deep `fsck` может восстановить
  allocator state, найти orphan objects и сохранить их в recovery namespace,
  не выполняя опасный repair наугад.

Checksum не заменяет replica: она надёжно обнаруживает порчу, но для ремонта
нужна ещё одна корректная копия.

## Производительность и носители

Общая fast path использует extent'ы, delayed allocation, group commit и
асинхронные очереди I/O. Маленькая транзакция копирует только путь изменённых
B+tree nodes, а не полный metadata snapshot.

### SSD, NVMe, eMMC и SD

- основной профиль оптимизирован под non-rotational device;
- allocator группирует последовательные writes и разделяет hot metadata от
  cold data, уменьшая write amplification;
- discard/TRIM отправляется пакетно только после durable commit и никогда не
  участвует в корректности;
- geometry/erase hints являются оптимизацией: неизвестному USB/SD controller
  нельзя доверять точный внутренний erase size;
- частые маленькие sync requests объединяются intent log, но `fsync` сохраняет
  строгую durability семантику.

### HDD

- большие последовательные extent'ы и locality одного каталога/файла;
- background cleaning и scrub работают с низким priority;
- allocator избегает flash-style постоянного перемещения cold extents;
- поведение остаётся корректным без discard и device write-zeroes.

### RAM profiles

Cache не является частью on-disk correctness и может быть освобождён в любой
момент:

- `embedded`: 128 МиБ RAM, фиксированный metadata cache 4–8 МиБ и bounded
  transaction 1 МиБ;
- `desktop`: adaptive cache с жёстким верхним пределом, отдельно от process
  memory pressure;
- `builder`: увеличенные cache/transaction budgets для Rust toolchain, но тот
  же on-disk формат.

Ни один алгоритм mount, lookup, iteration, scrub cursor или transaction commit
не требует RAM, пропорциональной размеру всего volume.

## Snapshot и удаление

COW roots позволяют дешёвые snapshots, но бесконечное хранение старых
поколений привело бы к заполнению диска. Политика retention находится в
`vfsd`: небольшой набор recent safety checkpoints, явные пользовательские
snapshots и pressure-based reclamation. Корзина рабочего стола остаётся
пользовательской функцией; filesystem snapshot защищает от ошибочного удаления
и повреждённой транзакции независимо от GUI.

## Проверки до переключения с v1

V2 не станет форматом по умолчанию, пока не пройдёт:

1. unit/fuzz tests каждого decoder и tree invariant;
2. power-cut после каждой block write, flush и superblock publication;
3. differential model tests create/write/rename/delete/snapshot;
4. corruption injection: bit flip, lost write, misdirected write, stale DMA;
5. mount/fsck/scrub при повреждении каждой metadata replica;
6. stress с миллионами directory entries, sparse/fragmented files и volume
   больше 1 ТиБ;
7. QEMU persistence test на AMD64 и AArch64;
8. host migration v1 -> v2 с digest comparison и сохранением исходного образа.

## Материалы, повлиявшие на проектирование

- [Btrfs checksumming](https://btrfs.readthedocs.io/en/stable/Checksumming.html)
  и [tree checker](https://btrfs.readthedocs.io/en/latest/Tree-checker.html);
- [Linux F2FS design](https://docs.kernel.org/filesystems/f2fs.html);
- [ext4/JBD2 journal ordering](https://docs.kernel.org/filesystems/ext4/journal.html);
- [XFS self-describing metadata и online fsck](https://docs.kernel.org/filesystems/xfs/xfs-online-fsck-design.html);
- [OpenZFS end-to-end checksums](https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Data%20Storage/Checksums.html);
- [littlefs power-loss и bounded-RAM design](https://github.com/littlefs-project/littlefs/blob/master/DESIGN.md);
- [bcachefs: COW filesystem as database](https://bcachefs.org/).

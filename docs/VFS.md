# VFS как изолированный сервис

## Что уже исполняется

Файловая система больше не является RAM overlay ядра. При загрузке process
manager создаёт отдельный ring-3 процесс `vfsd.rune` и передаёт только ему:

- `RECEIVE` capability на служебный IPC endpoint;
- `READ | WRITE` capability на системное блочное устройство.

Приложение получает `SEND` endpoint `vfsd` и собственный reply endpoint. У
него нет block capability и доступа к метаданным диска. `vfs-1.dll` создаёт
одно переиспользуемое 64-КиБ shared-memory окно, передаёт производную
capability в каждом data-запросе и скрывает формат IPC от приложения.

```text
application
    |
    | open/read/write/seek/resize/readdir/mkdir/unlink/rename/sync
    v
vfs-1.dll                 stable C ABI + safe Rust facade
    |
    | bounded capability IPC; file data in shared memory
    v
vfsd.rune (ring 3)        paths, open descriptions, VaraniaFS allocator
    |
    | 4-KiB block syscalls; capability checked by kernel
    v
virtio-blk bootstrap      build/system.vfs, persistent between boots
```

`vfsd` связывает каждый непрозрачный `VfsObject` с настоящим `sender_pid`,
который заполняет kernel. Число, украденное у другого процесса, нельзя
использовать как открытый файл. После падения сервиса kernel и остальные
процессы продолжают работать; следующий шаг supervisor — создать новый
endpoint, перезапустить `vfsd` и переиздать клиентские capabilities.

## API версии 2

Исполняемый wire ABI находится в [`abi/src/vfs.rs`](../abi/src/vfs.rs), а
клиент — в [`libs/vfs/src/lib.rs`](../libs/vfs/src/lib.rs). Реализованы:

| Операция | Семантика |
|---|---|
| `open` / `close` | `READ`, `WRITE`, `CREATE`, `EXCLUSIVE`, `TRUNCATE`, `APPEND`, `DIRECTORY` |
| `read` / `write` | потоковые операции с текущей позицией файла |
| `seek` | от начала, текущей позиции или конца файла |
| `resize` | shrink с возвратом блоков или sparse grow с нулевыми holes |
| `readdir` | по одной 256-байтной записи, без загрузки каталога целиком |
| `mkdir` | создание каталога после проверки родителя |
| `unlink` | удаление файла или пустого каталога и возврат extent'ов |
| `rename` | переименование файла; для каталога также меняются пути потомков |
| `sync` | flush данных и committed metadata |

Пути и содержимое файлов не помещаются в 64-байтный inline payload IPC.
Клиент кладёт их в shared window; control message передаёт только offset и
length. `read`/`write` автоматически разбиваются на chunks по 64 КиБ, поэтому
размер файла не ограничен размером сообщения или окна.

У самостоятельного `VfsClient` один синхронный запрос в полёте. Порт upstream
`std::fs` сериализует короткие RPC одного процесса и переиспользует одно
64-КиБ окно; заблокированный VFS вызов усыпляет только вызывающий thread.
Wire protocol остаётся пригоден для будущего pool из нескольких клиентов без
общего mutable server state в приложении.

## Постоянный том

`scripts/build.sh` создаёт `build/system.vfs` размером не менее 1 ГиБ и затем
сохраняет его между интерактивными запусками. Старый 64-МиБ том расширяется
транзакционно командой `--grow`; sparse host file занимает только реально
записанные блоки. QEMU подключает образ отдельным
legacy virtio-blk устройством в фиксированном PCI slot 5. ESP остаётся
read-only загрузочным диском и никогда не принимается за системный том.

Формат диска и протокол восстановления описаны в
[`docs/VARANIAFS.md`](VARANIAFS.md). Host-команда:

```bash
cargo run -p rustos-vfs-image -- build/system.vfs 1024
cargo run -p rustos-vfs-image -- --grow build/system.vfs 1024
cargo run -p rustos-vfs-image -- --verify build/system.vfs
```

Первая команда не перезаписывает существующий образ. Для заведомо чистого
тестового тома используется явный `--force`.

## Проверяемый сценарий загрузки

Boot-test запускает два разных клиента с полным завершением `vfsd` между
ними:

1. первый клиент создаёт `/tmp/vfsd-test`, потоково пишет файл 70 000 байт,
   делает `seek`, читает и сравнивает данные, переименовывает файл и находит
   его через `readdir`;
2. `sync` фиксирует том, после чего процесс `vfsd` завершается и его address
   space освобождается;
3. новый `vfsd` заново читает superblock и metadata с virtio-blk;
4. второй клиент открывает переименованный файл, проверяет размер и последний
   байт, удаляет файл и каталог, выполняет `sync`.

Успех отмечается строками serial log:

```text
[vfsd] open/read/write/seek/readdir/create/rename over shared memory verified
[vfsd] restart recovered committed VaraniaFS metadata and file data
```

Это тестирует не только API, но и отсутствие зависимости от памяти первого
server process.

Дополнительно boot-test запускает RUNE-программу с настоящей upstream `std`.
Она проверяет CWD и относительные пути, `File::set_len`, sparse нули,
`Read/Write/Seek`, metadata, canonicalize, readdir, rename и recursive cleanup
через ту же границу `std -> shared memory IPC -> vfsd`.

## Честные границы текущего этапа

- `vfs-1.rune` уже содержит manifest-backed interface ABI и проверяется
  нативным resolver'ом. `std::fs` пока компилирует тот же client protocol в
  PAL статически; перевод PAL на вызовы общей DLL уберёт это дублирование.
- Legacy virtio-blk transport временно находится в kernel. После появления
  PCI, DMA и IRQ capabilities он переедет в изолированный `virtioblkd`, не
  меняя VFS ABI.
- VaraniaFS v1 имеет 64-битные номера блоков, но bounded таблицы v1 рассчитаны
  на 64 inode, 32 extent'а на inode и 64 свободных extent'а. Масштабируемые
  B-деревья/extent trees относятся к формату v2.
- Две checksummed копии метаданных защищают commit. Данные файла сейчас
  пишутся in-place и не имеют checksum/COW, поэтому torn sector в data block
  пока может испортить содержимое при сохранённых метаданных.
- `std::process::Command` прозрачно направляет небутовый executable path через
  `rune-runner`; runner и native resolver читают target/DLL из VaraniaFS без
  parser'а диска в kernel.

В интерактивной сборке `vfsd` не завершается после boot milestones. Команда
GUI terminal `RUN /apps/examples/hello.rune student` создаёт runner-процесс,
передаёт ему VFS/stdout/stderr capabilities, дренирует pipe при заполнении и
показывает exit status. Это переходный console bridge; постоянная policy
перезапуска и владение сессией должны переехать в user-space supervisor.

## Направление развития

`vfsd` останется namespace/cache service, а конкретные filesystem и block
драйверы будут отдельными процессами:

```text
shell / editor / rustc
          |
       vfs-1.dll
          |
         vfsd
       /      \
varaniafsd   initramfsd
     |
   blockd
  /     \
NVMe   AHCI/virtio
```

Так terminal, editor и будущий `std::fs` используют один API, а парсер
файловой системы и аппаратный драйвер можно перезапускать независимо.

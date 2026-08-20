# VFS и файловые утилиты RustOS

## Статус

Уже работает ранний вертикальный срез: kernel монтирует RIFS initramfs
read-only в `/boot`, создаёт volatile RAM overlay для `/system`, `/home`,
`/src` и `/build`, а terminal выполняет основные файловые команды. Он нужен,
чтобы проверять команды и path semantics до ring 3. Финальная граница ниже —
`vfsd` и драйверы как отдельные процессы; bootstrap backend будет удалён из
kernel после их запуска.

## Слои

```text
shell / editor / rustc / applications
             |
          vfs-1.dll
             |
     capability IPC + shared buffers
             |
            vfsd                 namespace, cwd/root, mounts, cache policy
          /      \
  varaniafsd   initramfsd        filesystem format, journal, directories
       |
    blockd                     queues, flush/FUA, TRIM, partitions
       |
 virtio-blk / NVMe / AHCI      user-space drivers with device capabilities
```

Kernel не знает каталогов, имён и filesystem format. Он предоставляет
процессы, memory mapping, IPC, IRQ и capabilities на MMIO/ports/DMA. Поэтому
падение parser'а файловой системы не останавливает scheduler или GUI, а
supervisor может перезапустить сервис.

## Единый API

Terminal и Rust-программа используют один и тот же ABI из
[`abi/src/vfs.rs`](../abi/src/vfs.rs). Shell не вызывает код драйвера и не
имеет скрытого «особого» пути. `vfs.dll` предоставляет функции наподобие:

```text
vfs_open_at(directory, path, flags) -> file handle
vfs_read(file, shared_buffer, offset) -> bytes
vfs_write(file, shared_buffer, offset) -> bytes
vfs_read_dir(directory, cookie, buffer) -> entries
vfs_mkdir_at(directory, path)
vfs_unlink_at(directory, path)
vfs_rename_at(old_directory, old_path, new_directory, new_path)
vfs_sync(file_or_mount)
```

Current directory — handle каталога в runtime процесса, а не строка в kernel.
Это убирает race между `chdir` и path lookup. Отдельный process root handle
позволит позже делать sandboxes, даже если система не вводит Unix uid/sudo.

## Команды

Лучший компромисс для учебной системы — multicall-программа `/system/bin/fs`,
а не большой shell с копией VFS и не десятки крошечных бинарников на первом
этапе:

```text
fs ls [path]          fs cat <file>       fs stat <path>
fs mkdir <path>       fs touch <file>     fs write <file> [text]
fs cp <src> <dst>     fs mv <src> <dst>   fs rm <path>
fs find <path>        fs sync [path]
```

Имена-апплеты `ls`, `cat`, `mkdir` могут быть маленькими links/manifests на
тот же ELF, поэтому привычные команды работают без дублирования кода. `cd`
обязан оставаться builtin shell: отдельный процесс не может поменять cwd
родителя. Полноэкранный editor является отдельным приложением и использует
тот же `vfs.dll`.

## Потоковая запись и надёжность

API не требует держать файл целиком в RAM. `rustc`, editor и copy utility
читают/пишут окнами, а `vfsd` применяет backpressure. Семантика durability:

- обычный write может оставаться в cache;
- `vfs_sync(file)` фиксирует данные и метаданные файла;
- atomic replace: write temporary -> sync -> rename -> sync directory;
- blockd реализует flush/FUA и не сообщает commit раньше устройства;
- discard/TRIM передаётся только после того, как блоки больше не нужны
  recovery.

VaraniaFS будет copy-on-write/checksummed с transaction commit, но VFS ABI от
конкретного on-disk format не зависит. Initramfs и RAM overlay реализуют тот
же protocol и служат ранним bootstrap.

## Обмен с macOS/Linux

Основной способ — `rustos-disk`, host tool с тем же parser'ом VaraniaFS:
`ls/get/put/mkdir/fsck`, работающий с выключенным образом. Для живой VM будет
отдельный virtio-serial file-transfer service; host никогда не изменяет
mounted image за спиной filesystem driver.

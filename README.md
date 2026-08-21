# RustOS

RustOS — учебная 64-битная микроядерная операционная система на Rust.
Рабочие эталонные платформы — AMD64 (GRUB/Multiboot2) и AArch64 (QEMU `virt`
+ AAVMF). Обе запускают изолированные пользовательские RUNE-процессы,
вытеснение, IPC, VFS и dynamic loader; CPU-зависимый код изолирован для
будущих Raspberry Pi и других ARMv8-A платформ.

Текущий рабочий вертикальный срез:

- GRUB/Multiboot2 boot на AMD64 и UEFI loader на Rust для AArch64;
- ELF64 PIE kernel и initramfs;
- собственные GDT/TSS/IDT, NX/WP и отдельный ring-0 trap stack;
- 64-битная карта памяти без ограничения 4 ГиБ;
- реальное резервирование kernel pages через UEFI `AllocatePages`;
- разрежённые четырёхуровневые page tables;
- extent allocator физических кадров, служебный размер которого не зависит
  от объёма RAM;
- настоящий ELF64 PIE-процесс в CPL3 с отдельным CR3, W^X-страницами и
  process-local VFS capability;
- локализация user exception: намеренный `#UD` завершает только процесс, а
  все его data/stack/page-table frames возвращаются allocator'у;
- local APIC timer и настоящее вытеснение нескольких независимых CPL3
  контекстов без добровольного `yield`;
- ACPI MADT discovery и INIT–SIPI–SIPI запуск AP-ядер через 16/32/64-битный
  trampoline; AP пока безопасно parked до per-CPU TSS/IDT;
- dynamic create/exit/reap с generation-safe PID/TID и полным reclaim;
- capability ABI v2: ring-3 `spawn/wait/kill`, несколько потоков с
  `create/join`, argv/environment, FS-base/TPIDR TLS и monotonic clock;
- anonymous `map/unmap/protect` с W^X и shared-memory objects с раздельным
  учётом capability/mapping references;
- bounded endpoint IPC: block/wake, kernel-supplied sender PID, FIFO queue и
  передача только ослабленных capabilities;
- host-tested scheduler core: CPU affinity, приоритетные классы и bounded
  driver priority;
- GOP 1280×800×32, CPU rendering и RAM back buffer без мерцания сцены;
- PS/2 keyboard и mouse;
- desktop, taskbar, Start menu и icons;
- window manager: drag, minimize, maximize, restore и close;
- цветной terminal с командами `help`, `clear`, `about`, `mem`, `gui`,
  `echo` и `shutdown`;
- bootstrap VFS: read-only RIFS initramfs в `/boot` и writable RAM overlay;
- файловый workflow в terminal: `pwd`, `cd`, `ls`, `cat`, `mkdir`, `touch`,
  `write`, `append`, `rm` и `stat` (также через multicall-префикс `fs`);
- versioned ABI для capability handles, IPC, VFS и ELF64 `.dll` metadata;
- архитектурный HAL с AMD64/AArch64 trap context, MMU, syscall и timer
  контрактами; обязательная cross-сборка kernel и всего bootstrap user-space;
- UI-компоненты: panel, label, button, icon button, checkbox, radio button,
  toggle, text edit, scroll/list view, tabs и image surface;
- настоящие QEMU boot/keyboard/mouse/framebuffer integration tests.

## Быстрый запуск

macOS Apple Silicon:

```sh
brew install qemu
make bootstrap
make build
make run
```

Debian/Ubuntu:

```sh
sudo apt install qemu-system-x86 qemu-system-arm ovmf
make bootstrap
make build
make run
```

ARM (AArch64, QEMU `virt` + UEFI):

```sh
make build-arm      # kernel/RUNE/initramfs/VaraniaFS/AAVMF/ESP
make run-arm        # интерактивный QEMU virt; serial в терминале
make test-arm-boot  # headless EL0/EL1/GICv3/PSCI integration test
```

Firmware для ARM (AAVMF) уже закоммичен в `firmware/aarch64/`; сетевая
загрузка не требуется. Полный ESP находится в `build/arm/esp-arm.img`,
канонический загрузчик — `EFI/BOOT/BOOTAA64.EFI`, persistent диск —
`build/arm-system.vfs`.

`make run` открывает графическое окно QEMU. Клавиатура сразу направлена в
terminal. Мышью можно перемещать окно и использовать кнопки заголовка.

## Проверка

```sh
make lint
make test-host
make test-arch
make test
```

`make test` выполняет пять разных сценариев:

1. Host unit tests проверяют ABI, scheduler, video и инструменты образов.
2. Cross-build создаёт AMD64 и AArch64 ELF ядра, runtime и приложений.
3. AMD64 boot-test завершается настоящим `isa-debug-exit`.
4. AArch64 boot-test проходит AAVMF, GICv3 preemption, PSCI SMP, EL0 fault
   containment и завершается через `PSCI SYSTEM_OFF`.
5. GUI-тест через настоящий PS/2 выполняет запись/чтение файлов и работу с
   каталогами, перетаскивает и сворачивает terminal, получает QEMU
   screendump и проверяет геометрию и пиксели.

Диагностика сохраняется в `build/test-results/`.

## Архитектурный статус

Обязательный первый рубеж микроядра пройден: QEMU действительно выполняет
`system/bin/init.rune` в CPL3/EL0. Процесс получает только read-only handle корня
bootstrap VFS, выполняет syscall, завершается, а второй тестовый ELF вызывает
`UD2`; ядро перехватывает fault, освобождает address space и продолжает boot.

GUI пока остаётся bootstrap-сеансом CPU0 внутри kernel image. До его запуска
process manager выполняет настоящую preemptive-сессию: timer IRQ сохраняет
user context, scheduler выбирает другой TID, меняется CR3/TTBR0, а
`iretq`/`eret` возвращает уже другой процесс. На обеих ISA второй CPU реально
запускается и подтверждает online, но пока parked: следующий рубеж — per-CPU
runtime, interrupt routing, TLB shootdown и work stealing. Затем display/input
и desktop переносятся в изолированные процессы. Нативный `rustc` ещё не
заявлен готовым.

Подробнее: [архитектуры CPU](docs/ARCHITECTURES.md), [архитектура системы](docs/ARCHITECTURE.md),
[графическая подсистема](docs/GUI.md), [видеосистема](docs/VIDEO.md), [VFS](docs/VFS.md),
[микроядро](docs/MICROKERNEL.md), [DLL](docs/DYNAMIC_LIBRARIES.md),
[IPC](docs/IPC.md), [процессы и память ABI v2](docs/PROCESS_MEMORY_ABI.md),
[self-hosting Rust](docs/SELF_HOSTING.md),
[сборка](docs/BUILDING.md).

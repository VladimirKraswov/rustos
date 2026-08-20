# RustOS

RustOS — учебная 64-битная операционная система на Rust для современных
x86-64 компьютеров. Она загружается через UEFI, получает GOP framebuffer и
показывает собственный рабочий стол с оконным менеджером и terminal.

Текущий рабочий вертикальный срез:

- UEFI/OVMF bootloader на Rust;
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
sudo apt install qemu-system-x86 ovmf
make bootstrap
make build
make run
```

`make run` открывает графическое окно QEMU. Клавиатура сразу направлена в
terminal. Мышью можно перемещать окно и использовать кнопки заголовка.

## Проверка

```sh
make lint
make test-host
make test
```

`make test` выполняет два разных сценария:

1. UEFI boot-test завершается настоящим `isa-debug-exit`.
2. GUI-тест через настоящий PS/2 выполняет запись/чтение файлов и работу с
   каталогами, перетаскивает и сворачивает terminal, получает QEMU
   screendump и проверяет геометрию и пиксели.

Диагностика сохраняется в `build/test-results/`.

## Архитектурный статус

Обязательный первый рубеж микроядра пройден: QEMU действительно выполняет
`system/bin/init.elf` в CPL3. Процесс получает только read-only handle корня
bootstrap VFS, выполняет syscall, завершается, а второй тестовый ELF вызывает
`UD2`; ядро перехватывает fault, освобождает address space и продолжает boot.

GUI пока остаётся bootstrap-сеансом CPU0 внутри kernel image. До его запуска
process manager выполняет настоящую preemptive-сессию: timer IRQ сохраняет
user context, scheduler выбирает другой TID, меняется CR3, а `iretq`
возвращается уже в другой процесс. Второй CPU запускается и подтверждает APIC
ID, но пока parked: без отдельного TSS/IDT/interrupt stack выдавать ему user
thread небезопасно. Следующий рубеж — per-CPU runtime, IOAPIC/IRQ routing и
work stealing, затем `vfsd`, display/input и desktop переносятся в
изолированные процессы. Нативный `rustc` ещё не заявлен готовым.

Подробнее: [архитектура](docs/ARCHITECTURE.md),
[графическая подсистема](docs/GUI.md), [VFS](docs/VFS.md),
[микроядро](docs/MICROKERNEL.md), [DLL](docs/DYNAMIC_LIBRARIES.md),
[IPC](docs/IPC.md), [self-hosting Rust](docs/SELF_HOSTING.md),
[сборка](docs/BUILDING.md).

# RustOS

RustOS — учебная 64-битная операционная система на Rust для современных
x86-64 компьютеров. Она загружается через UEFI, получает GOP framebuffer и
показывает собственный рабочий стол с оконным менеджером и terminal.

Текущий рабочий вертикальный срез:

- UEFI/OVMF bootloader на Rust;
- ELF64 PIE kernel и initramfs;
- 64-битная карта памяти без ограничения 4 ГиБ;
- реальное резервирование kernel pages через UEFI `AllocatePages`;
- разрежённые четырёхуровневые page tables;
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

GUI сейчас является ранним bootstrap-сеансом CPU0 внутри kernel image. Это
осознанный вертикальный срез для проверки framebuffer, input и UI API до
переноса display/input/window services в ring 3. Следующий milestone —
процессы ring 3, preemptive scheduler и реализация уже описанных
capabilities/IPC. Нативный `rustc` ещё не заявлен готовым: точный bootstrap и
его системные предпосылки описаны отдельно, чтобы cross-сборку нельзя было
случайно принять за self-hosting.

Подробнее: [архитектура](docs/ARCHITECTURE.md),
[графическая подсистема](docs/GUI.md), [VFS](docs/VFS.md),
[DLL](docs/DYNAMIC_LIBRARIES.md), [self-hosting Rust](docs/SELF_HOSTING.md),
[сборка](docs/BUILDING.md).

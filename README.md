# RustOS

RustOS — учебная 64-битная микроядерная операционная система на Rust.
Рабочие эталонные платформы — AMD64 (GRUB/Multiboot2) и AArch64 (QEMU `virt`
+ AAVMF). Обе запускают изолированные пользовательские RUNE-процессы,
вытеснение, IPC, VFS и dynamic loader; CPU-зависимый код изолирован для
будущих Raspberry Pi и других ARMv8-A платформ.

Текущий рабочий вертикальный срез:

- GRUB/Multiboot2 boot на AMD64 и UEFI loader на Rust для AArch64;
- ELF64 kernel для выбранной ISA и проверяемый RIFS initramfs;
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
- process/capability ABI v8: ring-3 `spawn/wait/kill`, несколько потоков с
  `create/join`, argv/environment, FS-base/TPIDR TLS и monotonic clock;
- anonymous `map/unmap/protect` с W^X и shared-memory objects с раздельным
  учётом capability/mapping references;
- bounded endpoint IPC: block/wake, kernel-supplied sender PID, FIFO queue,
  process-owned динамические endpoint'ы и передача только ослабленных
  capabilities;
- host-tested scheduler core: CPU affinity, приоритетные классы и bounded
  driver priority;
- wide-screen GOP/virtio-gpu scanout поверх PCI (AMD64) и MMIO (AArch64),
  выбор режима и CPU back buffer с damage;
- современный graphics ABI: capability buffers, packed/multi-plane RGB/YUV,
  color metadata, explicit timeline sync, surface queues и presentation
  feedback;
- настоящие kernel objects `GraphicsBuffer`/`SyncTimeline`, эксклюзивная
  scanout capability и atomic present с оценочным vblank через постоянные
  supervisor-сервисы `compositord`/`displayd`;
- асинхронная Virtio GPU command queue, Mesa/Gallium platform layer,
  изолированный ring-3 `renderd` и системное приложение **Aurora 3D** с
  shader/lighting scene без guest CPU rasterization или копирования пикселей;
- xHCI USB HID keyboard/mouse на AMD64 и AArch64 с hot-plug и независимыми
  PS/2/virtio-input fallback backend'ами;
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
brew install qemu shellcheck
make run       # автоматически AArch64 + HVF + CPU host
```

Debian/Ubuntu:

```sh
sudo apt install qemu-system-x86 qemu-system-arm ovmf shellcheck
make bootstrap
make run       # AMD64; на native AArch64 Linux можно явно make run-arm
```

ARM (AArch64, QEMU `virt` + UEFI):

```sh
make build-arm      # kernel/RUNE/initramfs/VaraniaFS/AAVMF/ESP
make run-arm        # интерактивный QEMU virt; serial в терминале
make test-arm-boot  # headless EL0/EL1/GICv3/PSCI integration test
make test-arm-gui   # virtio GPU + xHCI USB keyboard/mouse + SystemUI
```

Firmware для ARM (AAVMF) уже закоммичен в `firmware/aarch64/`; сетевая
загрузка не требуется. Полный ESP находится в `build/arm/esp-arm.img`,
канонический загрузчик — `EFI/BOOT/BOOTAA64.EFI`, persistent диск —
`build/arm-system.vfs`.

На Apple Silicon `make build` выбирает ARM, а `make run` — UTM GPU-профиль:
QEMU `virt`, HVF, CPU `host`, 4 vCPU, 2 ГиБ RAM, `virtio-gpu-gl`, VirGL и
ANGLE/Metal. Это аппаратная виртуализация AArch64 и host GPU acceleration без
медленной эмуляции x86. Первый запуск требует установленный UTM
(`brew install --cask utm`). Явные переносимый ARM и AMD64 профили сохранены
как `make run-arm` и `make run-x86`. На других хостах default остаётся AMD64.

UTM запускается только через `make run`, `make run-utm-gpu` или приложение
UTM. Вложенный `QEMULauncher.app` — sandboxed XPC helper, а не самостоятельный
QEMU executable; прямой запуск helper'а macOS завершит диагностикой sandbox.

`make run` открывает графическое окно QEMU. Клавиатура сразу направлена в
terminal. Мышью можно перемещать окно и использовать кнопки заголовка.

## Проверка

```sh
make lint
make test-host
make test-arch
make test
make test-virgl   # отдельный E2E-тест; нужен QEMU с virtio-vga-gl
make test-utm-gpu # Apple Silicon: VirGL -> UTM ANGLE -> Metal
```

`make test` выполняет шесть разных сценариев:

1. Host unit tests проверяют ABI, scheduler, SystemUI/assets, video,
   RUNE/loaders, filesystem и инструменты образов.
2. Cross-build создаёт AMD64 и AArch64 ELF ядра, runtime и приложений.
3. AMD64 boot-test завершается настоящим `isa-debug-exit`.
4. AArch64 boot-test проходит AAVMF, GICv3 preemption, PSCI SMP, EL0 fault
   containment и завершается через `PSCI SYSTEM_OFF`.
5. ARM GUI smoke выполняет ввод `help`, клик Start и screendump через настоящие
   virtio GPU и xHCI USB HID event/transfer rings.
6. AMD64 GUI-тест через настоящий xHCI USB HID выполняет запись/чтение файлов и работу с
   каталогами, перетаскивает и сворачивает terminal, получает QEMU
   screendump и проверяет геометрию и пиксели.

Диагностика сохраняется в `build/test-results/`.

## Архитектурный статус

Обязательный первый рубеж микроядра пройден: QEMU действительно выполняет
`system/bin/init.rune` в CPL3/EL0. Процесс получает только read-only handle корня
bootstrap VFS, выполняет syscall, завершается, а второй тестовый RUNE вызывает
illegal instruction (`UD2` на AMD64, `BRK` на AArch64); ядро локализует fault,
освобождает address space и продолжает boot.

GUI пока остаётся bootstrap-сеансом CPU0 внутри kernel image. До его запуска
process manager выполняет настоящую preemptive-сессию: timer IRQ сохраняет
user context, scheduler выбирает другой TID, меняется CR3/TTBR0, а
`iretq`/`eret` возвращает уже другой процесс. На обеих ISA второй CPU реально
запускается и подтверждает online, но пока parked: следующий рубеж — per-CPU
runtime, interrupt routing, TLB shootdown и work stealing. Постоянный
`displayd` уже один владеет scanout capability, `renderd` отдельно владеет
3D render capability, а `compositord` передаёт готовые GraphicsBuffer через
atomic present и получает presentation feedback. Подключение оконных
surface queues начато: `surface.dll` и boot-test уже проходят полный
`create/commit/direct-scanout/release/feedback/destroy` между независимым
ring-3 приложением и compositord. Multi-layer GPU composition, input и сам
desktop ещё не переключены на этот путь. Нативный `rustc` ещё не заявлен
готовым.

Подробнее: [архитектуры CPU](docs/ARCHITECTURES.md), [архитектура системы](docs/ARCHITECTURE.md),
[графическая подсистема](docs/GUI.md), [видеосистема](docs/VIDEO.md), [VFS](docs/VFS.md),
[ADR современной графической архитектуры](docs/adr/0001-modern-graphics-architecture.md),
[graphics objects ABI](docs/GRAPHICS_ABI.md), [GPU rendering](docs/GPU_RENDERING.md),
[план полного GPU-ускорения](docs/GPU_ACCELERATION.md),
[Mesa и Aurora 3D](docs/MESA.md),
[USB и HID](docs/USB.md),
[микроядро](docs/MICROKERNEL.md), [DLL](docs/DYNAMIC_LIBRARIES.md),
[модель приложений и RUNE](docs/APPLICATION_MODEL.md),
[IPC](docs/IPC.md), [процессы и память ABI](docs/PROCESS_MEMORY_ABI.md),
[self-hosting Rust](docs/SELF_HOSTING.md),
[сборка](docs/BUILDING.md).

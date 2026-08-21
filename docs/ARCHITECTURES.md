# Поддержка архитектур CPU

RustOS разделяет **ISA**, **платформу** и переносимые механизмы. ARM — это не
одна плата: QEMU `virt`, Raspberry Pi и телефонный SoC используют одну ISA
AArch64, но разные GIC, UART, таймеры загрузки, GPIO, USB и display controller.
Поэтому адреса устройств никогда не должны попадать в общий код ядра.

## Границы исходников

```text
abi / microkernel / video / VFS / GUI / applications
                         |
                         v
kernel/src/arch/mod.rs          общий контракт CPU
          |                |
          v                v
arch/x86_64              arch/aarch64
GDT/IDT/TSS              EL0/EL1 context
CR3, APIC, TSC           TTBR0, ESR/FAR, Generic Counter
iretq, int 0x80          eret contract, svc #0

platform drivers (отдельный слой)
ACPI / Device Tree / PCI / virtio / GPIO / USB / framebuffer
```

Process manager видит только `TrapFrame`, `TrapKind`, `UserContext`, root
address space и абстрактный scheduler timer. Он не знает имён регистров,
номеров APIC/GIC или способа возврата в user mode. ISA-specific assembler
разрешён только в `kernel/src/arch/*`, `runtime/src/arch.rs` и platform boot
слое. AMD64 Multiboot entry изолирован в `arch/x86_64/multiboot2.rs`.

PS/2 находится в `kernel/src/input/ps2.rs`, потому что это устройство PC, а
не часть x86. На ARM input backend пока пустой; конкретная плата позднее
выберет USB HID, virtio-input или SoC controller из Device Tree/ACPI.

## Общий ABI

`BootInfo v3` передаёт:

- нормализованную 64-битную карту памяти;
- framebuffer без предположения о GPU;
- тип и адрес раннего UART: port/MMIO 16550 либо PL011;
- firmware root: ACPI RSDP либо Flattened Device Tree;
- initramfs, kernel reservation и boot stack.

Syscall numbers, capability handles, IPC и RUNE metadata одинаковы на обеих
ISA. Различается только низкоуровневая конвенция:

| Target | Номер | Аргументы | Вход | Результат |
|---|---|---|---|---|
| AMD64 | RAX | RDI, RSI, RDX | `int 0x80` | RAX |
| AArch64 | x8 | x0, x1, x2 | `svc #0` | x0 |

RUNE выбирает `X86_64` либо `AARCH64` slice. Build converter принимает
соответствующий ELF machine/RELATIVE relocation только как промежуточный
toolchain output. User stack соблюдает SysV AMD64 и AAPCS64.
Исходники bootstrap-программ общие: даже test fault и monotonic counter
вызываются через runtime, без assembler в приложениях.

## Проверяемый статус

| Возможность | AMD64 | AArch64 |
|---|---:|---:|
| kernel/runtime/apps + RUNE converter | да | да |
| page descriptor encoding | да | да, 4 KiB granule |
| syscall/context ABI | работает: CPL3, `int 0x80` | работает: EL0, `svc #0` |
| bootloader + запуск в VM | GRUB 2/Multiboot2 + OVMF | AAVMF + BOOTAA64.EFI |
| interrupt controller / timer | xAPIC/x2APIC + TSC deadline | GICv3 + Generic Timer PPI 30 |
| SMP startup | ACPI + INIT-SIPI | Device Tree + PSCI `CPU_ON` |
| persistent block | virtio-blk PCI | virtio-blk modern MMIO |
| input | PS/2 bootstrap | нужен virtio-input/USB HID |

`make test-arch` **собирает**, а не только парсит, kernel, runtime и все
bootstrap applications для обоих JSON targets. `make test-arm-boot`
дополнительно запускает двухпроцессорную AArch64 VM: проверяет UEFI handoff,
FDT, GICv3, timer preemption, настоящий `PSCI CPU_ON`, fault containment,
RUNE/std/VFS/loader и маркер `RING3_MILESTONE_OK`. CI выполняет обе цели,
поэтому случайный x86-only assembler в общем коде либо runtime-регрессия ARM
не останутся compile-only «успехом».

## Эталонная ARM-платформа

Эталонная платформа — QEMU `virt`, CPU `cortex-a72`, AAVMF и Device Tree.
Рабочая цепочка уже включает:

1. `aarch64-unknown-uefi` loader, который принимает вход AAVMF из EL1 либо
   EL2, строит 48-битный 4-KiB identity map и передаёт FDT в `BootInfo`;
2. полные EL0/EL1 exception frames, `eret`/`svc`, отдельные process TTBR0,
   W^X и локализацию synchronous fault;
3. GICv3 system-register interface и architected physical timer с настоящим
   вытеснением пользовательских контекстов;
4. bounded FDT parser и PSCI HVC/SMC conduit; до 64 CPU получают отдельные
   стеки, подтверждают online и пока безопасно parked;
5. modern virtio-mmio block transport, persistent VaraniaFS, RUNE и
   портированный Rust `std` в ring 3.

Следующая граница честно уже не «запустить ARM»: нужны per-CPU scheduler/GICR,
TLB shootdown и распределение runnable threads, затем virtio-input и
virtio-gpu/display service. Текущий ARM запуск serial-only, поскольку AAVMF в
этой конфигурации не предоставляет GOP.

После QEMU `virt` Raspberry Pi добавляет только platform backend (firmware
boot, BCM interrupt/display/USB либо UEFI), не форк микроядра. Телефоны требуют
отдельной платы поддержки для каждого SoC и разблокированного boot chain;
наличие AArch64 само по себе не делает закрытые GPU/modem drivers переносимыми.

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
разрешён только в `kernel/src/arch/*`, `runtime/src/arch.rs` и
`boot/uefi/src/arch/*`.

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
| syscall/context ABI | работает | определён и компилируется |
| bootloader + запуск в VM | UEFI/OVMF | следующий milestone |
| interrupt controller | xAPIC/x2APIC | нужен GICv2/v3 driver |
| SMP startup | ACPI + INIT-SIPI | нужен PSCI + DT/ACPI CPU discovery |
| input | PS/2 bootstrap | нужен virtio-input/USB HID |

`make test-arch` **собирает**, а не только парсит, kernel, runtime и все
bootstrap applications для обоих JSON targets. CI выполняет эту цель на
каждом изменении, поэтому случайный `asm!("rdtsc")` в общем коде сразу ломает
AArch64 build.

## Следующий ARM milestone

Первой эталонной ARM-платформой должна быть QEMU `virt`: она документирована,
имеет GIC, architected timer, PL011, PSCI и virtio-mmio/PCI. Порядок работ:

1. AArch64 UEFI handoff или минимальный Image/FDT boot protocol;
2. MAIR/TCR/TTBR bootstrap tables и `VBAR_EL1` vector table;
3. вход/выход EL0 через `eret`, `svc` и containment synchronous faults;
4. GICv3 + Generic Timer, затем реальное вытеснение;
5. PSCI `CPU_ON`, per-CPU state и TLB shootdown;
6. PL011, virtio-input/block/GPU как user-space drivers;
7. тот же boot/process/IPC/GUI test contract в `qemu-system-aarch64`.

После QEMU `virt` Raspberry Pi добавляет только platform backend (firmware
boot, BCM interrupt/display/USB либо UEFI), не форк микроядра. Телефоны требуют
отдельной платы поддержки для каждого SoC и разблокированного boot chain;
наличие AArch64 само по себе не делает закрытые GPU/modem drivers переносимыми.

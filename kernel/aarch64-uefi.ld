/*
 * Фиксированная раскладка AArch64-ядра, которое UEFI-загрузчик RustOS
 * загружает по vaddr сегментов (ET_EXEC, без динамических релокаций).
 *
 * QEMU `virt` (современные QEMU): RAM начинается с 0x40000000
 * (mach-virt.ram); окна GIC (0x08000000) и PL011 (0x09000000) находятся
 * ниже. Ядро линкуется в начало RAM, загрузчик закрепляет ровно этот
 * адрес (AllocatePages(AllocateType::Address)) и копирует PT_LOAD
 * по физическим vaddr — релокации не нужны.
 *
 * Пользовательские программы RustOS остаются PIE. Только kernel image
 * имеет физические адреса (зеркало x86_64-grub.ld).
 */
OUTPUT_FORMAT(elf64-littleaarch64)
OUTPUT_ARCH(aarch64)
ENTRY(_start)

KERNEL_BASE = 0x40000000;

SECTIONS
{
    . = KERNEL_BASE;
    __kernel_start = .;

    .text ALIGN(4K) :
    {
        *(.text .text.*)
    }

    .rodata ALIGN(4K) :
    {
        *(.rodata .rodata.*)
    }

    .data.rel.ro ALIGN(4K) :
    {
        *(.data.rel.ro .data.rel.ro.*)
    }

    .data ALIGN(4K) :
    {
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss ALIGN(4K) (NOLOAD) :
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
    }
}

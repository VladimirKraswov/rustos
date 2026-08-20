/*
 * Фиксированная раскладка AMD64-ядра, которое GRUB загружает по Multiboot2.
 *
 * Пользовательские программы RustOS остаются PIE. Только kernel image имеет
 * физические адреса: до включения собственных page tables загрузчик должен
 * знать, куда положить 32-битную точку входа и ранний стек.
 */
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(rustos_multiboot2_entry)

KERNEL_BASE = 16M;

SECTIONS
{
    . = KERNEL_BASE;
    __kernel_start = .;

    /* Header обязан целиком находиться в первых 32 KiB ELF-файла. */
    .multiboot2 ALIGN(8) :
    {
        KEEP(*(.multiboot2))
    }

    .boot.text ALIGN(16) :
    {
        KEEP(*(.boot.text))
    }

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

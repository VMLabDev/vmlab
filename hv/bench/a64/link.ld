/* The flat aarch64 images of bench/micro_a64.py. QEMU's `virt` machine has
 * its RAM at 0x40000000 and puts its device tree at the start of it, so the
 * image starts 2 MiB above. The stack ends at 0x40f00000 (start.S, loop.S). */
OUTPUT_ARCH(aarch64)
ENTRY(_start)
SECTIONS {
    . = 0x40200000;
    .text : { *(.text.start) *(.text .text.*) }
    .rodata ALIGN(4096) : { *(.rodata .rodata.*) }
    .data ALIGN(4096) : { *(.data .data.*) }
    .bss ALIGN(4096) (NOLOAD) : { *(.bss .bss.* COMMON) }
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.ARM.*) }
}

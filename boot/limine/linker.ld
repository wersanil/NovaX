OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text   PT_LOAD;
    rodata PT_LOAD;
    data   PT_LOAD;
}

SECTIONS
{
    . = 0xffffffff80000000;
    __kernel_start = .;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(4096);
    .data : {
        *(.data .data.*)
    } :data

    . = ALIGN(4096);
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /* Limine request markers — must be in this order and kept. */
    . = ALIGN(8);
    .requests_start_marker : { KEEP(*(.requests_start_marker)) }
    . = ALIGN(8);
    .requests             : { KEEP(*(.requests)) }
    . = ALIGN(8);
    .requests_end_marker  : { KEEP(*(.requests_end_marker)) }

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.note .note.*)
        *(.comment)
    }
}
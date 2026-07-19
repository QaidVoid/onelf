/* Flat-binary layout for the onelf bootstrap. Everything contiguous from
   vaddr 0; the entry trampoline (.text.onelf_start) is forced first so
   `_onelf_start` sits at offset 0. rust-objcopy -O binary flattens the
   linked ELF; there must be no runtime relocations. */
ENTRY(_onelf_start)
SECTIONS {
    . = 0;
    .text : {
        *(.text.onelf_start)
        *(.text .text.*)
    }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) }
    .bss : { *(.bss .bss.*) }
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) }
}

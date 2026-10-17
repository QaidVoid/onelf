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
    /* Declared so the asserts below can measure them on every target:
       ld refuses SIZEOF of a section that does not exist. */
    .got : { *(.got .got.*) }
    .dynamic : { *(.dynamic) }
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) }
    /* Anything the flattening would silently drop or that needs a runtime
       relocation is a bootstrap that crashes on the user's machine with
       no build-time signal. A GOT, initialized data, or a dynamic section
       here means the compiler started emitting one of those. */
    ASSERT(SIZEOF(.data) == 0, "bootstrap: .data must be empty")
    ASSERT(SIZEOF(.got) == 0, "bootstrap: no GOT allowed")
    ASSERT(SIZEOF(.dynamic) == 0, "bootstrap: no dynamic section allowed")
}

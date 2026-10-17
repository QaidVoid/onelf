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
    /* i686 position-independent code always gets the GOT's reserved
       header, zeros that need no relocation, so the GOT itself is fine.
       Declared so the asserts below can measure these on every target:
       ld refuses SIZEOF of a section that does not exist. */
    .got : { *(.got .got.*) }
    .rel.dyn : { *(.rel.*) }
    .rela.dyn : { *(.rela.*) }
    .dynamic : { *(.dynamic) }
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) }
    /* Anything the flattening would silently drop or that needs a runtime
       relocation is a bootstrap that crashes on the user's machine with
       no build-time signal. Initialized data, a relocation or a dynamic
       section here means the compiler started emitting one of those. */
    ASSERT(SIZEOF(.data) == 0, "bootstrap: .data must be empty")
    ASSERT(SIZEOF(.rel.dyn) == 0 && SIZEOF(.rela.dyn) == 0, "bootstrap: no runtime relocations allowed")
    ASSERT(SIZEOF(.dynamic) == 0, "bootstrap: no dynamic section allowed")
}

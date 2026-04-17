// onelf bootstrap trampoline for aarch64.

.text
.globl _onelf_start
_onelf_start:
    // Save original stack pointer and link register.
    mov     x29, sp
    // Align stack to 16 bytes.
    bic     x0, x29, #15
    mov     sp, x0
    // arg1 (x0): pointer to original stack (argc at top)
    mov     x0, x29
    // arg2 (x1): pointer to metadata
    adr     x1, _onelf_metadata
    // Preserve x2 (fini function pointer from kernel)
    mov     x19, x2
    bl      _onelf_bootstrap
    // x0 = interpreter entry address
    mov     x2, x19
    mov     sp, x29
    br      x0

.globl _onelf_metadata
_onelf_metadata:
    // Filled in by inject_relative_interp():
    //   u64  original_entry
    //   u16  rel_path_len
    //   char rel_path[]  (NUL-terminated)

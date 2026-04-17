# onelf bootstrap trampoline for x86_64.
# Entry point that sets up args and calls the C bootstrap function,
# then jumps to the returned interpreter entry address.

.text
.globl _onelf_start
_onelf_start:
    # Save original stack pointer (argc/argv/envp/auxv).
    mov     %rsp, %rbp
    # Align stack to 16 bytes for C ABI.
    and     $-16, %rsp
    # arg1: pointer to original stack (argc at top)
    mov     %rbp, %rdi
    # arg2: pointer to metadata (right after this code)
    lea     _onelf_metadata(%rip), %rsi
    # Preserve rdx (set by kernel to fini function pointer)
    mov     %rdx, %rbx
    call    _onelf_bootstrap
    # rax = interpreter entry address
    # Restore original stack and rdx
    mov     %rbx, %rdx
    mov     %rbp, %rsp
    # Jump to interpreter
    jmp     *%rax

.globl _onelf_metadata
_onelf_metadata:
    # Filled in by inject_relative_interp():
    #   u64  original_entry
    #   u16  rel_path_len
    #   char rel_path[]  (NUL-terminated)

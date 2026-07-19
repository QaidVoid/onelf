//! onelf relative-interpreter bootstrap.
//!
//! Compiled per target to a position-independent, freestanding flat binary
//! that the `onelf` packer appends as a `PT_LOAD` segment and points the ELF
//! entry at. It resolves the bundled interpreter relative to `AT_EXECFN`,
//! maps it, rewrites the program headers to append `PT_INTERP`, patches the
//! auxiliary vector, and jumps to the interpreter's entry.
//!
//! Ported from the former `payload/bootstrap_{x86_64,aarch64}.c` +
//! `trampoline_*.S`. The trampoline layout is byte-compatible with the
//! metadata-pointer patch offsets in `onelf`'s `payload.rs`.
#![no_std]
#![no_main]
#![allow(unsafe_op_in_unsafe_fn)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    arch::exit(127)
}

// Volatile byte copy/zero. The flat binary is executed with no loader, so it
// must contain zero GOT-indirect calls: LLVM would otherwise lower a plain
// copy loop or `core::mem::zeroed` to a `memcpy`/`memset` libcall reached
// through a never-relocated GOT slot (a link-time-absolute address, wrong once
// loaded at the injected vaddr). `write_volatile` is not loop-idiom-recognized,
// so no such symbol is ever referenced.
#[inline(always)]
unsafe fn bcopy(dst: *mut u8, src: *const u8, n: usize) {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
        i += 1;
    }
}

#[inline(always)]
unsafe fn bzero(dst: *mut u8, n: usize) {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), 0);
        i += 1;
    }
}

// ---- architecture entry trampoline + syscalls -----------------------------

#[cfg(target_arch = "x86_64")]
mod arch {
    use core::arch::{asm, global_asm};

    // Entry point. Kept in its own section so the linker script can place it
    // at offset 0. The `lea rsi, [rip + _onelf_metadata]` is at byte 0x0a with
    // its disp32 at 0x0d; the packer overwrites that disp to point at the
    // appended metadata (see payload.rs X86_64_METADATA_LEA_* constants).
    global_asm!(
        ".pushsection .text.onelf_start,\"ax\",@progbits",
        ".globl _onelf_start",
        "_onelf_start:",
        "mov rbp, rsp",
        "and rsp, -16",
        "mov rdi, rbp",
        "lea rsi, [rip + _onelf_metadata]",
        "mov rbx, rdx",
        "call _onelf_bootstrap",
        "mov rdx, rbx",
        "mov rsp, rbp",
        "jmp rax",
        ".globl _onelf_metadata",
        "_onelf_metadata:",
        ".popsection",
    );

    #[inline]
    unsafe fn sys3(nr: i64, a: i64, b: i64, c: i64) -> i64 {
        let ret;
        asm!("syscall", inlateout("rax") nr => ret,
            in("rdi") a, in("rsi") b, in("rdx") c,
            out("rcx") _, out("r11") _);
        ret
    }

    pub unsafe fn open_rdonly(path: *const u8) -> i64 {
        sys3(2, path as i64, 0, 0)
    }
    pub unsafe fn read(fd: i32, buf: *mut u8, n: u64) -> i64 {
        sys3(0, fd as i64, buf as i64, n as i64)
    }
    pub unsafe fn close(fd: i32) -> i64 {
        sys3(3, fd as i64, 0, 0)
    }
    pub unsafe fn write(fd: i32, buf: *const u8, n: u64) -> i64 {
        sys3(1, fd as i64, buf as i64, n as i64)
    }
    pub unsafe fn readlink(path: *const u8, buf: *mut u8, n: u64) -> i64 {
        sys3(89, path as i64, buf as i64, n as i64)
    }

    pub unsafe fn pread(fd: i32, buf: *mut u8, n: u64, off: u64) -> i64 {
        let ret;
        asm!("syscall", inlateout("rax") 17i64 => ret,
            in("rdi") fd as i64, in("rsi") buf as i64, in("rdx") n as i64,
            in("r10") off, out("rcx") _, out("r11") _);
        ret
    }

    pub unsafe fn mmap(addr: u64, len: u64, prot: i32, flags: i32, fd: i32, off: u64) -> i64 {
        let ret;
        asm!("syscall", inlateout("rax") 9i64 => ret,
            in("rdi") addr, in("rsi") len, in("rdx") prot as i64,
            in("r10") flags as i64, in("r8") fd as i64, in("r9") off,
            out("rcx") _, out("r11") _);
        ret
    }

    pub fn exit(code: i32) -> ! {
        unsafe {
            asm!("syscall", in("rax") 60i64, in("rdi") code as i64, options(noreturn));
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use core::arch::{asm, global_asm};

    // Entry point in its own section (placed first by the linker script). The
    // `adr x1, _onelf_metadata` is at byte 0x10; the packer rewrites its imm
    // to point at the appended metadata (payload.rs AARCH64_METADATA_ADR_*).
    global_asm!(
        ".pushsection .text.onelf_start,\"ax\",@progbits",
        ".globl _onelf_start",
        "_onelf_start:",
        "mov x29, sp",
        "bic x0, x29, #15",
        "mov sp, x0",
        "mov x0, x29",
        "adr x1, _onelf_metadata",
        "mov x19, x2",
        "bl _onelf_bootstrap",
        "mov x2, x19",
        "mov sp, x29",
        "br x0",
        ".globl _onelf_metadata",
        "_onelf_metadata:",
        ".popsection",
    );

    #[inline]
    unsafe fn svc(nr: i64, a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> i64 {
        let ret;
        asm!("svc #0", inlateout("x0") a => ret,
            in("x1") b, in("x2") c, in("x3") d, in("x4") e, in("x5") f,
            in("x8") nr);
        ret
    }

    pub unsafe fn open_rdonly(path: *const u8) -> i64 {
        svc(56, -100, path as i64, 0, 0, 0, 0) // openat(AT_FDCWD, path, O_RDONLY)
    }
    pub unsafe fn read(fd: i32, buf: *mut u8, n: u64) -> i64 {
        svc(63, fd as i64, buf as i64, n as i64, 0, 0, 0)
    }
    pub unsafe fn pread(fd: i32, buf: *mut u8, n: u64, off: u64) -> i64 {
        svc(67, fd as i64, buf as i64, n as i64, off as i64, 0, 0)
    }
    pub unsafe fn close(fd: i32) -> i64 {
        svc(57, fd as i64, 0, 0, 0, 0, 0)
    }
    pub unsafe fn write(fd: i32, buf: *const u8, n: u64) -> i64 {
        svc(64, fd as i64, buf as i64, n as i64, 0, 0, 0)
    }
    pub unsafe fn readlink(path: *const u8, buf: *mut u8, n: u64) -> i64 {
        svc(78, -100, path as i64, buf as i64, n as i64, 0, 0) // readlinkat(AT_FDCWD, ...)
    }
    pub unsafe fn mmap(addr: u64, len: u64, prot: i32, flags: i32, fd: i32, off: u64) -> i64 {
        svc(222, addr as i64, len as i64, prot as i64, flags as i64, fd as i64, off as i64)
    }
    pub fn exit(code: i32) -> ! {
        unsafe {
            asm!("svc #0", in("x8") 94i64, in("x0") code as i64, options(noreturn));
        }
    }
}

// ---- ELF / bootstrap constants --------------------------------------------

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_ENTRY: u64 = 9;
const AT_EXECFN: u64 = 31;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 0x02;
const MAP_FIXED: i32 = 0x10;
const MAP_ANONYMOUS: i32 = 0x20;

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const PAGE_SIZE: u64 = 4096;

#[repr(C)]
struct Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

fn die(msg: &[u8]) -> ! {
    unsafe {
        arch::write(2, msg.as_ptr(), msg.len() as u64);
    }
    arch::exit(127)
}

fn pflags(f: u32) -> i32 {
    (if f & PF_R != 0 { PROT_READ } else { 0 })
        | (if f & PF_W != 0 { PROT_WRITE } else { 0 })
        | (if f & PF_X != 0 { PROT_EXEC } else { 0 })
}

unsafe fn map_interp(fd: i32, ehdr: *const Ehdr, pmask: u64) -> u64 {
    let phnum = (*ehdr).e_phnum as usize;
    if phnum > 32 {
        die(b"onelf: interp too many phdrs\n");
    }
    let mut ph_buf: [core::mem::MaybeUninit<Phdr>; 32] =
        [const { core::mem::MaybeUninit::uninit() }; 32];
    let ph = ph_buf.as_mut_ptr() as *mut Phdr;
    let phsz = (phnum * core::mem::size_of::<Phdr>()) as u64;
    if arch::pread(fd, ph as *mut u8, phsz, (*ehdr).e_phoff) != phsz as i64 {
        die(b"onelf: read interp phdrs\n");
    }

    let mut vmax = 0u64;
    for i in 0..phnum {
        let p = &*ph.add(i);
        if p.p_type == PT_LOAD {
            let v = p.p_vaddr + p.p_memsz;
            if v > vmax {
                vmax = v;
            }
        }
    }

    let mut base = 0u64;
    let mut fixed = false;
    for i in 0..phnum {
        let p = &*ph.add(i);
        if p.p_type != PT_LOAD {
            continue;
        }
        let mut off = p.p_offset;
        let mis = off & pmask;
        let mut va = base.wrapping_add(p.p_vaddr).wrapping_sub(mis);
        let pr = pflags(p.p_flags);
        off -= mis;

        if !fixed {
            base = base.wrapping_sub(va);
            va = arch::mmap(0, vmax - va, pr, MAP_PRIVATE, fd, off) as u64;
            if (va as i64) < 0 {
                die(b"onelf: mmap interp\n");
            }
            base = base.wrapping_add(va);
            fixed = true;
        } else if p.p_filesz != 0 {
            va = arch::mmap(va, p.p_filesz + mis, pr, MAP_PRIVATE | MAP_FIXED, fd, off) as u64;
            if (va as i64) < 0 {
                die(b"onelf: mmap interp seg\n");
            }
        }

        if p.p_memsz <= p.p_filesz {
            continue;
        }
        let fe = va + mis + p.p_filesz;
        let pe = (fe + pmask) & !pmask;
        let mut me = va + mis + p.p_memsz;
        if pe < me {
            let r = arch::mmap(pe, me - pe, pr, MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS, -1, 0);
            if r < 0 {
                die(b"onelf: mmap bss\n");
            }
            me = pe;
        }
        if pr & PROT_WRITE != 0 {
            bzero(fe as *mut u8, (me - fe) as usize);
        }
    }
    base
}

/// Called by the trampoline: `stack` points at the original stack (argc at
/// top); `meta` at the appended `[i64 entry_delta][u16 rel_path_len][path]`.
/// Returns the resolved interpreter entry address.
#[unsafe(no_mangle)]
unsafe extern "C" fn _onelf_bootstrap(stack: *mut u64, meta: *const u8) -> u64 {
    let entry_delta = (meta as *const i64).read_unaligned();
    let rel_path_len = (meta.add(8) as *const u16).read_unaligned() as usize;
    let rel_path = meta.add(10);

    let argc = *(stack as *const u32) as usize;
    // envp = stack + argc + 2 (skip argc word + argv[argc] + NULL)
    let mut envp = (stack as *const *const u8).add(argc + 2);
    while !(*envp).is_null() {
        envp = envp.add(1);
    }
    envp = envp.add(1);

    // Index auxv val slots (tags 0..=31), keeping writable pointers.
    let mut auxv_buf: [core::mem::MaybeUninit<*mut u64>; 32] =
        [const { core::mem::MaybeUninit::uninit() }; 32];
    let auxv = auxv_buf.as_mut_ptr() as *mut *mut u64;
    let mut seen: u32 = 0;
    let mut a = envp as *mut u64;
    loop {
        let tag = *a;
        if tag <= 31 {
            seen |= 1u32 << tag;
            *auxv.add(tag as usize) = a.add(1);
        }
        if tag == AT_NULL {
            break;
        }
        a = a.add(2);
    }

    let has = |t: u64| seen & (1u32 << t) != 0;

    if !has(AT_EXECFN) {
        die(b"onelf: no AT_EXECFN\n");
    }
    let mut execfn = *(*auxv.add(AT_EXECFN as usize)) as *const u8;

    // Resolve /proc/... execfn (e.g. /proc/self/exe after a re-exec) so the
    // sibling lib/ dir is found relative to the real binary.
    let mut resolved: [core::mem::MaybeUninit<u8>; 4096] =
        [const { core::mem::MaybeUninit::uninit() }; 4096];
    let resolved = resolved.as_mut_ptr() as *mut u8;
    if slice_eq(execfn, b"/proc/") {
        let n = arch::readlink(execfn, resolved, 4095);
        if n > 0 {
            *resolved.add(n as usize) = 0;
            execfn = resolved as *const u8;
        }
    }

    // Dirname length of execfn (index past the last '/').
    let mut dlen = 0usize;
    let mut i = 0usize;
    while *execfn.add(i) != 0 {
        if *execfn.add(i) == b'/' {
            dlen = i + 1;
        }
        i += 1;
    }

    let plen = dlen + rel_path_len;

    let nph = if has(AT_PHNUM) {
        *(*auxv.add(AT_PHNUM as usize)) as usize
    } else {
        0
    };
    let alloc = ((nph + 1) * core::mem::size_of::<Phdr>() + plen + 1) as u64;
    let buf = arch::mmap(
        0,
        alloc,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    if buf < 0 {
        die(b"onelf: alloc\n");
    }
    let buf = buf as u64;

    // Copy phdrs, patch PT_PHDR to describe the new (grown) table.
    let nw = buf as *mut Phdr;
    let mut baddr = 0u64;
    if nph != 0 && has(AT_PHDR) {
        let old = *(*auxv.add(AT_PHDR as usize)) as *const Phdr;
        bcopy(
            nw as *mut u8,
            old as *const u8,
            nph * core::mem::size_of::<Phdr>(),
        );
        for j in 0..nph {
            if (*nw.add(j)).p_type == PT_PHDR {
                baddr = (old as u64).wrapping_sub((*nw.add(j)).p_vaddr);
                (*nw.add(j)).p_vaddr = buf.wrapping_sub(baddr);
                (*nw.add(j)).p_paddr = (*nw.add(j)).p_vaddr;
                (*nw.add(j)).p_filesz = ((nph + 1) * core::mem::size_of::<Phdr>()) as u64;
                (*nw.add(j)).p_memsz = (*nw.add(j)).p_filesz;
            }
        }
    }

    // Build interp path after the phdrs: dirname(execfn) + rel_path.
    let ipath = nw.add(nph + 1) as *mut u8;
    bcopy(ipath, execfn, dlen);
    bcopy(ipath.add(dlen), rel_path, rel_path_len);
    *ipath.add(plen) = 0;

    // Append the PT_INTERP entry.
    let iph = nw.add(nph);
    bzero(iph as *mut u8, core::mem::size_of::<Phdr>());
    (*iph).p_type = PT_INTERP;
    (*iph).p_vaddr = (ipath as u64).wrapping_sub(baddr);
    (*iph).p_filesz = (plen + 1) as u64;
    (*iph).p_memsz = (plen + 1) as u64;
    (*iph).p_flags = PF_R;

    // Patch auxv to point at the new phdr table and shift the entry.
    if has(AT_PHDR) {
        *(*auxv.add(AT_PHDR as usize)) = buf;
    }
    if has(AT_PHNUM) {
        *(*auxv.add(AT_PHNUM as usize)) = (nph + 1) as u64;
    }
    if has(AT_ENTRY) {
        *(*auxv.add(AT_ENTRY as usize)) = (*(*auxv.add(AT_ENTRY as usize))).wrapping_add(entry_delta as u64);
    }

    // Map the interpreter and hand back its entry.
    let fd = arch::open_rdonly(ipath);
    if fd < 0 {
        die(b"onelf: open interp\n");
    }
    let fd = fd as i32;
    let mut ehdr = core::mem::MaybeUninit::<Ehdr>::uninit();
    let ehsz = core::mem::size_of::<Ehdr>() as u64;
    if arch::read(fd, ehdr.as_mut_ptr() as *mut u8, ehsz) != ehsz as i64 {
        die(b"onelf: read interp\n");
    }
    let ehdr = ehdr.as_ptr();
    if ((*ehdr).e_ident.as_ptr() as *const u32).read_unaligned() != 0x464c_457f {
        die(b"onelf: not ELF\n");
    }

    let pmask = if has(AT_PAGESZ) {
        *(*auxv.add(AT_PAGESZ as usize)) - 1
    } else {
        PAGE_SIZE - 1
    };
    let ibase = map_interp(fd, ehdr, pmask);
    if has(AT_BASE) {
        *(*auxv.add(AT_BASE as usize)) = ibase;
    }
    arch::close(fd);

    ibase + (*ehdr).e_entry
}

/// True if the NUL-terminated `s` starts with `prefix`.
unsafe fn slice_eq(s: *const u8, prefix: &[u8]) -> bool {
    for (k, &b) in prefix.iter().enumerate() {
        if *s.add(k) != b {
            return false;
        }
    }
    true
}

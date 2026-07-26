//! Userland `execve`: load the bundled dynamic linker into this process and
//! hand control to it, without an `execve(2)` syscall.
//!
//! onelf invokes the bundled linker directly, passing `--library-path` and
//! `--argv0` as arguments; the linker then loads the target binary itself. This
//! pattern (matching sharun) avoids polluting the child env with
//! `LD_LIBRARY_PATH`, which would otherwise leak into every host binary the app
//! spawns.
//!
//! This is a small in-tree implementation built on `rustix` + `goblin` (both
//! already dependencies). It replaces the external `userland-execve` crate,
//! which supported only x86_64/aarch64; here x86_64, aarch64 and i686 are all
//! handled. The initial auxv is taken from `/proc/self/auxv` (so entries the
//! loader relies on -- notably the vDSO `AT_SYSINFO_EHDR`, and `AT_RANDOM` /
//! `AT_HWCAP` / `AT_PLATFORM` -- are carried over verbatim) with only the
//! program-specific entries overridden for the loaded image.

use std::ffi::{CStr, CString};
use std::mem::size_of;
use std::os::fd::AsFd;
use std::path::Path;

use goblin::elf::Elf;
use goblin::elf::program_header::PT_LOAD;
use rustix::mm::{MapFlags, ProtFlags, mmap, mmap_anonymous};

// Auxiliary-vector tags we read or override for the loaded image.
const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_PHENT: usize = 4;
const AT_PHNUM: usize = 5;
const AT_PAGESZ: usize = 6;
const AT_BASE: usize = 7;
const AT_ENTRY: usize = 9;
const AT_EXECFN: usize = 31;

/// The mapped loader image.
struct Loaded {
    entry: usize,
    phoff: usize,
    phnum: usize,
    phent: usize,
}

/// Set the stack pointer to `sp` and jump to `entry`, zeroing the registers the
/// SysV process-entry protocol expects to be clear (so the loader never treats
/// a stale rdx/edx as an `atexit` finalizer). Never returns.
#[cfg(target_arch = "x86_64")]
unsafe fn enter(sp: usize, entry: usize) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rsp, {sp}",
            "jmp {entry}",
            sp = in(reg) sp,
            entry = in(reg) entry,
            in("rax") 0usize,
            in("rdx") 0usize,
            options(noreturn),
        )
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn enter(sp: usize, entry: usize) -> ! {
    unsafe {
        core::arch::asm!(
            "mov sp, {sp}",
            "br {entry}",
            sp = in(reg) sp,
            entry = in(reg) entry,
            options(noreturn),
        )
    }
}

#[cfg(target_arch = "x86")]
unsafe fn enter(sp: usize, entry: usize) -> ! {
    // ebx is the PIC base and esi is reserved by LLVM in inline asm, so use
    // explicit ecx/edi for the operands and clear eax/edx.
    unsafe {
        core::arch::asm!(
            "mov esp, ecx",
            "jmp edi",
            in("ecx") sp,
            in("edi") entry,
            in("eax") 0usize,
            in("edx") 0usize,
            options(noreturn),
        )
    }
}

/// Read `/proc/self/auxv` into `(tag, value)` pairs (excluding `AT_NULL`).
fn read_auxv() -> Vec<(usize, usize)> {
    let bytes = std::fs::read("/proc/self/auxv").unwrap_or_default();
    let w = size_of::<usize>();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 * w <= bytes.len() {
        let t = usize::from_ne_bytes(bytes[i..i + w].try_into().unwrap());
        let v = usize::from_ne_bytes(bytes[i + w..i + 2 * w].try_into().unwrap());
        if t == AT_NULL {
            break;
        }
        out.push((t, v));
        i += 2 * w;
    }
    out
}

fn auxval(av: &[(usize, usize)], tag: usize) -> Option<usize> {
    av.iter().find(|(t, _)| *t == tag).map(|(_, v)| *v)
}

/// Map the position-independent ELF at `path` (the bundled loader) into memory.
fn load(path: &Path, page: usize) -> Loaded {
    let bytes = std::fs::read(path).expect("ulexec: read interpreter");
    let elf = Elf::parse(&bytes).expect("ulexec: parse interpreter");
    let file = std::fs::File::open(path).expect("ulexec: open interpreter");

    let round_down = |a: usize| a & !(page - 1);
    let round_up = |a: usize| (a + page - 1) & !(page - 1);

    // The loader must be position-independent (ET_DYN, zero-based first LOAD)
    // and self-contained (no PT_INTERP) -- onelf only ever passes it here.
    let first = elf
        .program_headers
        .iter()
        .find(|h| h.p_type == PT_LOAD)
        .expect("ulexec: no PT_LOAD");
    assert!(
        first.p_vaddr == 0,
        "ulexec: interpreter is not position-independent"
    );

    let total: usize = elf
        .program_headers
        .iter()
        .filter(|h| h.p_type == PT_LOAD)
        .map(|h| (h.p_vaddr + h.p_memsz) as usize)
        .max()
        .expect("ulexec: no PT_LOAD");

    // Reserve the whole image as anonymous zero pages up front: bss then needs
    // no extra mapping, and the MAP_FIXED file overlays land inside it.
    let base = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            round_up(total),
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    }
    .expect("ulexec: reserve image") as usize;

    for ph in elf.program_headers.iter().filter(|h| h.p_type == PT_LOAD) {
        let filesz = ph.p_filesz as usize;
        if filesz == 0 {
            continue; // pure bss: covered by the anonymous reservation
        }
        let mut prot = ProtFlags::empty();
        if ph.p_flags & 0b100 != 0 {
            prot |= ProtFlags::READ;
        }
        if ph.p_flags & 0b010 != 0 {
            prot |= ProtFlags::WRITE;
        }
        if ph.p_flags & 0b001 != 0 {
            prot |= ProtFlags::EXEC;
        }

        let unaligned = base + ph.p_vaddr as usize;
        let addr = round_down(unaligned);
        let align = unaligned - addr;
        unsafe {
            mmap(
                addr as *mut _,
                filesz + align,
                prot | ProtFlags::WRITE, // writable for tail-zeroing / relocations
                MapFlags::PRIVATE | MapFlags::FIXED,
                file.as_fd(),
                (ph.p_offset as usize - align) as u64,
            )
            .expect("ulexec: map segment");
        }
        // A file mapping exposes the file's bytes between filesz and the page
        // boundary; zero that tail so bss (and the loader's early allocations)
        // start clean.
        let file_end = addr + align + filesz;
        unsafe {
            std::ptr::write_bytes(file_end as *mut u8, 0, round_up(file_end) - file_end);
        }
    }

    // No relocations are applied here: the dynamic linker self-relocates from
    // its entry point (both glibc and musl), and pre-applying its REL/RELA
    // RELATIVE entries would be re-applied by the linker -- harmless for the
    // absolute RELA form but a double `+= base` for the i386 REL form.
    Loaded {
        entry: base + elf.header.e_entry as usize,
        phoff: base + elf.header.e_phoff as usize,
        phnum: elf.header.e_phnum as usize,
        phent: elf.header.e_phentsize as usize,
    }
}

/// Builds the initial stack top-down: each push lands at a lower address, so the
/// first byte pushed ends up highest.
struct StackBuilder {
    top: usize,
    rev: Vec<u8>,
}

impl StackBuilder {
    fn push_bytes(&mut self, bytes: &[u8]) -> usize {
        for b in bytes.iter().rev() {
            self.rev.push(*b);
        }
        self.top - self.rev.len()
    }
    fn push_word(&mut self, w: usize) {
        self.push_bytes(&w.to_ne_bytes());
    }
    fn push_cstr(&mut self, s: &CStr) -> usize {
        self.push_bytes(s.to_bytes_with_nul())
    }
}

/// Allocate a fresh stack and lay out `argc`/`argv`/`envp`/`auxv` for the loader.
fn make_stack(
    loaded: &Loaded,
    av: &[(usize, usize)],
    exe: &CStr,
    args: &[CString],
    env: &[CString],
) -> usize {
    let stack_size = 8 * 1024 * 1024;
    let stack = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            stack_size,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    }
    .expect("ulexec: stack") as usize;
    let top = stack + stack_size;
    let word = size_of::<usize>();

    let mut b = StackBuilder {
        top,
        rev: Vec::new(),
    };

    // String data (highest addresses); the arrays below reference these.
    let exe_addr = b.push_cstr(exe);
    let env_addrs: Vec<usize> = env.iter().rev().map(|e| b.push_cstr(e)).collect();
    let arg_addrs: Vec<usize> = args.iter().rev().map(|a| b.push_cstr(a)).collect();

    // auxv carried over from /proc/self/auxv with the program entries
    // overridden. AT_NULL is first so it lands at the highest auxv address.
    let mut auxv: Vec<(usize, usize)> = vec![(AT_NULL, 0)];
    for &(t, v) in av {
        let nv = match t {
            AT_PHDR => loaded.phoff,
            AT_PHENT => loaded.phent,
            AT_PHNUM => loaded.phnum,
            AT_BASE => 0, // the loader is itself the program image here
            AT_ENTRY => loaded.entry,
            AT_EXECFN => exe_addr,
            _ => v,
        };
        auxv.push((t, nv));
    }

    // Pad the string region so the final `argc` (the returned sp) is 16-aligned.
    let fixed_words = auxv.len() * 2 + (env.len() + 1) + (args.len() + 1) + 1;
    while (b.rev.len() + fixed_words * word) % 16 != 0 {
        b.rev.push(0);
    }

    for (t, v) in &auxv {
        b.push_word(*v);
        b.push_word(*t);
    }
    b.push_word(0); // envp terminator
    for a in &env_addrs {
        b.push_word(*a);
    }
    b.push_word(0); // argv terminator
    for a in &arg_addrs {
        b.push_word(*a);
    }
    b.push_word(args.len()); // argc

    let mut data = b.rev;
    data.reverse();
    let sp = top - data.len();
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), sp as *mut u8, data.len()) };
    sp
}

/// Execute an ELF binary by invoking its bundled dynamic linker directly. The
/// linker receives `--library-path` and `--argv0` as command-line flags so the
/// bundled lib search is scoped to this single exec and never inherited by
/// child processes.
///
/// # Arguments
/// * `target` - Path to the ELF binary the linker will load
/// * `interpreter` - Path to the bundled ELF interpreter (ld-linux.so / ld-musl-*)
/// * `lib_path` - Colon-separated library search path for `--library-path`
/// * `argv0` - Value for argv[0] (how the program sees its name) via `--argv0`
/// * `args` - Additional command-line arguments (argv[1..])
///
/// Never returns on success (it replaces the current process image).
pub fn exec_with_interp(
    target: &Path,
    interpreter: &Path,
    lib_path: &str,
    argv0: &str,
    args: &[String],
) -> ! {
    let interp_str = interpreter.to_string_lossy();
    let target_str = target.to_string_lossy();

    let is_musl = interpreter
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("ld-musl-"));

    // argv for the linker: its own path as argv[0], then flags, target, args.
    let mut argv: Vec<CString> = vec![CString::new(interp_str.as_ref()).unwrap()];
    // glibc accepts --inhibit-cache to skip /etc/ld.so.cache (host libs); musl
    // has no cache and errors on unknown flags.
    if !is_musl {
        argv.push(CString::new("--inhibit-cache").unwrap());
    }
    if !lib_path.is_empty() {
        argv.push(CString::new("--library-path").unwrap());
        argv.push(CString::new(lib_path).unwrap());
    }
    argv.push(CString::new("--argv0").unwrap());
    argv.push(CString::new(argv0).unwrap());
    argv.push(CString::new(target_str.as_ref()).unwrap());
    for arg in args {
        argv.push(CString::new(arg.as_str()).unwrap());
    }

    let env: Vec<CString> = std::env::vars()
        .map(|(k, v)| CString::new(format!("{k}={v}")).unwrap())
        .collect();

    let auxv = read_auxv();
    let page = auxval(&auxv, AT_PAGESZ)
        .filter(|p| p.is_power_of_two())
        .unwrap_or(4096);
    let exe = CString::new(interp_str.as_ref()).unwrap();

    let loaded = load(interpreter, page);
    let sp = make_stack(&loaded, &auxv, &exe, &argv, &env);
    unsafe { enter(sp, loaded.entry) }
}

/// Whether userland-exec is supported on this platform.
pub const fn is_supported() -> bool {
    cfg!(all(
        target_os = "linux",
        any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "x86"
        )
    ))
}

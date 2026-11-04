//! onelf payload-side environment constructor (freestanding).
//!
//! Built per target to a tiny `-nostdlib` cdylib with no `DT_NEEDED` of its
//! own, bundled into a package's lib/ and injected as a `DT_NEEDED` of the
//! entrypoint. Its `.init_array` constructor runs on every exec (surviving a
//! sandboxed `clearenv()` + re-exec): it self-locates via `/proc/self/maps`,
//! walks to the package root (the dir containing `.onelf/`), first applies
//! the launch's evaluated per-view input `.onelf/runtime-env` (see [`rtenv`]
//! for the format and the bounds), which restores the generated framework
//! mappings and the exec-hook records a sandboxed `clearenv()` took away,
//! then re-applies `.onelf/env` (KEY=VALUE, `${ONELF_DIR}`/`${VAR:-word}`
//! expanded) and `.onelf/preload` (one dlopen'd lib per line).
//!
//! `setenv`/`dlopen`/`getenv` are left UNDEFINED and resolve from the
//! application's own libc at load time, so one per-arch blob serves both
//! glibc and musl. Ported from the former `payload/onelf_env.c`.
//!
//! It also carries the exec and spawn hooks in [`exec`], which start a
//! program outside the package with the environment the runtime was
//! started with.
#![cfg_attr(not(test), no_std)]
#![allow(unsafe_op_in_unsafe_fn)]

mod exec;

mod rtenv;

#[cfg(test)]
pub(crate) mod harness;

#[cfg(not(test))]
use core::panic::PanicInfo;

#[cfg(not(test))]
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    // The constructor must never unwind into the loader. Exiting with a
    // distinct status is something a user can report; a process spinning
    // at full CPU forever is not.
    unsafe { sys::exit_group(127) }
}

// Resolved from the application's libc at load time (kept UNDEF here).
unsafe extern "C" {
    fn setenv(name: *const u8, value: *const u8, overwrite: i32) -> i32;
    fn dlopen(filename: *const u8, flags: i32) -> *mut core::ffi::c_void;
    fn getenv(name: *const u8) -> *const u8;
}

// `dlopen` lived in libdl.so.2 before glibc 2.34, and nothing guarantees
// that library is in the process. A strong reference would make the
// loader refuse this object, and with it the whole application, at
// startup; a weak one resolves to null and is checked before use.
core::arch::global_asm!(".weak dlopen");

/// Where the loader wrote `dlopen`'s address: zero when the weak symbol
/// went unresolved. Read through memory, since the compiler takes a
/// function's address to be non-null and folds a direct comparison away.
struct Relocated(*const ());
unsafe impl Sync for Relocated {}
static DLOPEN_ADDR: Relocated = Relocated(dlopen as *const ());

unsafe fn have_dlopen() -> bool {
    !core::ptr::read_volatile(&DLOPEN_ADDR.0).is_null()
}

const RTLD_NOW: i32 = 0x0002;
const RTLD_GLOBAL: i32 = 0x0100;

// The code below uses only byte loops, so LLVM emits no mem* intrinsics. Were
// any emitted, they would stay UNDEF and resolve from the app's libc at load
// time (like setenv/dlopen), so no local definition is needed here.

// ---- raw syscalls ---------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[cfg_attr(test, allow(dead_code))]
mod sys {
    use core::arch::asm;

    unsafe fn sys3(nr: i64, a: i64, b: i64, c: i64) -> i64 {
        let ret;
        asm!("syscall", inlateout("rax") nr => ret,
            in("rdi") a, in("rsi") b, in("rdx") c,
            out("rcx") _, out("r11") _);
        ret
    }
    unsafe fn sys4(nr: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
        let ret;
        asm!("syscall", inlateout("rax") nr => ret,
            in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d,
            out("rcx") _, out("r11") _);
        ret
    }
    pub unsafe fn openat_rdonly(path: *const u8) -> i64 {
        sys4(257, -100, path as i64, 0, 0) // openat(AT_FDCWD, path, O_RDONLY)
    }
    pub unsafe fn read(fd: i32, buf: *mut u8, n: u64) -> i64 {
        sys3(0, fd as i64, buf as i64, n as i64)
    }
    pub unsafe fn close(fd: i32) -> i64 {
        sys3(3, fd as i64, 0, 0)
    }
    pub unsafe fn exit_group(code: i32) -> ! {
        sys3(231, code as i64, 0, 0);
        loop {
            core::hint::spin_loop();
        }
    }
    pub unsafe fn write_stderr(buf: *const u8, n: usize) {
        sys3(1, 2, buf as i64, n as i64);
    }

    pub const O_DIRECTORY: i64 = 0o200000;

    pub unsafe fn open(path: *const u8, flags: i64) -> i64 {
        sys4(257, -100, path as i64, flags, 0)
    }
    pub unsafe fn readlink(path: *const u8, buf: *mut u8, n: usize) -> i64 {
        sys4(267, -100, path as i64, buf as i64, n as i64)
    }
    pub unsafe fn access_x(path: *const u8) -> i64 {
        sys3(269, -100, path as i64, 1)
    }
    pub unsafe fn mmap_anon(len: usize) -> i64 {
        let ret;
        asm!("syscall", inlateout("rax") 9i64 => ret,
            in("rdi") 0i64, in("rsi") len as i64, in("rdx") 3i64,
            in("r10") 0x22i64, in("r8") -1i64, in("r9") 0i64,
            out("rcx") _, out("r11") _);
        ret
    }
    pub unsafe fn munmap(addr: *mut u8, len: usize) {
        sys3(11, addr as i64, len as i64, 0);
    }
}

#[cfg(target_arch = "aarch64")]
#[cfg_attr(test, allow(dead_code))]
mod sys {
    use core::arch::asm;

    unsafe fn svc4(nr: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
        let ret;
        asm!("svc #0", inlateout("x0") a => ret,
            in("x1") b, in("x2") c, in("x3") d, in("x8") nr);
        ret
    }
    pub unsafe fn openat_rdonly(path: *const u8) -> i64 {
        svc4(56, -100, path as i64, 0, 0) // openat(AT_FDCWD, path, O_RDONLY)
    }
    pub unsafe fn read(fd: i32, buf: *mut u8, n: u64) -> i64 {
        svc4(63, fd as i64, buf as i64, n as i64, 0)
    }
    pub unsafe fn close(fd: i32) -> i64 {
        svc4(57, fd as i64, 0, 0, 0)
    }
    pub unsafe fn exit_group(code: i32) -> ! {
        svc4(94, code as i64, 0, 0, 0);
        loop {
            core::hint::spin_loop();
        }
    }
    pub unsafe fn write_stderr(buf: *const u8, n: usize) {
        svc4(64, 2, buf as i64, n as i64, 0);
    }

    pub const O_DIRECTORY: i64 = 0o40000;

    pub unsafe fn open(path: *const u8, flags: i64) -> i64 {
        svc4(56, -100, path as i64, flags, 0)
    }
    pub unsafe fn readlink(path: *const u8, buf: *mut u8, n: usize) -> i64 {
        svc4(78, -100, path as i64, buf as i64, n as i64)
    }
    pub unsafe fn access_x(path: *const u8) -> i64 {
        svc4(48, -100, path as i64, 1, 0)
    }
    pub unsafe fn mmap_anon(len: usize) -> i64 {
        let ret;
        asm!("svc #0", inlateout("x0") 0i64 => ret,
            in("x1") len as i64, in("x2") 3i64, in("x3") 0x22i64,
            in("x4") -1i64, in("x5") 0i64, in("x8") 222i64);
        ret
    }
    pub unsafe fn munmap(addr: *mut u8, len: usize) {
        svc4(215, addr as i64, len as i64, 0, 0);
    }
}

#[cfg(target_arch = "x86")]
#[cfg_attr(test, allow(dead_code))]
mod sys {
    use core::arch::asm;

    // On x86-32 both `ebx` (PIC base) and `esi` are reserved by LLVM and can't
    // be named. Keeping to ebx/ecx/edx covers 3-arg syscalls; the first arg is
    // swapped through ebx around `int 0x80` (which preserves ecx/edx).
    unsafe fn sys3(nr: i32, a: i32, b: i32, c: i32) -> i32 {
        let ret;
        asm!(
            "xchg {a}, ebx",
            "int 0x80",
            "xchg {a}, ebx",
            a = inout(reg) a => _,
            inlateout("eax") nr => ret,
            in("ecx") b, in("edx") c,
        );
        ret
    }
    pub unsafe fn openat_rdonly(path: *const u8) -> i64 {
        // open (not openat): env.rs only opens absolute paths, so AT_FDCWD is
        // unneeded, and open takes 3 args (no esi).
        sys3(5, path as i32, 0, 0) as i64 // open(path, O_RDONLY, 0)
    }
    pub unsafe fn read(fd: i32, buf: *mut u8, n: u64) -> i64 {
        sys3(3, fd, buf as i32, n as i32) as i64
    }
    pub unsafe fn close(fd: i32) -> i64 {
        sys3(6, fd, 0, 0) as i64
    }
    pub unsafe fn exit_group(code: i32) -> ! {
        sys3(252, code, 0, 0);
        loop {
            core::hint::spin_loop();
        }
    }
    pub unsafe fn write_stderr(buf: *const u8, n: usize) {
        sys3(4, 2, buf as i32, n as i32);
    }

    // The path-based calls rather than the `*at` ones, which take a
    // fourth argument in `esi`; every path given here is absolute or
    // meant relative to the working directory anyway.
    pub const O_DIRECTORY: i64 = 0o200000;

    pub unsafe fn open(path: *const u8, flags: i64) -> i64 {
        sys3(5, path as i32, flags as i32, 0) as i64
    }
    pub unsafe fn readlink(path: *const u8, buf: *mut u8, n: usize) -> i64 {
        sys3(85, path as i32, buf as i32, n as i32) as i64
    }
    pub unsafe fn access_x(path: *const u8) -> i64 {
        sys3(33, path as i32, 1, 0) as i64
    }
    /// The old `mmap`, which takes its six arguments through memory.
    pub unsafe fn mmap_anon(len: usize) -> i64 {
        let args: [u32; 6] = [0, len as u32, 3, 0x22, u32::MAX, 0];
        let ret = sys3(90, args.as_ptr() as i32, 0, 0);
        // An address in the upper half is still an address; only the
        // last page's worth of values are errors.
        if (-4095..0).contains(&ret) {
            ret as i64
        } else {
            ret as u32 as i64
        }
    }
    pub unsafe fn munmap(addr: *mut u8, len: usize) {
        sys3(91, addr as i32, len as i32, 0);
    }
}

// ---- static scratch buffers (constructor runs single-threaded) ------------

const MAPS_CAP: usize = 65536;
const ROOT_CAP: usize = 4096;
const BUF_CAP: usize = 65536;
/// The evaluated per-view environment input, bounded by the same cap the
/// runtime enforces when it generates the file.
const RENV_CAP: usize = rtenv::MAX_RECORD_BYTES;

static mut G_MAPS: [u8; MAPS_CAP] = [0; MAPS_CAP];
static mut G_ROOT: [u8; ROOT_CAP] = [0; ROOT_CAP];
/// `.onelf/env` or `.onelf/preload`, with the extra byte [`read_bounded`]
/// needs to tell a file of exactly `BUF_CAP` bytes from a longer one.
static mut G_BUF: [u8; BUF_CAP + 1] = [0; BUF_CAP + 1];
static mut G_RENV: [u8; RENV_CAP + 1] = [0; RENV_CAP + 1];

// ---- tiny freestanding helpers --------------------------------------------

unsafe fn slen(s: *const u8) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// Read a whole file into `buf`, which must hold `cap` bytes plus one.
/// Returns the byte count, [`READ_ABSENT`] when the file cannot be opened,
/// [`READ_IO`] on a read error, or [`READ_OVERFLOW`] when the file is
/// longer than `cap`: an oversized file is refused, never truncated.
unsafe fn read_bounded(path: *const u8, buf: *mut u8, cap: usize) -> i64 {
    let fd = sys::openat_rdonly(path);
    if fd < 0 {
        return READ_ABSENT;
    }
    let mut off = 0usize;
    let mut failed = false;
    while off <= cap {
        let r = sys::read(fd as i32, buf.add(off), (cap + 1 - off) as u64);
        if r < 0 {
            failed = true;
            break;
        }
        if r == 0 {
            break;
        }
        off += r as usize;
    }
    sys::close(fd as i32);
    if failed {
        return READ_IO;
    }
    if off > cap {
        return READ_OVERFLOW;
    }
    off as i64
}

/// [`read_bounded`] result: the file cannot be opened.
const READ_ABSENT: i64 = -1;
/// [`read_bounded`] result: the file is longer than the cap.
const READ_OVERFLOW: i64 = -2;
/// [`read_bounded`] result: a read failed part way through.
const READ_IO: i64 = -3;

/// Copy `src[..n]` into `dst` (capacity `cap`, NUL-terminated). Returns the
/// length written, or -1 on overflow.
unsafe fn scopy(dst: *mut u8, cap: usize, src: *const u8, n: usize) -> i64 {
    if n + 1 > cap {
        return -1;
    }
    let mut i = 0;
    while i < n {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
    *dst.add(n) = 0;
    n as i64
}

/// NUL-terminated string equality.
unsafe fn seq(a: *const u8, b: *const u8) -> bool {
    let mut i = 0;
    loop {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return false;
        }
        if x == 0 {
            return true;
        }
        i += 1;
    }
}

/// Parse a hex number starting at `s[*i]`, advancing `*i`.
unsafe fn parse_hex(s: *const u8, i: &mut usize) -> u64 {
    let mut v = 0u64;
    loop {
        let c = *s.add(*i);
        let d = if c.is_ascii_digit() {
            (c - b'0') as u64
        } else if (b'a'..=b'f').contains(&c) {
            (c - b'a' + 10) as u64
        } else if (b'A'..=b'F').contains(&c) {
            (c - b'A' + 10) as u64
        } else {
            break;
        };
        v = (v << 4) | d;
        *i += 1;
    }
    v
}

/// Expand `${NAME}` / `${NAME:-word}` in `src[..n]` into `dst` (capacity
/// `cap`, NUL-terminated). `${ONELF_DIR}` -> package root; other names ->
/// `getenv`. If the name is unset or empty and a `:-word` default is present,
/// the literal word is substituted. Returns false on overflow.
unsafe fn expand(dst: *mut u8, cap: usize, src: *const u8, n: usize, root: *const u8) -> bool {
    let mut o = 0usize;
    let mut i = 0usize;
    while i < n {
        if i + 1 < n && *src.add(i) == b'$' && *src.add(i + 1) == b'{' {
            let mut j = i + 2;
            while j < n && *src.add(j) != b'}' {
                j += 1;
            }
            if j < n {
                let (ts, te) = (i + 2, j);
                let mut name_end = te;
                let mut def_start = 0usize;
                let mut has_def = false;
                let mut p = ts;
                while p + 1 < te {
                    if *src.add(p) == b':' && *src.add(p + 1) == b'-' {
                        name_end = p;
                        def_start = p + 2;
                        has_def = true;
                        break;
                    }
                    p += 1;
                }
                let mut name = [0u8; 256];
                let nl = name_end - ts;
                if nl < name.len() {
                    let mut k = 0;
                    while k < nl {
                        name[k] = *src.add(ts + k);
                        k += 1;
                    }
                    name[nl] = 0;
                    let rep: *const u8 = if seq(name.as_ptr(), c"ONELF_DIR".as_ptr().cast()) {
                        root
                    } else {
                        getenv(name.as_ptr())
                    };
                    if !rep.is_null() && *rep != 0 {
                        let mut k = 0;
                        while *rep.add(k) != 0 {
                            if o + 1 >= cap {
                                return false;
                            }
                            *dst.add(o) = *rep.add(k);
                            o += 1;
                            k += 1;
                        }
                    } else if has_def {
                        let mut k = def_start;
                        while k < te {
                            if o + 1 >= cap {
                                return false;
                            }
                            *dst.add(o) = *src.add(k);
                            o += 1;
                            k += 1;
                        }
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        if o + 1 >= cap {
            return false;
        }
        *dst.add(o) = *src.add(i);
        o += 1;
        i += 1;
    }
    if o >= cap {
        return false;
    }
    *dst.add(o) = 0;
    true
}

/// Trim ASCII whitespace, returning the adjusted `(start, end)` byte offsets.
unsafe fn trim(base: *const u8, mut s: usize, mut e: usize) -> (usize, usize) {
    while s < e {
        let c = *base.add(s);
        if c == b' ' || c == b'\t' || c == b'\r' {
            s += 1;
        } else {
            break;
        }
    }
    while e > s {
        let c = *base.add(e - 1);
        if c == b' ' || c == b'\t' || c == b'\r' {
            e -= 1;
        } else {
            break;
        }
    }
    (s, e)
}

// ---- self-location + application --------------------------------------------

/// Find the package root: the nearest parent dir of this object containing a
/// `.onelf/`. Writes it into `G_ROOT`, returns true on success.
unsafe fn find_root() -> bool {
    let self_addr = onelf_env_init as *const () as usize as u64;

    // Read in chunks and scan complete lines as they arrive, carrying a
    // partial last line over to the next read. The whole file does not fit
    // the buffer for a large application (an Electron app maps a few
    // hundred libraries before any constructor runs), and this object is
    // an early dependency at a high address, so it sits near the end.
    let maps = &raw mut G_MAPS as *mut u8;
    let fd = sys::openat_rdonly(c"/proc/self/maps".as_ptr().cast());
    if fd < 0 {
        return false;
    }
    let mut have = 0usize;
    let mut eof = false;
    let mut so = 0usize;
    let mut so_len = 0usize;
    let mut found = false;
    while !found {
        if !eof {
            let r = sys::read(fd as i32, maps.add(have), (MAPS_CAP - 1 - have) as u64);
            if r <= 0 {
                eof = true;
            } else {
                have += r as usize;
            }
        }
        *maps.add(have) = 0;

        // Scan lines: "start-end perms off dev inode  /path/to/lib.so".
        let mut line = 0usize;
        while line < have {
            let mut eol = line;
            while eol < have && *maps.add(eol) != b'\n' {
                eol += 1;
            }
            if eol == have && !eof {
                break; // partial line: wait for the rest
            }
            let mut p = line;
            let start = parse_hex(maps, &mut p);
            let mut end = 0u64;
            if *maps.add(p) == b'-' {
                p += 1;
                end = parse_hex(maps, &mut p);
            }
            if self_addr >= start && self_addr < end {
                let mut path = p;
                while path < eol && *maps.add(path) != b'/' {
                    path += 1;
                }
                if path < eol {
                    so = path;
                    so_len = eol - path;
                    found = true;
                }
                break;
            }
            line = eol + 1;
        }
        if found || eof {
            break;
        }
        if line == 0 && have == MAPS_CAP - 1 {
            break; // one line longer than the buffer: not a maps file we know
        }
        // Keep the partial line, moved to the front.
        let mut k = 0usize;
        while line + k < have {
            *maps.add(k) = *maps.add(line + k);
            k += 1;
        }
        have = k;
    }
    sys::close(fd as i32);
    if !found || so_len == 0 {
        return false;
    }

    // Walk up parent dirs looking for a child `.onelf/env` or `.onelf/preload`.
    let mut cand = [0u8; 4096];
    if scopy(cand.as_mut_ptr(), cand.len(), maps.add(so), so_len) < 0 {
        return false;
    }

    for _ in 0..10 {
        let mut l = slen(cand.as_ptr());
        while l > 1 && cand[l - 1] != b'/' {
            l -= 1;
        }
        if l <= 1 {
            break;
        }
        cand[l - 1] = 0; // drop the slash -> directory path

        let cl = slen(cand.as_ptr());
        for suffix in [
            b"/.onelf/env\0".as_slice(),
            b"/.onelf/preload\0".as_slice(),
            b"/.onelf/runtime-env\0".as_slice(),
        ] {
            let mut probe = [0u8; 4096];
            if cl + suffix.len() > probe.len() {
                continue;
            }
            let mut k = 0;
            while k < cl {
                probe[k] = cand[k];
                k += 1;
            }
            let mut m = 0;
            while m < suffix.len() {
                probe[cl + m] = suffix[m];
                m += 1;
            }
            let fd = sys::openat_rdonly(probe.as_ptr());
            if fd >= 0 {
                sys::close(fd as i32);
                let root = &raw mut G_ROOT as *mut u8;
                return scopy(root, ROOT_CAP, cand.as_ptr(), cl) >= 0;
            }
        }
    }
    false
}

/// Read `<root>/.onelf/<name>` into `buf` (capacity `cap`, plus one byte
/// for the overflow check, see [`read_bounded`]).
unsafe fn read_onelf_file(name: &[u8], buf: *mut u8, cap: usize) -> i64 {
    let root = &raw const G_ROOT as *const u8;
    let mut path = [0u8; 4096];
    let rl = slen(root);
    if rl + name.len() > path.len() {
        return READ_ABSENT;
    }
    let mut i = 0;
    while i < rl {
        path[i] = *root.add(i);
        i += 1;
    }
    let mut m = 0;
    while m < name.len() {
        path[rl + m] = name[m];
        m += 1;
    }
    read_bounded(path.as_ptr(), buf, cap)
}

/// One line of a diagnostic on stderr.
unsafe fn warn(message: &[u8]) {
    sys::write_stderr(message.as_ptr(), message.len());
}

/// Apply the launch's evaluated per-view input, `.onelf/runtime-env`.
///
/// A record that is missing is nothing to do: packages the constructor
/// cannot serve a record for, and views that publish none, keep working
/// from `.onelf/env` alone. A record that is oversized or malformed is
/// rejected whole, with a diagnostic, and nothing from it is applied:
/// reading it back must not trust or truncate what it did not verify.
unsafe fn apply_runtime_env() {
    let n = read_onelf_file(
        b"/.onelf/runtime-env\0",
        &raw mut G_RENV as *mut u8,
        RENV_CAP,
    );
    match n {
        n if n < 0 => {
            if n == READ_OVERFLOW {
                const MSG: &[u8] =
                    b"onelf-env: .onelf/runtime-env is beyond the record cap and is not applied\n";
                warn(MSG);
            } else if n == READ_IO {
                const MSG: &[u8] =
                    b"onelf-env: .onelf/runtime-env could not be read and is not applied\n";
                warn(MSG);
            }
        }
        n => {
            let bytes = core::slice::from_raw_parts(&raw const G_RENV as *const u8, n as usize);
            if let Err(reject) = rtenv::apply(bytes) {
                const A: &[u8] = b"onelf-env: .onelf/runtime-env rejected: ";
                const B: &[u8] = b", nothing applied from it\n";
                warn(A);
                let reason = reject.reason().as_bytes();
                warn(reason);
                warn(B);
            }
        }
    }
}

unsafe fn apply_env() {
    let n = read_onelf_file(b"/.onelf/env\0", &raw mut G_BUF as *mut u8, BUF_CAP);
    match n {
        n if n < 0 => {
            if n == READ_OVERFLOW {
                const MSG: &[u8] =
                    b"onelf-env: .onelf/env is beyond the buffer cap and is not applied\n";
                warn(MSG);
            } else if n == READ_IO {
                const MSG: &[u8] = b"onelf-env: .onelf/env could not be read and is not applied\n";
                warn(MSG);
            }
        }
        n => {
            let bytes = core::slice::from_raw_parts(&raw const G_BUF as *const u8, n as usize);
            apply_env_bytes(bytes)
        }
    }
}

/// Apply `.onelf/env` bytes: one `KEY=VALUE` per line, `${ONELF_DIR}` and
/// `${VAR:-word}` expanded against the live environment, a value that
/// wraps its own variable left alone once applied. Recipe values are
/// applied after the generated record and overwrite it, so what the
/// package declares itself still wins.
unsafe fn apply_env_bytes(bytes: &[u8]) {
    let buf = bytes.as_ptr();
    let root = &raw const G_ROOT as *const u8;
    let end = bytes.len();
    let mut cur = 0usize;
    while cur < end {
        let ls = cur;
        let mut le = cur;
        while le < end && *buf.add(le) != b'\n' {
            le += 1;
        }
        cur = if le < end { le + 1 } else { end };

        let (s, e) = trim(buf, ls, le);
        if s >= e || *buf.add(s) == b'#' {
            continue;
        }
        let mut eq = s;
        while eq < e && *buf.add(eq) != b'=' {
            eq += 1;
        }
        if eq >= e {
            continue;
        }
        let (ks, ke) = trim(buf, s, eq);
        if ks >= ke {
            continue;
        }
        let (vs, ve) = trim(buf, eq + 1, e);

        let mut key = [0u8; 1024];
        if scopy(key.as_mut_ptr(), key.len(), buf.add(ks), ke - ks) < 0 {
            continue;
        }
        if already_applied(key.as_ptr(), buf.add(vs), ve - vs, root) {
            continue;
        }
        let mut val = [0u8; 8192];
        if !expand(val.as_mut_ptr(), val.len(), buf.add(vs), ve - vs, root) {
            continue;
        }
        setenv(key.as_ptr(), val.as_ptr(), 1);
    }
}

/// `n` bytes at `a` equal those at `b`.
unsafe fn bytes_eq(a: *const u8, b: *const u8, n: usize) -> bool {
    let mut i = 0;
    while i < n {
        if *a.add(i) != *b.add(i) {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `key` already holds what the value `src[..n]` wraps around its
/// own `${key}` or `${key:-word}`.
///
/// The default `${ONELF_DIR}/bin:${PATH}` refers to the variable it sets.
/// The runtime applies it before this constructor runs, and every exec of
/// the entrypoint runs the constructor again, so without this each one
/// would add the prefix once more. A live value that already starts and
/// ends with what surrounds the reference has had it applied.
unsafe fn already_applied(key: *const u8, src: *const u8, n: usize, root: *const u8) -> bool {
    let key_len = slen(key);
    let mut i = 0usize;
    while i + 1 < n {
        if *src.add(i) != b'$' || *src.add(i + 1) != b'{' {
            i += 1;
            continue;
        }
        let mut close = i + 2;
        while close < n && *src.add(close) != b'}' {
            close += 1;
        }
        if close >= n {
            return false;
        }
        let mut name_end = i + 2;
        while name_end + 1 < close
            && !(*src.add(name_end) == b':' && *src.add(name_end + 1) == b'-')
        {
            name_end += 1;
        }
        if name_end + 1 >= close {
            name_end = close;
        }
        if name_end - (i + 2) == key_len && bytes_eq(src.add(i + 2), key, key_len) {
            return wraps_current(key, src, i, src.add(close + 1), n - close - 1, root);
        }
        i = close + 1;
    }
    false
}

/// Whether the live value of `key` starts with the expansion of
/// `before[..before_n]` and ends with that of `after[..after_n]`.
unsafe fn wraps_current(
    key: *const u8,
    before: *const u8,
    before_n: usize,
    after: *const u8,
    after_n: usize,
    root: *const u8,
) -> bool {
    let current = getenv(key);
    if current.is_null() || *current == 0 {
        return false;
    }
    let mut prefix = [0u8; 4096];
    let mut suffix = [0u8; 4096];
    if !expand(prefix.as_mut_ptr(), prefix.len(), before, before_n, root)
        || !expand(suffix.as_mut_ptr(), suffix.len(), after, after_n, root)
    {
        return false;
    }
    let (cl, pl, sl) = (slen(current), slen(prefix.as_ptr()), slen(suffix.as_ptr()));
    cl >= pl + sl
        && bytes_eq(current, prefix.as_ptr(), pl)
        && bytes_eq(current.add(cl - sl), suffix.as_ptr(), sl)
}

unsafe fn apply_preload() {
    let n = read_onelf_file(b"/.onelf/preload\0", &raw mut G_BUF as *mut u8, BUF_CAP);
    match n {
        n if n < 0 => {
            if n == READ_OVERFLOW {
                const MSG: &[u8] =
                    b"onelf-env: .onelf/preload is beyond the buffer cap and is not applied\n";
                warn(MSG);
            } else if n == READ_IO {
                const MSG: &[u8] =
                    b"onelf-env: .onelf/preload could not be read and is not applied\n";
                warn(MSG);
            }
            return;
        }
        _ => {}
    }
    if n == 0 {
        return;
    }
    if !have_dlopen() {
        const MSG: &[u8] = b"onelf-env: dlopen is not in this process (glibc before 2.34 keeps it in libdl.so.2), .onelf/preload not applied\n";
        sys::write_stderr(MSG.as_ptr(), MSG.len());
        return;
    }
    let buf = &raw const G_BUF as *const u8;
    let root = &raw const G_ROOT as *const u8;
    let end = n as usize;
    let mut cur = 0usize;
    while cur < end {
        let ls = cur;
        let mut le = cur;
        while le < end && *buf.add(le) != b'\n' {
            le += 1;
        }
        cur = if le < end { le + 1 } else { end };

        let (s, e) = trim(buf, ls, le);
        if s >= e || *buf.add(s) == b'#' {
            continue;
        }
        let mut lib = [0u8; 8192];
        if !expand(lib.as_mut_ptr(), lib.len(), buf.add(s), e - s, root) {
            continue;
        }
        dlopen(lib.as_ptr(), RTLD_NOW | RTLD_GLOBAL);
    }
}

extern "C" fn onelf_env_init() {
    unsafe {
        exec::resolve();
        if !find_root() {
            return;
        }
        apply_runtime_env();
        apply_env();
        apply_preload();
    }
}

/// Register the constructor: the loader runs `.init_array` entries before
/// main() on every load of this object.
#[used]
#[unsafe(link_section = ".init_array")]
static ONELF_ENV_CTOR: extern "C" fn() = onelf_env_init;

#[cfg(test)]
mod tests {
    use super::{READ_ABSENT, READ_OVERFLOW, apply_env, apply_runtime_env, read_bounded};
    use crate::harness::{
        c_path, env, env_lock, runtime_entry, runtime_record, set_env, set_root, unset_env, view,
    };
    use crate::rtenv::Policy;

    /// `bytes` written to `<view>/.onelf/<name>`.
    fn write_onelf(view_dir: &std::path::Path, name: &str, bytes: &[u8]) {
        std::fs::write(view_dir.join(".onelf").join(name), bytes).unwrap();
    }

    #[test]
    fn a_recipe_value_wins_over_a_generated_mapping() {
        let _guard = env_lock();
        let view_dir = view("precedence");
        let recipe = b"ONELF_T_PRECEDENCE=recipe\n";
        write_onelf(&view_dir, "env", recipe);
        unsafe {
            unset_env("ONELF_T_PRECEDENCE");
            set_root(&view_dir);
            let record = runtime_record(
                &[runtime_entry(
                    "ONELF_T_PRECEDENCE",
                    Policy::Replace,
                    b"generated",
                    true,
                )],
                None,
                None,
            );
            crate::rtenv::apply(&record).unwrap();
            assert_eq!(env("ONELF_T_PRECEDENCE").as_deref(), Some("generated"));
            apply_env();
        }
        assert_eq!(env("ONELF_T_PRECEDENCE").as_deref(), Some("recipe"));
        unsafe { unset_env("ONELF_T_PRECEDENCE") };
        let _ = std::fs::remove_dir_all(&view_dir);
    }

    #[test]
    fn a_self_referential_path_is_applied_once() {
        let _guard = env_lock();
        let view_dir = view("path-once");
        let recipe = b"PATH=${ONELF_DIR}/bin:${PATH:-/usr/bin:/bin}\n";
        write_onelf(&view_dir, "env", recipe);
        unsafe {
            set_env("PATH", "/usr/bin:/bin");
            set_root(&view_dir);
            apply_env();
            let once = env("PATH");
            apply_env();
            assert_eq!(env("PATH"), once);
        }
        assert_eq!(
            env("PATH").as_deref(),
            Some(format!("{}/bin:/usr/bin:/bin", view_dir.display()).as_str())
        );
        unsafe { unset_env("PATH") };
        let _ = std::fs::remove_dir_all(&view_dir);
    }

    #[test]
    fn an_oversized_env_file_is_refused_not_truncated() {
        let _guard = env_lock();
        let view_dir = view("oversize-env");
        let mut big = b"ONELF_T_OVERSIZE_ENV=yes\n".to_vec();
        big.resize(super::BUF_CAP + 4096, b'#');
        big.push(b'\n');
        write_onelf(&view_dir, "env", &big);
        unsafe {
            unset_env("ONELF_T_OVERSIZE_ENV");
            set_root(&view_dir);
            apply_env();
        }
        assert_eq!(env("ONELF_T_OVERSIZE_ENV"), None);
        let _ = std::fs::remove_dir_all(&view_dir);
    }

    #[test]
    fn a_generated_record_in_the_view_is_applied() {
        let _guard = env_lock();
        let view_dir = view("record-view");
        let value = b"/view/lib/qt6/plugins";
        let record = runtime_record(
            &[runtime_entry(
                "ONELF_T_FROM_VIEW",
                Policy::Replace,
                value,
                true,
            )],
            Some(b"/view"),
            None,
        );
        write_onelf(&view_dir, "runtime-env", &record);
        unsafe {
            unset_env("ONELF_T_FROM_VIEW");
            unset_env("ONELF_ROOTS");
            set_root(&view_dir);
            apply_runtime_env();
        }
        assert_eq!(
            env("ONELF_T_FROM_VIEW").as_deref(),
            Some("/view/lib/qt6/plugins")
        );
        assert_eq!(env("ONELF_ROOTS").as_deref(), Some("/view"));
        unsafe {
            unset_env("ONELF_T_FROM_VIEW");
            unset_env("ONELF_ROOTS");
        }
        let _ = std::fs::remove_dir_all(&view_dir);
    }

    #[test]
    fn a_garbled_record_in_the_view_applies_nothing() {
        let _guard = env_lock();
        let view_dir = view("garbled-view");
        write_onelf(&view_dir, "runtime-env", b"ONELF-RUNTIME-ENV 1 1\nA=!1:v");
        unsafe {
            unset_env("A");
            set_root(&view_dir);
            apply_runtime_env();
        }
        assert_eq!(env("A"), None);
        let _ = std::fs::remove_dir_all(&view_dir);
    }

    #[test]
    fn a_file_beyond_the_cap_is_refused_not_truncated() {
        let view_dir = view("read-cap");
        let path = view_dir.join("nine");
        std::fs::write(&path, b"123456789").unwrap();
        let mut buf = [0u8; 9];
        let got = unsafe { read_bounded(c_path(&path).as_ptr(), buf.as_mut_ptr(), 8) };
        assert_eq!(got, READ_OVERFLOW);
        std::fs::write(&path, b"12345678").unwrap();
        let got = unsafe { read_bounded(c_path(&path).as_ptr(), buf.as_mut_ptr(), 8) };
        assert_eq!(got, 8);
        assert_eq!(&buf[..8], b"12345678");
        let got = unsafe {
            read_bounded(
                c_path(&view_dir.join("absent")).as_ptr(),
                buf.as_mut_ptr(),
                8,
            )
        };
        assert_eq!(got, READ_ABSENT);
        let _ = std::fs::remove_dir_all(&view_dir);
    }
}

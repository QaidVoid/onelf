//! onelf payload-side environment constructor (freestanding).
//!
//! Built per target to a tiny `-nostdlib` cdylib with no `DT_NEEDED` of its
//! own, bundled into a package's lib/ and injected as a `DT_NEEDED` of the
//! entrypoint. Its `.init_array` constructor runs on every exec (surviving a
//! sandboxed `clearenv()` + re-exec): it self-locates via `/proc/self/maps`,
//! walks to the package root (the dir containing `.onelf/`), and re-applies
//! `.onelf/env` (KEY=VALUE, `${ONELF_DIR}`/`${VAR:-word}` expanded) and
//! `.onelf/preload` (one dlopen'd lib per line).
//!
//! `setenv`/`dlopen`/`getenv` are left UNDEFINED and resolve from the
//! application's own libc at load time, so one per-arch blob serves both
//! glibc and musl. Ported from the former `payload/onelf_env.c`.
#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    // The constructor must never unwind into the loader; just stop.
    loop {
        core::hint::spin_loop();
    }
}

// Resolved from the application's libc at load time (kept UNDEF here).
unsafe extern "C" {
    fn setenv(name: *const u8, value: *const u8, overwrite: i32) -> i32;
    fn dlopen(filename: *const u8, flags: i32) -> *mut core::ffi::c_void;
    fn getenv(name: *const u8) -> *const u8;
}

const RTLD_NOW: i32 = 0x0002;
const RTLD_GLOBAL: i32 = 0x0100;

// The code below uses only byte loops, so LLVM emits no mem* intrinsics. Were
// any emitted, they would stay UNDEF and resolve from the app's libc at load
// time (like setenv/dlopen), so no local definition is needed here.

// ---- raw syscalls ---------------------------------------------------------

#[cfg(target_arch = "x86_64")]
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
}

#[cfg(target_arch = "aarch64")]
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
}

#[cfg(target_arch = "x86")]
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
}

// ---- static scratch buffers (constructor runs single-threaded) ------------

const MAPS_CAP: usize = 65536;
const ROOT_CAP: usize = 4096;
const BUF_CAP: usize = 65536;

static mut G_MAPS: [u8; MAPS_CAP] = [0; MAPS_CAP];
static mut G_ROOT: [u8; ROOT_CAP] = [0; ROOT_CAP];
static mut G_BUF: [u8; BUF_CAP] = [0; BUF_CAP];

// ---- tiny freestanding helpers --------------------------------------------

unsafe fn slen(s: *const u8) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// Read a whole file into `buf` (capacity `cap`). Returns byte count, or -1 on
/// open failure. Silently truncates past `cap` (env/preload files are tiny).
unsafe fn read_file(path: *const u8, buf: *mut u8, cap: usize) -> i64 {
    let fd = sys::openat_rdonly(path);
    if fd < 0 {
        return -1;
    }
    let mut off = 0usize;
    while off < cap {
        let r = sys::read(fd as i32, buf.add(off), (cap - off) as u64);
        if r <= 0 {
            break;
        }
        off += r as usize;
    }
    sys::close(fd as i32);
    off as i64
}

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
                    let rep: *const u8 = if seq(name.as_ptr(), b"ONELF_DIR\0".as_ptr()) {
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
    let self_addr = onelf_env_init as usize as u64;

    let maps = &raw mut G_MAPS as *mut u8;
    let n = read_file(b"/proc/self/maps\0".as_ptr(), maps, MAPS_CAP - 1);
    if n <= 0 {
        return false;
    }
    let n = n as usize;
    *maps.add(n) = 0;

    // Scan lines: "start-end perms off dev inode  /path/to/lib.so".
    let mut line = 0usize;
    let mut so = 0usize;
    let mut so_len = 0usize;
    let mut found = false;
    while *maps.add(line) != 0 {
        let mut p = line;
        let start = parse_hex(maps, &mut p);
        let mut end = 0u64;
        if *maps.add(p) == b'-' {
            p += 1;
            end = parse_hex(maps, &mut p);
        }
        let mut eol = p;
        while *maps.add(eol) != 0 && *maps.add(eol) != b'\n' {
            eol += 1;
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
        line = if *maps.add(eol) == b'\n' {
            eol + 1
        } else {
            eol
        };
    }
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
        for suffix in [b"/.onelf/env\0".as_slice(), b"/.onelf/preload\0".as_slice()] {
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

/// Read `<root>/.onelf/<name>` into `G_BUF`; returns the byte count or <=0.
unsafe fn read_onelf_file(name: &[u8]) -> i64 {
    let root = &raw const G_ROOT as *const u8;
    let mut path = [0u8; 4096];
    let rl = slen(root);
    if rl + name.len() > path.len() {
        return 0;
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
    read_file(path.as_ptr(), &raw mut G_BUF as *mut u8, BUF_CAP)
}

unsafe fn apply_env() {
    let n = read_onelf_file(b"/.onelf/env\0");
    if n <= 0 {
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
        let mut val = [0u8; 8192];
        if !expand(val.as_mut_ptr(), val.len(), buf.add(vs), ve - vs, root) {
            continue;
        }
        setenv(key.as_ptr(), val.as_ptr(), 1);
    }
}

unsafe fn apply_preload() {
    let n = read_onelf_file(b"/.onelf/preload\0");
    if n <= 0 {
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
        if !find_root() {
            return;
        }
        apply_env();
        apply_preload();
    }
}

/// Register the constructor: the loader runs `.init_array` entries before
/// main() on every load of this object.
#[used]
#[unsafe(link_section = ".init_array")]
static ONELF_ENV_CTOR: extern "C" fn() = onelf_env_init;

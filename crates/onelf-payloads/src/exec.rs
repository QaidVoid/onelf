//! Exec and spawn hooks that give a host program the caller's environment.
//!
//! The runtime changes the environment for the application: the
//! package's `bin/` first on `PATH`, toolkit module paths into the
//! package, a portable `HOME`. A program outside the package must not
//! inherit any of that. This object defines `execve`, `execv`, `execvp`,
//! `execvpe`, `posix_spawn` and `posix_spawnp`, and since the packer
//! places it ahead of libc in the executable's `DT_NEEDED` list, its
//! definitions are the ones every object in the process binds to. Each
//! resolves the target the way the real call will, and a target outside
//! the package and its dependency sets is started with the values that
//! `ONELF_HOST_ENV` records put back.
//!
//! `ONELF_HOST_ENV` holds one entry per variable the runtime changed:
//!
//! ```text
//! NAME=<applied><original>;
//! applied:  16 hex digits    FNV-1a hash of the value the runtime set
//!           -                the runtime removed the variable
//!           *                restore whatever the variable holds
//! original: <len>:<bytes>    the value the runtime was started with
//!           -                it was unset
//! ```
//!
//! A variable is put back only while it still holds what the runtime
//! applied. One the application or the caller of the hook changed is a
//! deliberate choice and is passed on as it is. `ONELF_ROOTS` lists the
//! package root and the dependency set roots, `:`-separated and
//! canonical.
//!
//! What cannot be reached: glibc and musl run `system`, `popen` and the
//! `execl` family through internal entry points that no definition here
//! replaces, and `fexecve` and `execveat` name the target by descriptor.
//! Those keep the application's environment.
//!
//! A hook may run in a child made by `vfork`, which shares the parent's
//! memory. It takes no lock and never touches the heap: its buffers live
//! on the stack, with an anonymous mapping for an environment too large
//! for them. The real functions are looked up once, by the constructor,
//! while the process is still single-threaded.

use core::ffi::c_void;
use core::mem::{MaybeUninit, size_of};
use core::ptr::null;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{bytes_eq, getenv, slen, sys};

unsafe extern "C" {
    static environ: *const *const u8;
    fn dl_iterate_phdr(
        callback: unsafe extern "C" fn(*mut DlPhdrInfo, usize, *mut c_void) -> i32,
        data: *mut c_void,
    ) -> i32;
    fn __errno_location() -> *mut i32;
}

const RECORD: &[u8] = b"ONELF_HOST_ENV\0";
const ROOTS: &[u8] = b"ONELF_ROOTS\0";
/// Programs that hand a file or URL to the desktop's choice of
/// application, which is never the package's.
const ALWAYS_HOST: [&[u8]; 2] = [b"xdg-open", b"gio-launch-desktop"];

const ENOSYS: i32 = 38;
const O_PATH: i64 = 0o10000000;
const O_CLOEXEC: i64 = 0o2000000;
const PATH_CAP: usize = 4096;
/// Enough for the environment of an ordinary session; a larger one is
/// built in a mapping instead.
const SCRATCH_WORDS: usize = 1024;

type ExecFn = unsafe extern "C" fn(*const u8, *const *const u8, *const *const u8) -> i32;
type SpawnFn = unsafe extern "C" fn(
    *mut i32,
    *const u8,
    *const c_void,
    *const c_void,
    *const *const u8,
    *const *const u8,
) -> i32;

const EXECVE: usize = 0;
const EXECVPE: usize = 1;
const SPAWN: usize = 2;
const SPAWNP: usize = 3;
const NAMES: [&[u8]; 4] = [
    b"execve\0",
    b"execvpe\0",
    b"posix_spawn\0",
    b"posix_spawnp\0",
];
static REAL: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

// ---- hooks ------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execve(
    path: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    let addr = real(EXECVE);
    if addr == 0 {
        return fail();
    }
    let call: ExecFn = core::mem::transmute(addr);
    let mut scratch = Scratch::new();
    let envp = environment_for(path, envp, &mut scratch);
    call(path, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execv(path: *const u8, argv: *const *const u8) -> i32 {
    execve(path, argv, environ)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execvpe(
    file: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    let addr = real(EXECVPE);
    if addr == 0 {
        return fail();
    }
    let call: ExecFn = core::mem::transmute(addr);
    let mut found = MaybeUninit::<[u8; PATH_CAP]>::uninit();
    let target = search(file, found.as_mut_ptr() as *mut u8);
    let mut scratch = Scratch::new();
    let envp = environment_for(target, envp, &mut scratch);
    call(file, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execvp(file: *const u8, argv: *const *const u8) -> i32 {
    execvpe(file, argv, environ)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn(
    pid: *mut i32,
    path: *const u8,
    actions: *const c_void,
    attr: *const c_void,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    let addr = real(SPAWN);
    if addr == 0 {
        return ENOSYS;
    }
    let call: SpawnFn = core::mem::transmute(addr);
    let mut scratch = Scratch::new();
    let envp = environment_for(path, envp, &mut scratch);
    call(pid, path, actions, attr, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnp(
    pid: *mut i32,
    file: *const u8,
    actions: *const c_void,
    attr: *const c_void,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    let addr = real(SPAWNP);
    if addr == 0 {
        return ENOSYS;
    }
    let call: SpawnFn = core::mem::transmute(addr);
    let mut found = MaybeUninit::<[u8; PATH_CAP]>::uninit();
    let target = search(file, found.as_mut_ptr() as *mut u8);
    let mut scratch = Scratch::new();
    let envp = environment_for(target, envp, &mut scratch);
    call(pid, file, actions, attr, argv, envp)
}

unsafe fn fail() -> i32 {
    *__errno_location() = ENOSYS;
    -1
}

/// The address of real function `which`, looked up now if a hook runs
/// before the constructor did.
unsafe fn real(which: usize) -> usize {
    let addr = REAL[which].load(Ordering::Relaxed);
    if addr != 0 {
        return addr;
    }
    resolve();
    REAL[which].load(Ordering::Relaxed)
}

// ---- choosing the environment -----------------------------------------------

/// Where a hook builds the environment it passes on: the stack when it
/// fits, an anonymous mapping when it does not.
struct Scratch {
    stack: MaybeUninit<[usize; SCRATCH_WORDS]>,
    map: *mut u8,
    map_len: usize,
}

impl Scratch {
    fn new() -> Self {
        Scratch {
            stack: MaybeUninit::uninit(),
            map: core::ptr::null_mut(),
            map_len: 0,
        }
    }

    /// `bytes` of word-aligned memory, or null.
    unsafe fn get(&mut self, bytes: usize) -> *mut u8 {
        if bytes <= size_of::<[usize; SCRATCH_WORDS]>() {
            return self.stack.as_mut_ptr() as *mut u8;
        }
        let addr = sys::mmap_anon(bytes);
        if addr < 0 {
            return core::ptr::null_mut();
        }
        self.map = addr as usize as *mut u8;
        self.map_len = bytes;
        self.map
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.map.is_null() {
            unsafe { sys::munmap(self.map, self.map_len) };
        }
    }
}

/// The environment to start `target` with: `envp` itself for a program
/// of the package, or in a process the runtime left no record in, and
/// otherwise `envp` with the recorded values put back.
unsafe fn environment_for(
    target: *const u8,
    envp: *const *const u8,
    scratch: &mut Scratch,
) -> *const *const u8 {
    let record = getenv(RECORD.as_ptr());
    if record.is_null() || target.is_null() || !is_host_program(target) {
        return envp;
    }
    let rewritten = restored(record, envp, scratch);
    if rewritten.is_null() { envp } else { rewritten }
}

/// Whether `path` names a program outside the package and its sets. One
/// that cannot be resolved is left to the real call, which will fail on
/// it, and keeps the environment it was given.
unsafe fn is_host_program(path: *const u8) -> bool {
    if always_host(path) {
        return true;
    }
    let mut buf = MaybeUninit::<[u8; PATH_CAP]>::uninit();
    let canon = buf.as_mut_ptr() as *mut u8;
    let Some(len) = canonical(path, canon) else {
        return false;
    };
    if always_host(canon) {
        return true;
    }
    let roots = getenv(ROOTS.as_ptr());
    !roots.is_null() && !inside_any(canon, len, roots)
}

unsafe fn always_host(path: *const u8) -> bool {
    let len = slen(path);
    let mut base = len;
    while base > 0 && *path.add(base - 1) != b'/' {
        base -= 1;
    }
    ALWAYS_HOST
        .iter()
        .any(|name| len - base == name.len() && bytes_eq(path.add(base), name.as_ptr(), name.len()))
}

/// Whether `path[..len]` is one of the `:`-separated `roots` or lies
/// below one.
unsafe fn inside_any(path: *const u8, len: usize, roots: *const u8) -> bool {
    let mut start = 0usize;
    loop {
        let mut end = start;
        while *roots.add(end) != 0 && *roots.add(end) != b':' {
            end += 1;
        }
        let rl = end - start;
        if rl > 0
            && len >= rl
            && bytes_eq(path, roots.add(start), rl)
            && (len == rl || *path.add(rl) == b'/')
        {
            return true;
        }
        if *roots.add(end) == 0 {
            return false;
        }
        start = end + 1;
    }
}

/// The path the kernel would execute for `path`, symlinks followed,
/// written NUL-terminated to `out` (`PATH_CAP` bytes). `None` when the
/// file cannot be opened or `/proc` is not there to ask.
unsafe fn canonical(path: *const u8, out: *mut u8) -> Option<usize> {
    let fd = sys::open(path, O_PATH | O_CLOEXEC);
    if fd < 0 {
        return None;
    }
    let mut link = [0u8; 32];
    let prefix = b"/proc/self/fd/";
    let mut n = 0;
    while n < prefix.len() {
        link[n] = prefix[n];
        n += 1;
    }
    let mut digits = [0u8; 20];
    let (mut value, mut count) = (fd as u64, 0);
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        link[n] = digits[count];
        n += 1;
    }
    let got = sys::readlink(link.as_ptr(), out, PATH_CAP - 1);
    sys::close(fd as i32);
    if got <= 0 || got as usize >= PATH_CAP - 1 {
        return None;
    }
    *out.add(got as usize) = 0;
    Some(got as usize)
}

/// The program `execvp` and `posix_spawnp` would run for `file`: the
/// first executable file of that name along `PATH`, written to `buf`
/// (`PATH_CAP` bytes). A name with a slash is used as it is, and so is
/// one the search does not find, which the real call then fails on.
unsafe fn search(file: *const u8, buf: *mut u8) -> *const u8 {
    if file.is_null() || *file == 0 {
        return file;
    }
    let fl = slen(file);
    let mut i = 0;
    while i < fl {
        if *file.add(i) == b'/' {
            return file;
        }
        i += 1;
    }
    let mut path = getenv(b"PATH\0".as_ptr());
    if path.is_null() {
        path = b"/bin:/usr/bin\0".as_ptr();
    }
    let mut start = 0usize;
    loop {
        let mut end = start;
        while *path.add(end) != 0 && *path.add(end) != b':' {
            end += 1;
        }
        let (dir, dl) = if end == start {
            (b".".as_ptr(), 1)
        } else {
            (path.add(start), end - start)
        };
        if dl + 1 + fl < PATH_CAP {
            copy(buf, dir, dl);
            *buf.add(dl) = b'/';
            copy(buf.add(dl + 1), file, fl);
            *buf.add(dl + 1 + fl) = 0;
            if sys::access_x(buf) == 0 && !is_dir(buf) {
                return buf;
            }
        }
        if *path.add(end) == 0 {
            return file;
        }
        start = end + 1;
    }
}

unsafe fn is_dir(path: *const u8) -> bool {
    let fd = sys::open(path, O_PATH | O_CLOEXEC | sys::O_DIRECTORY);
    if fd < 0 {
        return false;
    }
    sys::close(fd as i32);
    true
}

unsafe fn copy(dst: *mut u8, src: *const u8, n: usize) {
    let mut i = 0;
    while i < n {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
}

/// `envp` with every recorded variable that still holds what the
/// runtime applied put back to its original, and the record itself left
/// out. Null when there is no memory to build it in.
unsafe fn restored(
    record: *const u8,
    envp: *const *const u8,
    scratch: &mut Scratch,
) -> *const *const u8 {
    let mut count = 0usize;
    if !envp.is_null() {
        while !(*envp.add(count)).is_null() {
            count += 1;
        }
    }
    let mut entries = 0usize;
    let mut pos = 0usize;
    while parse_entry(record, &mut pos).is_some() {
        entries += 1;
    }
    // A restored `NAME=value` is shorter than its encoded entry, so the
    // record's length bounds the strings.
    let words = count + entries + 1;
    let base = scratch.get(words * size_of::<usize>() + slen(record) + 1);
    if base.is_null() {
        return null();
    }
    let out = base as *mut *const u8;
    let mut strings = base.add(words * size_of::<usize>());
    let mut k = 0usize;

    for i in 0..count {
        let var = *envp.add(i);
        let nl = name_len(var);
        if *var.add(nl) != b'=' {
            *out.add(k) = var;
            k += 1;
            continue;
        }
        if nl == RECORD.len() - 1 && bytes_eq(var, RECORD.as_ptr(), nl) {
            continue;
        }
        match find(record, var, nl) {
            Some(entry) if entry.restores(var.add(nl + 1)) => {
                if let Some((value, vl)) = entry.original {
                    *out.add(k) = put(&mut strings, var, nl, value, vl);
                    k += 1;
                }
            }
            _ => {
                *out.add(k) = var;
                k += 1;
            }
        }
    }

    let mut pos = 0usize;
    while let Some(entry) = parse_entry(record, &mut pos) {
        let Some((value, vl)) = entry.original else {
            continue;
        };
        if present(envp, count, entry.name, entry.name_len) || !entry.restores(null()) {
            continue;
        }
        *out.add(k) = put(&mut strings, entry.name, entry.name_len, value, vl);
        k += 1;
    }
    *out.add(k) = null();
    out
}

unsafe fn name_len(var: *const u8) -> usize {
    let mut n = 0;
    while *var.add(n) != 0 && *var.add(n) != b'=' {
        n += 1;
    }
    n
}

unsafe fn present(envp: *const *const u8, count: usize, name: *const u8, nl: usize) -> bool {
    (0..count).any(|i| {
        let var = *envp.add(i);
        name_len(var) == nl && *var.add(nl) == b'=' && bytes_eq(var, name, nl)
    })
}

/// Write `name=value` and its terminator at `*strings`, advancing it.
unsafe fn put(
    strings: &mut *mut u8,
    name: *const u8,
    nl: usize,
    value: *const u8,
    vl: usize,
) -> *const u8 {
    let start = *strings;
    copy(start, name, nl);
    *start.add(nl) = b'=';
    copy(start.add(nl + 1), value, vl);
    *start.add(nl + 1 + vl) = 0;
    *strings = start.add(nl + vl + 2);
    start
}

// ---- the record --------------------------------------------------------------

#[derive(Clone, Copy)]
enum Applied {
    Removed,
    Value(u64),
    Any,
}

struct Entry {
    name: *const u8,
    name_len: usize,
    applied: Applied,
    original: Option<(*const u8, usize)>,
}

impl Entry {
    /// Whether a variable now holding `current` (null when unset) still
    /// holds what the runtime applied.
    unsafe fn restores(&self, current: *const u8) -> bool {
        match self.applied {
            Applied::Any => true,
            Applied::Removed => current.is_null(),
            Applied::Value(hash) => !current.is_null() && fnv1a(current, slen(current)) == hash,
        }
    }
}

unsafe fn find(record: *const u8, name: *const u8, nl: usize) -> Option<Entry> {
    let mut pos = 0usize;
    while let Some(entry) = parse_entry(record, &mut pos) {
        if entry.name_len == nl && bytes_eq(entry.name, name, nl) {
            return Some(entry);
        }
    }
    None
}

/// The entry at `record[*pos..]`, advancing past it. `None` at the end
/// and at anything malformed, which ends the record there.
unsafe fn parse_entry(record: *const u8, pos: &mut usize) -> Option<Entry> {
    let start = *pos;
    let mut p = start;
    while *record.add(p) != b'=' {
        if *record.add(p) == 0 {
            return None;
        }
        p += 1;
    }
    if p == start {
        return None;
    }
    let name_len = p - start;
    p += 1;
    let applied = match *record.add(p) {
        b'-' => {
            p += 1;
            Applied::Removed
        }
        b'*' => {
            p += 1;
            Applied::Any
        }
        _ => {
            let mut hash = 0u64;
            for _ in 0..16 {
                hash = (hash << 4) | hex_digit(*record.add(p))?;
                p += 1;
            }
            Applied::Value(hash)
        }
    };
    let original = if *record.add(p) == b'-' {
        p += 1;
        None
    } else {
        let mut len = 0usize;
        let digits = p;
        while (*record.add(p)).is_ascii_digit() {
            len = len
                .checked_mul(10)?
                .checked_add((*record.add(p) - b'0') as usize)?;
            p += 1;
        }
        if p == digits || *record.add(p) != b':' {
            return None;
        }
        p += 1;
        let value = record.add(p);
        let mut i = 0;
        while i < len {
            if *value.add(i) == 0 {
                return None;
            }
            i += 1;
        }
        p += len;
        Some((value, len))
    };
    if *record.add(p) != b';' {
        return None;
    }
    *pos = p + 1;
    Some(Entry {
        name: record.add(start),
        name_len,
        applied,
        original,
    })
}

fn hex_digit(c: u8) -> Option<u64> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u64),
        b'a'..=b'f' => Some((c - b'a' + 10) as u64),
        _ => None,
    }
}

unsafe fn fnv1a(bytes: *const u8, n: usize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut i = 0;
    while i < n {
        hash ^= *bytes.add(i) as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

// ---- finding the real functions ---------------------------------------------

/// Look the real functions up in the objects loaded after this one, the
/// search `dlsym(RTLD_NEXT, ...)` makes. `dlsym` itself is not used:
/// before glibc 2.34 it lives in `libdl.so.2`, which a process need not
/// have, and the hooks have to reach the real functions regardless.
pub(crate) unsafe fn resolve() {
    let mut walk = Walk {
        past_self: false,
        found: [0; 4],
    };
    dl_iterate_phdr(visit, &raw mut walk as *mut c_void);
    for (slot, addr) in REAL.iter().zip(walk.found) {
        if addr != 0 {
            slot.store(addr, Ordering::Relaxed);
        }
    }
}

#[repr(C)]
struct DlPhdrInfo {
    addr: usize,
    name: *const u8,
    phdr: *const Phdr,
    phnum: u16,
}

#[cfg(target_pointer_width = "64")]
#[repr(C)]
#[allow(dead_code)]
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

#[cfg(target_pointer_width = "32")]
#[repr(C)]
#[allow(dead_code)]
struct Phdr {
    p_type: u32,
    p_offset: u32,
    p_vaddr: u32,
    p_paddr: u32,
    p_filesz: u32,
    p_memsz: u32,
    p_flags: u32,
    p_align: u32,
}

#[cfg(target_pointer_width = "64")]
#[repr(C)]
#[allow(dead_code)]
struct Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}

#[cfg(target_pointer_width = "32")]
#[repr(C)]
#[allow(dead_code)]
struct Sym {
    st_name: u32,
    st_value: u32,
    st_size: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
}

#[repr(C)]
struct Dyn {
    tag: isize,
    val: usize,
}

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_HASH: isize = 4;
const DT_STRTAB: isize = 5;
const DT_SYMTAB: isize = 6;
const DT_GNU_HASH: isize = 0x6fff_fef5;
const DT_VERSYM: isize = 0x6fff_fff0;
const STT_FUNC: u8 = 2;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;

struct Walk {
    past_self: bool,
    found: [usize; 4],
}

unsafe extern "C" fn visit(info: *mut DlPhdrInfo, _size: usize, data: *mut c_void) -> i32 {
    let walk = &mut *(data as *mut Walk);
    let info = &*info;
    if !walk.past_self {
        walk.past_self = maps(info, resolve as *const () as usize);
        return 0;
    }
    let mut done = true;
    for (addr, name) in walk.found.iter_mut().zip(NAMES) {
        if *addr == 0 {
            *addr = lookup(info, name);
        }
        done &= *addr != 0;
    }
    done as i32
}

/// Whether one of the object's loaded segments holds `addr`.
unsafe fn maps(info: &DlPhdrInfo, addr: usize) -> bool {
    (0..info.phnum as usize).any(|i| {
        let ph = &*info.phdr.add(i);
        let start = info.addr.wrapping_add(ph.p_vaddr as usize);
        ph.p_type == PT_LOAD && addr >= start && addr - start < ph.p_memsz as usize
    })
}

/// The address of the default version of function `name` in the
/// object, or 0.
unsafe fn lookup(info: &DlPhdrInfo, name: &[u8]) -> usize {
    let base = info.addr;
    let Some(ph) = (0..info.phnum as usize)
        .map(|i| &*info.phdr.add(i))
        .find(|ph| ph.p_type == PT_DYNAMIC)
    else {
        return 0;
    };
    // glibc relocates these entries in place and musl does not. An
    // unrelocated one is an offset into the object, below its base.
    let at = |v: usize| if v != 0 && v < base { v + base } else { v };
    let mut table = Table {
        base,
        strtab: 0,
        symtab: 0,
        versym: 0,
    };
    let (mut hash, mut gnu) = (0usize, 0usize);
    let mut d = base.wrapping_add(ph.p_vaddr as usize) as *const Dyn;
    while (*d).tag != 0 {
        let v = at((*d).val);
        match (*d).tag {
            DT_HASH => hash = v,
            DT_STRTAB => table.strtab = v,
            DT_SYMTAB => table.symtab = v,
            DT_GNU_HASH => gnu = v,
            DT_VERSYM => table.versym = v,
            _ => {}
        }
        d = d.add(1);
    }
    if table.strtab == 0 || table.symtab == 0 {
        return 0;
    }
    if gnu != 0 {
        table.gnu_lookup(gnu, name)
    } else if hash != 0 {
        table.sysv_lookup(hash, name)
    } else {
        0
    }
}

struct Table {
    base: usize,
    strtab: usize,
    symtab: usize,
    versym: usize,
}

impl Table {
    unsafe fn gnu_lookup(&self, gnu: usize, name: &[u8]) -> usize {
        let mut h = 5381u32;
        for &c in &name[..name.len() - 1] {
            h = h.wrapping_mul(33).wrapping_add(c as u32);
        }
        let words = gnu as *const u32;
        let (nbuckets, symoffset, bloom_size) = (*words, *words.add(1), *words.add(2));
        if nbuckets == 0 {
            return 0;
        }
        let buckets = (gnu + 16 + bloom_size as usize * size_of::<usize>()) as *const u32;
        let chain = buckets.add(nbuckets as usize);
        let mut i = *buckets.add((h % nbuckets) as usize);
        if i < symoffset {
            return 0;
        }
        loop {
            let link = *chain.add((i - symoffset) as usize);
            if link | 1 == h | 1 {
                let addr = self.matches(i as usize, name);
                if addr != 0 {
                    return addr;
                }
            }
            if link & 1 != 0 {
                return 0;
            }
            i += 1;
        }
    }

    unsafe fn sysv_lookup(&self, hash: usize, name: &[u8]) -> usize {
        let mut h = 0u32;
        for &c in &name[..name.len() - 1] {
            h = (h << 4).wrapping_add(c as u32);
            let g = h & 0xf000_0000;
            if g != 0 {
                h ^= g >> 24;
            }
            h &= !g;
        }
        let words = hash as *const u32;
        let nbucket = *words;
        if nbucket == 0 {
            return 0;
        }
        let bucket = words.add(2);
        let chain = bucket.add(nbucket as usize);
        let mut i = *bucket.add((h % nbucket) as usize);
        while i != 0 {
            let addr = self.matches(i as usize, name);
            if addr != 0 {
                return addr;
            }
            i = *chain.add(i as usize);
        }
        0
    }

    /// The address symbol `i` defines when it is the default version of
    /// global function `name`, or 0.
    unsafe fn matches(&self, i: usize, name: &[u8]) -> usize {
        let sym = &*(self.symtab as *const Sym).add(i);
        let (bind, kind) = (sym.st_info >> 4, sym.st_info & 0xf);
        if sym.st_shndx == 0
            || sym.st_value == 0
            || kind != STT_FUNC
            || (bind != STB_GLOBAL && bind != STB_WEAK)
        {
            return 0;
        }
        if self.versym != 0 {
            // Index 0 is local; the top bit marks a version that is not
            // the default, such as glibc's older `posix_spawn`.
            let version = *(self.versym as *const u16).add(i);
            if version == 0 || version & 0x8000 != 0 {
                return 0;
            }
        }
        let sym_name = (self.strtab + sym.st_name as usize) as *const u8;
        if !bytes_eq(sym_name, name.as_ptr(), name.len()) {
            return 0;
        }
        self.base.wrapping_add(sym.st_value as usize)
    }
}

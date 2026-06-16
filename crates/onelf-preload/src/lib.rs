//! LD_PRELOAD interposer for onelf cross-libc portability.
//!
//! Loaded into every bundled process (dlopen'd from `.onelf/preload` by the
//! onelf-env constructor, or injected via the bundled `ld.so` `--preload` /
//! `LD_PRELOAD` by the launcher). It interposes the exec family so that:
//!
//!   - **bundled ELF targets** are launched through the bundled interpreter
//!     with an env-independent library path. The package root is found by
//!     self-locating this object in `/proc/self/maps`, so it works even after
//!     the app calls `clearenv()` and re-execs itself in a sandbox. The
//!     interposer re-injects itself (`LD_PRELOAD`) into the child so the chain
//!     continues across every exec.
//!   - **host binaries** are launched with the bundle's `LD_LIBRARY_PATH` /
//!     `LD_PRELOAD` / `ONELF_*` stripped from the environment, so the bundled
//!     libc never leaks into `/bin/sh`, `ssh`, and similar.
//!
//! Self-location and the env-independent library path are what let onelf drop
//! the baked `$ORIGIN` rpath entirely: nothing relies on the inherited
//! environment surviving, and nothing relies on a search path baked into the
//! ELF.

#![allow(unsafe_op_in_unsafe_fn)]

use libc::{c_char, c_int, pid_t, posix_spawn_file_actions_t, posix_spawnattr_t};
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::addr_of_mut;
use std::sync::OnceLock;

type ExecveFn = unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int;
type PosixSpawnFn = unsafe extern "C" fn(
    *mut pid_t,
    *const c_char,
    *const posix_spawn_file_actions_t,
    *const posix_spawnattr_t,
    *const *const c_char,
    *const *const c_char,
) -> c_int;

static mut REAL_EXECVE: Option<ExecveFn> = None;
static mut REAL_POSIX_SPAWN: Option<PosixSpawnFn> = None;
static mut REAL_POSIX_SPAWNP: Option<PosixSpawnFn> = None;

unsafe fn real_execve() -> ExecveFn {
    if (*addr_of_mut!(REAL_EXECVE)).is_none() {
        let name = CString::new("execve").unwrap();
        let ptr = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        *addr_of_mut!(REAL_EXECVE) = Some(std::mem::transmute::<*mut libc::c_void, ExecveFn>(ptr));
    }
    (*addr_of_mut!(REAL_EXECVE)).unwrap()
}

unsafe fn real_posix_spawn(slot: &mut Option<PosixSpawnFn>, sym: &str) -> PosixSpawnFn {
    if slot.is_none() {
        let name = CString::new(sym).unwrap();
        let ptr = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        *slot = Some(std::mem::transmute::<*mut libc::c_void, PosixSpawnFn>(ptr));
    }
    slot.unwrap()
}

// ---- self-location (env-independent) -------------------------------------

/// `(package_root, this_object_path)`, located once via `/proc/self/maps`.
fn location() -> Option<&'static (PathBuf, PathBuf)> {
    static LOC: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    LOC.get_or_init(locate).as_ref()
}

fn locate() -> Option<(PathBuf, PathBuf)> {
    // An address known to fall inside this object's mapping.
    let probe = locate as *const () as usize;
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;

    let mut so: Option<PathBuf> = None;
    for line in maps.lines() {
        // "start-end perms offset dev inode   /path"
        let (range, _) = line.split_once(' ')?;
        let (s, e) = range.split_once('-')?;
        let start = usize::from_str_radix(s, 16).ok()?;
        let end = usize::from_str_radix(e, 16).ok()?;
        if probe >= start && probe < end {
            if let Some(idx) = line.find('/') {
                so = Some(PathBuf::from(line[idx..].trim_end()));
            }
            break;
        }
    }

    // Walk up from the object's directory to the one containing `.onelf/`.
    let so = so?;
    let mut dir = so.parent();
    for _ in 0..16 {
        let d = dir?;
        if d.join(".onelf").is_dir() {
            return Some((d.to_path_buf(), so.clone()));
        }
        dir = d.parent();
    }
    None
}

/// Absolute, colon-joined library search path derived from `.onelf/libpath`
/// (one package-relative directory per line). Empty if the file is absent.
fn lib_path(pkg: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Ok(s) = std::fs::read_to_string(pkg.join(".onelf/libpath")) {
        for line in s.lines() {
            let t = line.trim();
            if !t.is_empty() {
                parts.push(pkg.join(t).to_string_lossy().into_owned());
            }
        }
    }
    parts.join(":")
}

/// Absolute path to the bundled interpreter from `.onelf/interp`.
fn bundled_interp(pkg: &Path) -> Option<PathBuf> {
    let rel = std::fs::read_to_string(pkg.join(".onelf/interp")).ok()?;
    let p = pkg.join(rel.trim());
    p.is_file().then_some(p)
}

// ---- ELF classification --------------------------------------------------

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let got = f.read(&mut buf).ok()?;
    buf.truncate(got);
    Some(buf)
}

fn is_elf(path: &Path) -> bool {
    read_head(path, 4).map(|h| h.len() >= 4 && &h[0..4] == b"\x7fELF").unwrap_or(false)
}

/// True for ET_DYN (PIE / shared object); these can be launched via the
/// bundled interpreter. ET_EXEC must be kernel-loaded instead.
fn is_pie(path: &Path) -> bool {
    read_head(path, 18)
        .map(|h| h.len() >= 18 && &h[0..4] == b"\x7fELF" && u16::from_le_bytes([h[16], h[17]]) == 3)
        .unwrap_or(false)
}

/// Self-extracting binaries (bun and similar) read `/proc/self/exe` to find
/// an appended payload, so they must be kernel-loaded, not run through the
/// interpreter as a program.
fn has_self_extract_trailer(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    const TRAILER: &[u8] = b"\n---- Bun! ----\n";
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    if meta.len() < 24 {
        return false;
    }
    let mut buf = [0u8; 24];
    if f.seek(SeekFrom::End(-24)).is_err() || f.read_exact(&mut buf).is_err() {
        return false;
    }
    &buf[8..24] == TRAILER || &buf[0..16] == TRAILER
}

fn is_musl_interp(interp: &Path) -> bool {
    interp
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("ld-musl-"))
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

/// True if `target` resolves to a path inside the package root.
fn is_bundled(target: &Path, pkg: &Path) -> bool {
    let t = std::fs::canonicalize(target).unwrap_or_else(|_| absolutize(target));
    let p = std::fs::canonicalize(pkg).unwrap_or_else(|_| pkg.to_path_buf());
    t.starts_with(&p)
}

// ---- environment array helpers -------------------------------------------

unsafe fn envp_to_vec(envp: *const *const c_char) -> Vec<CString> {
    let mut out = Vec::new();
    if envp.is_null() {
        return out;
    }
    let mut p = envp;
    while !(*p).is_null() {
        out.push(CStr::from_ptr(*p).to_owned());
        p = p.add(1);
    }
    out
}

unsafe fn argv_to_vec(argv: *const *const c_char) -> Vec<CString> {
    let mut out = Vec::new();
    if argv.is_null() {
        return out;
    }
    let mut p = argv;
    while !(*p).is_null() {
        out.push(CStr::from_ptr(*p).to_owned());
        p = p.add(1);
    }
    out
}

fn env_key(e: &CString) -> &[u8] {
    let b = e.as_bytes();
    match b.iter().position(|&c| c == b'=') {
        Some(i) => &b[..i],
        None => b,
    }
}

/// Drop the bundle's library/preload/onelf variables so host binaries run
/// against the host environment.
fn strip_bundle_env(env: Vec<CString>) -> Vec<CString> {
    env.into_iter()
        .filter(|e| {
            let k = env_key(e);
            k != b"LD_LIBRARY_PATH" && k != b"LD_PRELOAD" && !k.starts_with(b"ONELF_")
        })
        .collect()
}

/// Override `LD_LIBRARY_PATH` and `LD_PRELOAD` with the bundle's values so the
/// child resolves bundled libs and re-loads this interposer, regardless of the
/// inherited environment.
fn build_bundle_env(env: Vec<CString>, libpath: &str, self_so: &Path) -> Vec<CString> {
    let mut out: Vec<CString> = env
        .into_iter()
        .filter(|e| {
            let k = env_key(e);
            // Drop LD_PRELOAD always (we re-add ours). Drop LD_LIBRARY_PATH only
            // when we have a derived path to replace it with; otherwise keep the
            // inherited one as a fallback (relevant before `.onelf/libpath`
            // exists or on arches without one).
            k != b"LD_PRELOAD" && !(k == b"LD_LIBRARY_PATH" && !libpath.is_empty())
        })
        .collect();
    if !libpath.is_empty() {
        out.push(CString::new(format!("LD_LIBRARY_PATH={libpath}")).unwrap());
    }
    let mut preload = b"LD_PRELOAD=".to_vec();
    preload.extend_from_slice(self_so.as_os_str().as_bytes());
    out.push(CString::new(preload).unwrap());
    out
}

fn to_c_envp(env: &[CString]) -> Vec<*const c_char> {
    let mut v: Vec<*const c_char> = env.iter().map(|e| e.as_ptr()).collect();
    v.push(std::ptr::null());
    v
}

// ---- PATH resolution for the `*p` exec variants --------------------------

fn resolve_in_path(file: &CStr, env: &[CString]) -> Option<PathBuf> {
    let bytes = file.to_bytes();
    if bytes.contains(&b'/') {
        return Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)));
    }
    let path_val = env.iter().find_map(|e| {
        let b = e.as_bytes();
        b.strip_prefix(b"PATH=").map(|v| v.to_vec())
    });
    let path_val = path_val.or_else(|| std::env::var_os("PATH").map(|p| p.as_bytes().to_vec()))?;
    for dir in path_val.split(|&c| c == b':') {
        let dir = if dir.is_empty() { b"." as &[u8] } else { dir };
        let mut cand = PathBuf::from(std::ffi::OsStr::from_bytes(dir));
        cand.push(std::ffi::OsStr::from_bytes(bytes));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

// ---- the actual routing --------------------------------------------------

/// Replace the current process image, choosing bundled-vs-host routing. Only
/// returns on failure (mirrors `execve` semantics).
unsafe fn route_execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let real = real_execve();
    if path.is_null() || argv.is_null() {
        return real(path, argv, envp);
    }

    let Some((pkg, self_so)) = location() else {
        return real(path, argv, envp);
    };

    let target = PathBuf::from(std::ffi::OsStr::from_bytes(CStr::from_ptr(path).to_bytes()));
    if !is_elf(&target) {
        // Scripts and the like: leave the environment untouched.
        return real(path, argv, envp);
    }

    let env = envp_to_vec(envp);

    if !is_bundled(&target, pkg) {
        let host_env = strip_bundle_env(env);
        let cenv = to_c_envp(&host_env);
        return real(path, argv, cenv.as_ptr());
    }

    let libpath = lib_path(pkg);
    let bundle_env = build_bundle_env(env, &libpath, self_so);

    // PIE, non-self-extract: run through the bundled interpreter while keeping
    // /proc/self/exe pointed at the target (userland-execve + override).
    if is_pie(&target) && !has_self_extract_trailer(&target) && ulexec_supported() {
        if let Some(interp) = bundled_interp(pkg) {
            let argvv = argv_to_vec(argv);
            ulexec_override(&target, &interp, &argvv, &bundle_env);
            // ulexec_override only returns on failure; fall through.
        }
    }

    // Non-PIE / self-extract / no interpreter: kernel-load with bundle env.
    let cenv = to_c_envp(&bundle_env);
    real(path, argv, cenv.as_ptr())
}

/// userland-execve invoking `interp` but presenting `target` as the program,
/// so `/proc/self/exe` resolves to the target. Library resolution comes from
/// the (bundle) environment we pass, not from the inherited one.
unsafe fn ulexec_override(target: &Path, interp: &Path, argv: &[CString], env: &[CString]) {
    let mut options = userland_execve::ExecOptions::new(interp);
    for a in argv {
        options.arg(a);
    }
    for e in env {
        let b = e.as_bytes();
        if let Some(eq) = b.iter().position(|&c| c == b'=') {
            let k = CString::new(&b[..eq]).unwrap();
            let v = CString::new(&b[eq + 1..]).unwrap();
            options.env(&k, &v);
        }
    }
    options.override_interpreter(Some(target));
    userland_execve::exec_with_options(options)
}

/// Build the `ld.so [--inhibit-cache] --library-path L --argv0 A target args`
/// argv used to launch a bundled PIE through posix_spawn (which cannot replace
/// the current image, so the interpreter is run as the program).
fn spawn_argv(interp: &Path, libpath: &str, target: &Path, argv: &[CString]) -> Vec<CString> {
    let mut out: Vec<CString> = Vec::new();
    out.push(CString::new(interp.as_os_str().as_bytes()).unwrap());
    if !is_musl_interp(interp) {
        out.push(CString::new("--inhibit-cache").unwrap());
    }
    if !libpath.is_empty() {
        out.push(CString::new("--library-path").unwrap());
        out.push(CString::new(libpath).unwrap());
    }
    out.push(CString::new("--argv0").unwrap());
    out.push(argv.first().cloned().unwrap_or_else(|| CString::new("").unwrap()));
    out.push(CString::new(target.as_os_str().as_bytes()).unwrap());
    for a in argv.iter().skip(1) {
        out.push(a.clone());
    }
    out
}

unsafe fn route_posix_spawn(
    is_p: bool,
    pid: *mut pid_t,
    path: *const c_char,
    fa: *const posix_spawn_file_actions_t,
    attr: *const posix_spawnattr_t,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let real = if is_p {
        real_posix_spawn(&mut *addr_of_mut!(REAL_POSIX_SPAWNP), "posix_spawnp")
    } else {
        real_posix_spawn(&mut *addr_of_mut!(REAL_POSIX_SPAWN), "posix_spawn")
    };
    if path.is_null() || argv.is_null() {
        return real(pid, path, fa, attr, argv, envp);
    }

    let Some((pkg, self_so)) = location() else {
        return real(pid, path, fa, attr, argv, envp);
    };

    let env = envp_to_vec(envp);
    let file = CStr::from_ptr(path);
    let resolved = if is_p {
        resolve_in_path(file, &env)
    } else {
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(file.to_bytes())))
    };
    let Some(target) = resolved else {
        return real(pid, path, fa, attr, argv, envp);
    };

    if !is_elf(&target) {
        return real(pid, path, fa, attr, argv, envp);
    }

    if !is_bundled(&target, pkg) {
        let host_env = strip_bundle_env(env);
        let cenv = to_c_envp(&host_env);
        return real(pid, path, fa, attr, argv, cenv.as_ptr());
    }

    let libpath = lib_path(pkg);
    let bundle_env = build_bundle_env(env, &libpath, self_so);
    let cenv = to_c_envp(&bundle_env);

    // Bundled PIE: spawn the bundled interpreter as the program.
    if is_pie(&target) && !has_self_extract_trailer(&target) {
        if let Some(interp) = bundled_interp(pkg) {
            let argvv = argv_to_vec(argv);
            let new_argv = spawn_argv(&interp, &libpath, &target, &argvv);
            let cargv = to_c_envp(&new_argv);
            let interp_c = CString::new(interp.as_os_str().as_bytes()).unwrap();
            return real(pid, interp_c.as_ptr(), fa, attr, cargv.as_ptr(), cenv.as_ptr());
        }
    }

    // Bundled non-PIE / self-extract: spawn as-is with bundle env.
    real(pid, path, fa, attr, argv, cenv.as_ptr())
}

// ---- interposed symbols --------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    route_execve(path, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    route_execve(path, argv, environ())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    execvpe(file, argv, environ())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execvpe(
    file: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if file.is_null() {
        return route_execve(file, argv, envp);
    }
    let env = envp_to_vec(envp);
    match resolve_in_path(CStr::from_ptr(file), &env) {
        Some(p) => {
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            route_execve(c.as_ptr(), argv, envp)
        }
        None => {
            *libc::__errno_location() = libc::ENOENT;
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawn(
    pid: *mut pid_t,
    path: *const c_char,
    fa: *const posix_spawn_file_actions_t,
    attr: *const posix_spawnattr_t,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    route_posix_spawn(false, pid, path, fa, attr, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_spawnp(
    pid: *mut pid_t,
    file: *const c_char,
    fa: *const posix_spawn_file_actions_t,
    attr: *const posix_spawnattr_t,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    route_posix_spawn(true, pid, file, fa, attr, argv, envp)
}

unsafe fn environ() -> *const *const c_char {
    unsafe extern "C" {
        static environ: *const *const c_char;
    }
    environ
}

/// userland-execve only supports replacing the image on these targets.
const fn ulexec_supported() -> bool {
    cfg!(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))
}

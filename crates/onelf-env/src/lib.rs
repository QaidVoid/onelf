//! onelf payload-side environment constructor.
//!
//! This `cdylib` is bundled into a package's `lib/` and injected as a
//! `DT_NEEDED` of the entrypoint binary. Because `DT_NEEDED` lives in the
//! ELF (not the environment) and the entrypoint carries a baked-in
//! `$ORIGIN` RUNPATH (see `bundle::set_origin_runpath`), this library is
//! loaded on *every* exec of the entrypoint, including after an
//! application re-execs itself in a sandbox that does `clearenv()`.
//!
//! Its `.init_array` constructor runs before `main`, locates the package
//! root relative to its own on-disk path (never via the environment),
//! and re-applies:
//!
//! * `.onelf/env`     — `KEY=VALUE` lines, `${ONELF_DIR}` expanded to the
//!                       package root, set with overwrite.
//! * `.onelf/preload` — one library path per line, `dlopen`'d with
//!                       `RTLD_NOW | RTLD_GLOBAL`.
//!
//! This gives the same guarantee as sharun's `.env` / `.preload`: the
//! values are restored no matter how aggressively the application clears
//! the environment, because the mechanism is on the exec path itself
//! rather than inherited through `envp`.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Register `onelf_env_ctor` in `.init_array` so the dynamic loader runs
/// it before `main` every time this object is loaded.
#[used]
#[unsafe(link_section = ".init_array")]
static ONELF_ENV_INIT: extern "C" fn() = onelf_env_ctor;

extern "C" fn onelf_env_ctor() {
    // A constructor must never unwind across the loader's call frame.
    // Swallow any panic and continue process startup unchanged.
    let _ = std::panic::catch_unwind(run);
}

fn run() {
    let Some(so_path) = self_so_path() else {
        return;
    };
    let Some(root) = find_package_root(&so_path) else {
        return;
    };
    let root_str = root.to_string_lossy().into_owned();

    apply_env(&root.join(".onelf/env"), &root_str);
    apply_preload(&root.join(".onelf/preload"), &root_str);
}

/// Resolve this shared object's own path via `dladdr` on a local symbol.
/// Independent of `argv0`, CWD, and the environment, so it stays correct
/// across a sandboxed re-exec.
fn self_so_path() -> Option<PathBuf> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let addr = onelf_env_ctor as *const libc::c_void;
    let ok = unsafe { libc::dladdr(addr, &mut info) };
    if ok == 0 || info.dli_fname.is_null() {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(info.dli_fname) };
    let path = Path::new(std::ffi::OsStr::from_bytes(cstr.to_bytes()));
    // `dli_fname` may be relative (the name the loader resolved); make it
    // absolute so the parent walk is reliable.
    std::fs::canonicalize(path).ok()
}

/// Walk up from the library's directory looking for an enclosing package
/// root, identified by a child `.onelf/` directory. Bounded so a stray
/// invocation can't traverse the whole filesystem.
fn find_package_root(so_path: &Path) -> Option<PathBuf> {
    let mut dir = so_path.parent();
    for _ in 0..10 {
        let d = dir?;
        if d.join(".onelf").is_dir() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Apply `.onelf/env`: `KEY=VALUE` per line, `#` comments and blank lines
/// skipped, `${ONELF_DIR}` expanded to the package root. Set with
/// overwrite so the values win even if the app cleared them.
fn apply_env(env_file: &Path, root: &str) {
    let Ok(text) = std::fs::read_to_string(env_file) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let val = val.trim().replace("${ONELF_DIR}", root);
        if let (Ok(k), Ok(v)) = (CString::new(key), CString::new(val)) {
            unsafe {
                libc::setenv(k.as_ptr(), v.as_ptr(), 1);
            }
        }
    }
}

/// Apply `.onelf/preload`: one library path per line (same comment and
/// `${ONELF_DIR}` rules), each `dlopen`'d with `RTLD_NOW | RTLD_GLOBAL`
/// so its symbols and constructors are available process-wide.
fn apply_preload(preload_file: &Path, root: &str) {
    let Ok(text) = std::fs::read_to_string(preload_file) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = line.replace("${ONELF_DIR}", root);
        if let Ok(c) = CString::new(path) {
            unsafe {
                libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_parsing_expands_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("onelf-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("env");
        std::fs::write(
            &f,
            "# comment\n\
             \n\
             ONELF_T_PLAIN=plain\n\
             ONELF_T_DIR=${ONELF_DIR}/share\n\
             =skipme\n\
             ONELF_T_TRIM =  spaced  \n",
        )
        .unwrap();

        // Pre-set one var to prove overwrite semantics.
        unsafe { std::env::set_var("ONELF_T_PLAIN", "stale") };

        apply_env(&f, "/pkg/root");

        assert_eq!(std::env::var("ONELF_T_PLAIN").unwrap(), "plain");
        assert_eq!(std::env::var("ONELF_T_DIR").unwrap(), "/pkg/root/share");
        assert_eq!(std::env::var("ONELF_T_TRIM").unwrap(), "spaced");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_are_silent() {
        // Non-existent paths must not panic or set anything.
        apply_env(Path::new("/nonexistent/onelf/env"), "/pkg");
        apply_preload(Path::new("/nonexistent/onelf/preload"), "/pkg");
    }
}

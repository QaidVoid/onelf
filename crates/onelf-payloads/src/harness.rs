//! A process-wide lock and builders for the tests that touch the
//! environment.
//!
//! The environment, and the constructor's static package root, are process
//! globals the test runner drives in parallel: every test that sets,
//! clears, or asserts a variable holds this lock while it does.

use std::os::unix::ffi::OsStrExt;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Hold the environment still for one test.
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The bytes of `path` NUL-terminated, in the shape the constructor's libc
/// surface speaks.
pub(crate) fn c_path(path: &std::path::Path) -> Vec<u8> {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// Point the constructor's package root at `path`.
pub(crate) unsafe fn set_root(path: &std::path::Path) {
    let src = c_path(path);
    let dst = core::ptr::addr_of_mut!(super::G_ROOT) as *mut u8;
    core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
}

/// A package view directory with `.onelf/` in it, cleared of any earlier
/// run of the same test.
pub(crate) fn view(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("onelf-env-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".onelf")).unwrap();
    root
}

/// One entry of the evaluated per-view input, the way the launch writes it.
pub(crate) fn runtime_entry(
    name: &str,
    policy: crate::rtenv::Policy,
    value: &[u8],
    left_set: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(name.as_bytes());
    out.push(b'=');
    out.push(match policy {
        crate::rtenv::Policy::Preserve => b'.',
        crate::rtenv::Policy::Replace => b'!',
    });
    out.extend_from_slice(value.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(value);
    out.push(if left_set { b'+' } else { b'-' });
    out.extend_from_slice(b";\n");
    out
}

/// A whole evaluated per-view input record with the given entries and the
/// given scalar records.
pub(crate) fn runtime_record(
    entries: &[Vec<u8>],
    roots: Option<&[u8]>,
    host_record: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"ONELF-RUNTIME-ENV 1 ");
    out.extend_from_slice(entries.len().to_string().as_bytes());
    out.push(b'\n');
    for entry in entries {
        out.extend_from_slice(entry);
    }
    for (tag, scalar) in [(b'R', roots), (b'H', host_record)] {
        if let Some(bytes) = scalar {
            out.push(tag);
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
            out.extend_from_slice(b";\n");
        }
    }
    out
}

/// Set a variable the way the test environment does it.
pub(crate) unsafe fn set_env(name: &str, value: &str) {
    // SAFETY: the caller holds the environment lock
    unsafe { std::env::set_var(name, value) };
}

/// Drop a variable the way the test environment does it.
pub(crate) unsafe fn unset_env(name: &str) {
    // SAFETY: the caller holds the environment lock
    unsafe { std::env::remove_var(name) };
}

/// The live value of a variable, when it is set.
pub(crate) fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

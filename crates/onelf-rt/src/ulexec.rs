//! Userland execve support for cross-libc portability.
//!
//! Uses userland-execve to invoke the bundled dynamic linker directly,
//! passing `--library-path` and `--argv0` as arguments. The linker then
//! loads the target binary itself. This pattern (matching sharun) avoids
//! polluting the child env with `LD_LIBRARY_PATH`, which would otherwise
//! leak into every host binary the app spawns.

use std::ffi::CString;
use std::path::Path;

/// Execute an ELF binary by invoking its bundled dynamic linker via
/// userland-execve. The linker receives `--library-path` and `--argv0`
/// as command-line flags so the bundled lib search is scoped to this
/// single exec and never inherited by child processes.
///
/// # Arguments
/// * `target` - Path to the ELF binary the linker will load
/// * `interpreter` - Path to the bundled ELF interpreter (ld-linux.so / ld-musl-*)
/// * `lib_path` - Colon-separated library search path for `--library-path`
/// * `argv0` - Value for argv[0] (how the program sees its name) via `--argv0`
/// * `args` - Additional command-line arguments (argv[1..])
///
/// This function never returns on success (it replaces the current process).
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

    let mut options = userland_execve::ExecOptions::new(interpreter);

    // argv[0] is the linker's own path; the linker treats it that way.
    let interp_argv0 = CString::new(interp_str.as_ref()).unwrap();
    options.arg(&interp_argv0);

    // glibc accepts --inhibit-cache to skip /etc/ld.so.cache (which points
    // at host libs). musl doesn't have a cache and errors on unknown flags.
    if !is_musl {
        let f = CString::new("--inhibit-cache").unwrap();
        options.arg(&f);
    }

    if !lib_path.is_empty() {
        let f = CString::new("--library-path").unwrap();
        let v = CString::new(lib_path).unwrap();
        options.arg(&f);
        options.arg(&v);
    }

    let argv0_flag = CString::new("--argv0").unwrap();
    let argv0_val = CString::new(argv0).unwrap();
    options.arg(&argv0_flag);
    options.arg(&argv0_val);

    let target_c = CString::new(target_str.as_ref()).unwrap();
    options.arg(&target_c);

    for arg in args {
        let arg_c = CString::new(arg.as_str()).unwrap();
        options.arg(&arg_c);
    }

    for (key, value) in std::env::vars() {
        let key_c = CString::new(key).unwrap();
        let value_c = CString::new(value).unwrap();
        options.env(&key_c, &value_c);
    }

    userland_execve::exec_with_options(options)
}

/// Check if userland-execve is supported on this platform.
/// Currently only supports Linux x86_64 and aarch64.
pub const fn is_supported() -> bool {
    cfg!(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
}

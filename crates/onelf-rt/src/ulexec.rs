//! Userland execve support for cross-libc portability.
//!
//! Uses userland-execve to execute ELF binaries with a bundled interpreter,
//! completely bypassing the kernel's ELF loader. This eliminates the need
//! for symlinks in /tmp since PT_INTERP is ignored.

use std::ffi::CString;
use std::path::Path;

/// Execute an ELF binary using userland-execve with a bundled interpreter.
///
/// This function maps the executable and interpreter into memory directly,
/// creates a proper stack with auxiliary vector, and jumps to the interpreter's
/// entry point. The kernel's ELF loader is bypassed entirely.
///
/// # Arguments
/// * `executable` - Path to the ELF binary to execute
/// * `interpreter` - Path to the bundled ELF interpreter (ld-linux.so)
/// * `argv0` - Value for argv[0] (how the program sees its name)
/// * `args` - Additional command-line arguments (argv[1..])
///
/// This function never returns on success (it replaces the current process).
pub fn exec_with_interp(executable: &Path, interpreter: &Path, argv0: &str, args: &[String]) -> ! {
    let mut options = userland_execve::ExecOptions::new(executable);

    let argv0_c = CString::new(argv0).unwrap();
    options.arg(&argv0_c);

    for arg in args {
        let arg_c = CString::new(arg.as_str()).unwrap();
        options.arg(&arg_c);
    }

    for (key, value) in std::env::vars() {
        let key_c = CString::new(key).unwrap();
        let value_c = CString::new(value).unwrap();
        options.env(&key_c, &value_c);
    }

    options.override_interpreter(Some(interpreter));

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

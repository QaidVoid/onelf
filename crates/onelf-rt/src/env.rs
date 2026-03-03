//! Environment variable setup for the running package.
//!
//! Sets `ONELF_*` variables and configures `LD_LIBRARY_PATH` for packages
//! that bundle shared libraries.

use std::env;
use std::path::Path;

pub fn setup_env(
    onelf_dir: &str,
    argv0: &str,
    exec_path: &str,
    entrypoint_name: &str,
    mode: &str,
    lib_subpath: &str,
) {
    let launch_dir = env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    // SAFETY: the runtime is single-threaded at this point (before exec)
    unsafe {
        env::set_var("ONELF_DIR", onelf_dir);
        env::set_var("ONELF_ARGV0", argv0);
        env::set_var("ONELF_EXEC", exec_path);
        env::set_var("ONELF_ENTRYPOINT", entrypoint_name);
        env::set_var("ONELF_LAUNCH_DIR", &launch_dir);
        env::set_var("ONELF_MODE", mode);
    }

    // Auto-set LD_LIBRARY_PATH if package has lib directories
    if !lib_subpath.is_empty() && !onelf_dir.is_empty() {
        let lib_paths: Vec<String> = lib_subpath
            .split(':')
            .map(|p| Path::new(onelf_dir).join(p).to_string_lossy().to_string())
            .collect();
        let lib_str = lib_paths.join(":");
        if !lib_str.is_empty() {
            let existing = env::var("LD_LIBRARY_PATH").unwrap_or_default();
            unsafe {
                if existing.is_empty() {
                    env::set_var("LD_LIBRARY_PATH", lib_str);
                } else {
                    env::set_var("LD_LIBRARY_PATH", format!("{lib_str}:{existing}"));
                }
            }
        }
    }
}

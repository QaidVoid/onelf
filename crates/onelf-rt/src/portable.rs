//! Portable directory support.
//!
//! Detects `{binary}.home`, `{binary}.config`, `{binary}.share`, `{binary}.cache`
//! directories next to the binary and redirects `HOME`, `XDG_CONFIG_HOME`,
//! `XDG_DATA_HOME`, `XDG_CACHE_HOME` to them. Original values are preserved
//! in `REAL_*` variables. Also loads `{binary}.env` if present.

use std::env;
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

struct PortableDir {
    path: PathBuf,
    env_var: &'static str,
    default_subpath: Option<&'static str>,
}

/// Check args for `--onelf-portable-*` flags. Creates directories and exits if matched.
/// Returns `true` if a flag was handled.
pub fn handle_portable_flags(args: &[String], exe_dir: &Path, exe_name: &str) -> bool {
    let dirs = [
        ("--onelf-portable-home", "home"),
        ("--onelf-portable-config", "config"),
        ("--onelf-portable-share", "share"),
        ("--onelf-portable-cache", "cache"),
    ];

    let create_all = args.iter().any(|a| a == "--onelf-portable");

    let mut handled = create_all;
    for (flag, suffix) in &dirs {
        if create_all || args.iter().any(|a| a == *flag) {
            let dir = exe_dir.join(format!("{exe_name}.{suffix}"));
            match fs::create_dir(&dir) {
                Ok(()) => println!("created: {}", dir.display()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    println!("already exists: {}", dir.display());
                }
                Err(e) => {
                    eprintln!("onelf-rt: failed to create {}: {e}", dir.display());
                }
            }
            handled = true;
        }
    }

    handled
}

/// Set up portable directories and load env file.
pub fn setup_portable(exe_dir: &Path, exe_name: &str) {
    let dirs = [
        PortableDir {
            path: exe_dir.join(format!("{exe_name}.home")),
            env_var: "HOME",
            default_subpath: None,
        },
        PortableDir {
            path: exe_dir.join(format!("{exe_name}.share")),
            env_var: "XDG_DATA_HOME",
            default_subpath: Some(".local/share"),
        },
        PortableDir {
            path: exe_dir.join(format!("{exe_name}.config")),
            env_var: "XDG_CONFIG_HOME",
            default_subpath: Some(".config"),
        },
        PortableDir {
            path: exe_dir.join(format!("{exe_name}.cache")),
            env_var: "XDG_CACHE_HOME",
            default_subpath: Some(".cache"),
        },
    ];

    for dir in &dirs {
        if dir.path.is_dir() {
            set_portable_dir(&dir.path, dir.env_var, dir.default_subpath);
        }
    }

    // Load {binary_name}.env if present
    let env_file = exe_dir.join(format!("{exe_name}.env"));
    if env_file.is_file() {
        load_env_file(&env_file);
    }
}

fn set_portable_dir(dir: &Path, env_var: &str, default_subpath: Option<&str>) {
    let real_var = format!("REAL_{env_var}");

    // Preserve original value in REAL_* if not already set
    if env::var(&real_var).is_err() {
        if let Ok(current) = env::var(env_var) {
            // SAFETY: single-threaded at this point (before exec)
            unsafe { env::set_var(&real_var, current) };
        } else if let Some(default) = default_subpath {
            if let Ok(home) = env::var("HOME") {
                let default_dir = PathBuf::from(home).join(default);
                unsafe { env::set_var(&real_var, default_dir) };
            }
        }
    }

    unsafe { env::set_var(env_var, dir) };
}

fn load_env_file(path: &Path) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("onelf-rt: failed to read {}: {e}", path.display());
            return;
        }
    };

    for line in io::BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Handle "unset VAR"
        if let Some(var) = trimmed.strip_prefix("unset ") {
            let var = var.trim();
            if !var.is_empty() {
                unsafe { env::remove_var(var) };
            }
            continue;
        }

        // Handle "KEY=VALUE"
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() {
                unsafe { env::set_var(key, value) };
            }
        }
    }
}

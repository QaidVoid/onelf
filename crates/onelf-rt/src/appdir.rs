//! AppDir launch mode.
//!
//! When the onelf-rt binary runs as a bare `AppRun` launcher inside an
//! unpacked AppDir (no embedded payload), it self-locates the AppDir, reads
//! the `.onelf/` metadata that `bundle-libs --apprun` wrote, and launches the
//! entrypoint with the same interpreter / library-path handling as the packed
//! cache mode. This is what lets an AppDir run with no baked rpath: the
//! launcher passes `--library-path` to the bundled interpreter and the
//! onelf-env interposer keeps it in place across re-execs.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Walk up from `start` to the directory containing a `.onelf/` directory.
fn find_appdir(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    for _ in 0..16 {
        let d = dir?;
        if d.join(".onelf").is_dir() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Run as an AppDir launcher. `args` is argv[1..]. Returns `Err` if this is
/// not an AppDir (so the caller can fall back); on a successful launch it
/// never returns.
pub fn run(exe_dir: &Path, argv0: &str, exec_path: &str, args: &[String]) -> std::io::Result<()> {
    let not_found = |m: &str| std::io::Error::new(std::io::ErrorKind::NotFound, m.to_string());

    let appdir = find_appdir(exe_dir).ok_or_else(|| not_found("no .onelf AppDir found"))?;
    let meta = appdir.join(".onelf");

    // Entrypoint: `.onelf/command`, otherwise the first argument.
    let (target_rel, prog_args): (String, Vec<String>) = match read_trim(&meta.join("command")) {
        Some(c) if !c.is_empty() => (c, args.to_vec()),
        _ => match args.split_first() {
            Some((first, rest)) => (first.clone(), rest.to_vec()),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "AppRun: no entrypoint (set .onelf/command or pass a target path)",
                ));
            }
        },
    };

    let target_path = appdir.join(&target_rel);
    if !target_path.exists() {
        return Err(not_found(&format!(
            "AppRun: entrypoint not found: {}",
            target_path.display()
        )));
    }

    // Colon-joined library subpath (relative dirs) from `.onelf/libpath`.
    let lib_subpath = read_trim(&meta.join("libpath"))
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(":")
        })
        .unwrap_or_default();
    let interp_data = std::fs::read(meta.join("interp")).ok();
    let env_data = std::fs::read(meta.join("env")).ok();

    let appdir_str = appdir.to_str().unwrap_or("");
    let ep_name = Path::new(&target_rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app");
    let target_str = target_path.to_str().unwrap_or("");

    let lib_path = crate::env::setup_env(
        appdir_str,
        argv0,
        exec_path,
        ep_name,
        "appdir",
        &lib_subpath,
        target_str,
    );
    if let Some(data) = &env_data {
        crate::env::apply_custom_env(data, appdir_str);
    }

    // Relative PT_INTERP resolves against the cwd, so chdir into the AppDir.
    let _ = std::env::set_current_dir(&appdir);

    let lib_dirs: Vec<&str> = lib_subpath.split(':').filter(|s| !s.is_empty()).collect();
    let bundled_interp_rel = interp_data
        .as_deref()
        .and_then(crate::interp::parse_bundled_interp_rel);

    if let Some(interp) =
        crate::interp::should_use_userland_exec(&target_path, &appdir, bundled_interp_rel)
    {
        crate::interp::exec_userland(&target_path, &interp, &lib_path, argv0, &prog_args);
    }

    let mut cmd = crate::interp::build_exec_command(
        &target_path,
        &appdir,
        &lib_dirs,
        &lib_path,
        false,
        argv0,
        &prog_args,
    );
    Err(cmd.exec())
}

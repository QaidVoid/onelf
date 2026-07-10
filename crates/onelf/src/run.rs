//! Run an AppDir in place for fast dev iteration.
//!
//! If the AppDir has an `onelf.toml`, use it for entrypoint/args/working-dir.
//! Otherwise auto-discover a single binary under `bin/` and run with defaults.
//! Sets environment variables matching the packed runtime and execs directly.
//! No packing, mounting, or extraction involved.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use onelf_format::WorkingDir;

use crate::bundle;
use crate::recipe;

/// Translate the recipe's [bundle] section into bundle-libs invocation.
fn bundle_from_recipe(dir: &Path, r: &recipe::Recipe) -> io::Result<()> {
    if r.bundle.skip {
        return Ok(());
    }
    let search_path: Vec<PathBuf> = r
        .bundle
        .search_paths
        .iter()
        .map(|s| PathBuf::from(s.as_str()))
        .collect();
    bundle::bundle_libs(&bundle::BundleOptions {
        directory: dir.to_path_buf(),
        target: None,
        lib_dir: PathBuf::from("lib"),
        exclude: r.bundle.exclude.clone(),
        include: r.bundle.include.clone(),
        search_path,
        dry_run: false,
        recursive: true,
        gl: r.bundle.gl,
        dri: r.bundle.dri,
        vulkan: r.bundle.vulkan,
        wayland: r.bundle.wayland,
        gtk: r.bundle.gtk,
        no_gl: r.bundle.no_gl,
        no_dri: r.bundle.no_dri,
        no_vulkan: r.bundle.no_vulkan,
        no_wayland: r.bundle.no_wayland,
        no_gtk: r.bundle.no_gtk,
        strip: r.bundle.strip,
        strict_libc: r.bundle.strict_libc,
        scan_dlopen: r.bundle.scan_dlopen,
        dlopen_extra: r.bundle.dlopen.clone(),
    })
}

pub fn run(
    app_spec: &Path,
    command_override: Option<&str>,
    entrypoint: Option<&str>,
    run_bundle: bool,
    passthrough_args: &[String],
) -> io::Result<()> {
    // Figure out the AppDir: user's spec is either the directory or a recipe file.
    let (dir, recipe) = resolve_app(app_spec)?;
    let dir = dir.canonicalize()?;

    if run_bundle {
        match recipe.as_ref() {
            Some(r) => bundle_from_recipe(&dir, r)?,
            None => eprintln!("onelf run: --bundle skipped: no recipe found in AppDir"),
        }
    }

    let (ep_name, ep_path, ep_args, working_dir) =
        resolve_entrypoint(&recipe, &dir, command_override, entrypoint)?;

    let target = dir.join(&ep_path);
    if !target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("entrypoint target not found: {}", target.display()),
        ));
    }

    // Detect lib dirs in the AppDir (same rule as pack: any dir containing .so files,
    // excluding .onelf/, share/, bin/, etc).
    let lib_dirs = detect_lib_dirs(&dir);
    let lib_paths: Vec<String> = lib_dirs
        .iter()
        .map(|p| dir.join(p).to_string_lossy().into_owned())
        .collect();
    let lib_paths_str = lib_paths.join(":");

    let requested_cwd = match working_dir {
        WorkingDir::PackageRoot => Some(dir.clone()),
        WorkingDir::EntrypointParent => target.parent().map(Path::to_path_buf),
        WorkingDir::Inherit => None,
    };

    let mut cmd = build_exec_command(&target, &dir, &lib_paths_str, &ep_name)?;
    cmd.args(&ep_args);
    cmd.args(passthrough_args);

    if let Some(c) = requested_cwd {
        cmd.env(
            "ONELF_USER_CWD",
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        );
        cmd.current_dir(c);
    }

    cmd.env("ONELF_DIR", &dir);
    cmd.env("ONELF_MODE", "dev");
    cmd.env("ONELF_ARGV0", &ep_name);
    cmd.env("ONELF_EXEC", &target);
    cmd.env("ONELF_ENTRYPOINT", &ep_name);

    let target_is_elf = is_elf_file_at(&target);
    if !lib_paths_str.is_empty() {
        // Only set LD_LIBRARY_PATH for ELF targets. When the entrypoint
        // is a script the kernel hands it to a host interpreter
        // (/bin/sh, /usr/bin/python3, ...) linked against the host
        // glibc. If our bundled lib dir comes first on LD_LIBRARY_PATH,
        // the host ld would load our bundled libc, mixing two glibc
        // versions in one process and crashing before main. Let the
        // script export LD_LIBRARY_PATH itself before exec'ing bundled
        // ELFs.
        if target_is_elf {
            let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            // Assemble: <bundle lib> : <existing> : <host driver/system dirs>.
            // The bundled loader has its baked-in paths scrubbed, so
            // host-provided GPU drivers (libcuda, libvulkan, libGL,
            // libva) need an explicit entry or Cycles/OptiX/Vulkan
            // won't see them.
            let mut parts: Vec<String> = Vec::new();
            parts.push(lib_paths_str.clone());
            if !existing.is_empty() {
                parts.push(existing);
            }
            let host_drivers = host_driver_paths();
            if !host_drivers.is_empty() {
                parts.push(host_drivers.join(":"));
            }
            cmd.env("LD_LIBRARY_PATH", parts.join(":"));
        }

        // Auto-set GPU/driver paths if the usual subdirs exist.
        let dri_paths: Vec<String> = lib_paths
            .iter()
            .map(|p| Path::new(p).join("dri").to_string_lossy().into_owned())
            .filter(|p| Path::new(p).is_dir())
            .collect();
        if !dri_paths.is_empty() {
            let joined = dri_paths.join(":");
            cmd.env("LIBGL_DRIVERS_PATH", &joined);
            cmd.env("LIBVA_DRIVERS_PATH", &joined);
        }

        let gbm_paths: Vec<String> = lib_paths
            .iter()
            .map(|p| Path::new(p).join("gbm").to_string_lossy().into_owned())
            .filter(|p| Path::new(p).is_dir())
            .collect();
        if !gbm_paths.is_empty() {
            cmd.env("GBM_BACKENDS_PATH", gbm_paths.join(":"));
        }
    }

    let share = dir.join("share");
    if share.is_dir() {
        let existing = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
        let base = share.to_string_lossy();
        let new_val = if existing.is_empty() {
            format!("{base}:/usr/local/share:/usr/share")
        } else {
            format!("{base}:{existing}")
        };
        cmd.env("XDG_DATA_DIRS", new_val);
    }

    // exec never returns on success.
    let err = cmd.exec();
    Err(io::Error::other(format!("exec failed: {err}")))
}

/// Build a Command for executing the target binary. Handles the cross-libc
/// case where the target's PT_INTERP doesn't exist on the host but a matching
/// interpreter is bundled in the AppDir's lib directories.
/// Locate the AppDir root and an optional recipe from a user-supplied path.
/// Accepts a directory (with or without onelf.toml) or a `.toml` file.
fn resolve_app(spec: &Path) -> io::Result<(PathBuf, Option<recipe::Recipe>)> {
    if spec.is_dir() {
        let toml = spec.join("onelf.toml");
        let recipe = if toml.is_file() {
            Some(recipe::load(&toml)?)
        } else {
            None
        };
        return Ok((spec.to_path_buf(), recipe));
    }
    if spec.is_file() && spec.extension().and_then(|s| s.to_str()) == Some("toml") {
        let recipe = recipe::load(spec)?;
        let dir = spec
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        return Ok((dir, Some(recipe)));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{}: expected an AppDir (directory) or an onelf.toml file",
            spec.display()
        ),
    ))
}

/// Pick the entrypoint and its args, consulting (in order): `--command`
/// override, the recipe's named entrypoints, the recipe's default command,
/// or an auto-detected binary under `<dir>/bin/`.
fn resolve_entrypoint(
    recipe: &Option<recipe::Recipe>,
    dir: &Path,
    command_override: Option<&str>,
    entrypoint: Option<&str>,
) -> io::Result<(String, String, Vec<String>, WorkingDir)> {
    let working_dir: WorkingDir = recipe
        .as_ref()
        .map(|r| r.package.working_dir.clone().into())
        .unwrap_or(WorkingDir::Inherit);

    if let Some(cmd) = command_override {
        let name = Path::new(cmd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app")
            .to_string();
        return Ok((name, cmd.to_string(), Vec::new(), working_dir));
    }

    if let Some(r) = recipe {
        if let Some(name) = entrypoint {
            let ep = r
                .entrypoint
                .iter()
                .find(|e| e.name == name)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("entrypoint '{name}' not defined in recipe"),
                    )
                })?;
            return Ok((
                ep.name.clone(),
                ep.path.clone(),
                ep.args.clone(),
                working_dir,
            ));
        }
        if let Some(ep) = r.entrypoint.iter().find(|e| e.default) {
            return Ok((
                ep.name.clone(),
                ep.path.clone(),
                ep.args.clone(),
                working_dir,
            ));
        }
        let name = r.package.name.clone().unwrap_or_else(|| {
            r.package
                .command
                .rsplit('/')
                .next()
                .unwrap_or("app")
                .to_string()
        });
        return Ok((name, r.package.command.clone(), Vec::new(), working_dir));
    }

    if entrypoint.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--entrypoint requires an onelf.toml in the AppDir",
        ));
    }

    // No recipe, no override: find a single executable under bin/.
    let bin = dir.join("bin");
    if !bin.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{}: no onelf.toml and no bin/ subdirectory; pass --command PATH",
                dir.display()
            ),
        ));
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(&bin)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}: no executables found", bin.display()),
        ));
    }
    if entries.len() > 1 {
        let names: Vec<String> = entries
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
            .collect();
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: multiple binaries found; pass --command bin/NAME (have: {})",
                bin.display(),
                names.join(", ")
            ),
        ));
    }
    let bin_path = &entries[0];
    let name = bin_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app")
        .to_string();
    let rel = format!("bin/{name}");
    Ok((name, rel, Vec::new(), working_dir))
}

/// Result of choosing how to exec the target.
///
/// When the target's PT_INTERP was patched to a relative path that
/// resolves under `app_dir` (i.e. `bundle-libs` did its job), we can
/// execve the target directly and let the kernel load the bundled ld.
/// That keeps `/proc/self/exe` pointing at the real binary, which is
/// what Python, Electron and friends read to find their resources.
///
/// Falls back to invoking the bundled loader explicitly when we have
/// one but PT_INTERP wasn't patched, or to a bare exec if nothing is
/// bundled (the host must have the right loader).
fn build_exec_command(
    target: &Path,
    app_dir: &Path,
    lib_path: &str,
    argv0: &str,
) -> io::Result<Command> {
    if let Some(interp) = read_elf_interp(target) {
        let interp_path = Path::new(&interp);

        if let Some(bundled) = find_bundled_interp(&interp, app_dir) {
            let is_musl = interp_path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ld-musl-"));
            let mut cmd = Command::new(&bundled);
            if !is_musl {
                cmd.arg("--inhibit-cache");
            }
            if !lib_path.is_empty() {
                cmd.arg("--library-path").arg(lib_path);
            }
            cmd.arg("--argv0").arg(argv0).arg(target);
            return Ok(cmd);
        }

        if !interp_path.exists() {
            eprintln!(
                "warning: ELF interpreter {} not found on host and no bundled \
                 equivalent in the AppDir; exec will likely fail",
                interp
            );
        }
    }

    let mut cmd = Command::new(target);
    cmd.arg0(argv0);
    Ok(cmd)
}

/// Read PT_INTERP from an ELF file. Returns None for non-ELF or missing interp.
/// Strips trailing NULs introduced by our PT_INTERP patching, which pads the
/// slot with zero bytes rather than shrinking p_filesz.
fn read_elf_interp(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    goblin::elf::Elf::parse(&data)
        .ok()?
        .interpreter
        .map(|s| s.trim_end_matches('\0').to_string())
}

/// Look in the AppDir's lib dirs for a file matching the PT_INTERP basename.
fn find_bundled_interp(interp: &str, app_dir: &Path) -> Option<PathBuf> {
    let name = Path::new(interp).file_name()?.to_os_string();
    for rel in detect_lib_dirs(app_dir) {
        let candidate = app_dir.join(&rel).join(&name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// True if `path` is an ELF file (first four bytes `\x7fELF`). Scripts
/// (shebang `#!`) and missing files return false. Used to decide
/// whether it's safe to set `LD_LIBRARY_PATH` in the child's env:
/// scripts get handed to a host interpreter linked against the host's
/// glibc, and pointing it at our bundled libc mixes versions and
/// crashes before main.
fn is_elf_file_at(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    matches!(f.read(&mut buf), Ok(4)) && buf == *b"\x7fELF"
}

/// Host driver / system library directories to append to LD_LIBRARY_PATH.
/// See `onelf-rt`'s `env.rs` for rationale; kept in sync here so `onelf run`
/// gets the same driver discovery behavior as packed runs.
fn host_driver_paths() -> Vec<String> {
    const CANDIDATES: &[&str] = &[
        "/run/opengl-driver/lib",
        "/run/opengl-driver-32/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib64",
        "/usr/lib",
        "/lib/x86_64-linux-gnu",
        "/lib64",
    ];
    CANDIDATES
        .iter()
        .filter(|p| Path::new(p).is_dir())
        .map(|s| (*s).to_string())
        .collect()
}

/// Find subdirectories of `dir` that contain `.so*` files, skipping obvious
/// non-lib roots. Mirrors the heuristic in pack.rs's auto_detect_lib_dirs.
fn detect_lib_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut lib_dirs: Vec<PathBuf> = Vec::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false).sort(true) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains(".so") {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        let Ok(rel) = parent.strip_prefix(dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        if rel_str.is_empty()
            || rel_str.starts_with(".onelf")
            || rel_str.starts_with("share/")
            || rel_str == "bin"
            || rel_str.starts_with("bin/")
            || rel_str.starts_with("etc/")
        {
            continue;
        }
        let rel_path = rel.to_path_buf();
        if !lib_dirs.contains(&rel_path) {
            lib_dirs.push(rel_path);
        }
    }
    lib_dirs
}

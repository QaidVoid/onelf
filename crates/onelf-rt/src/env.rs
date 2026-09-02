//! Environment variable setup for the running package.
//!
//! Sets `ONELF_*` variables and computes a `lib_path` string for the
//! dynamic linker's `--library-path` flag. Also auto-detects and configures
//! paths for graphics drivers (OpenGL/EGL/Vulkan/VA-API).
//!
//! `LD_LIBRARY_PATH` is intentionally NOT set for ELF entrypoints. It
//! would be inherited by every child process the packed app spawns,
//! including host binaries (`/bin/sh`, `ssh`, etc.), which corrupts them
//! by mixing the bundled libc/libcrypto with the host loader. Instead,
//! the lib path is passed via `--library-path` on a single linker
//! invocation.

use std::env;
use std::path::Path;

/// Set up environment variables and return a colon-joined lib path
/// string for use with the dynamic linker's `--library-path` flag.
///
/// For shebang scripts (non-ELF target), returns an empty string: the
/// kernel hands off to a host interpreter linked against the host glibc,
/// and pointing it at our bundled libs would mix two glibcs in one
/// process. Scripts that need bundled libs must export `LD_LIBRARY_PATH`
/// themselves before execing bundled binaries.
// Describes a single exec; every argument is distinct and a parameter
// object would be built at each call site and read once.
#[allow(clippy::too_many_arguments)]
pub fn setup_env(
    onelf_dir: &str,
    argv0: &str,
    exec_path: &str,
    entrypoint_name: &str,
    mode: &str,
    lib_subpath: &str,
    target_path: &str,
    farm: Option<&Path>,
) -> String {
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
        env::set_var("ONELF_ACTIVE_MODE", mode);
    }

    if onelf_dir.is_empty() {
        return String::new();
    }

    let pkg = Path::new(onelf_dir);

    let target_is_elf = is_elf_file(target_path);

    let mut lib_path = String::new();

    // Build the library search path for ELF entrypoints. Order:
    //   <link farm> : <bundled lib dirs> : <existing LD_LIBRARY_PATH>
    // The farm holds the host libraries the resolver chose over their
    // bundled copies, so it has to come first to shadow them. No host
    // directory is ever placed here: a soname the bundle lacks and the
    // resolver did not choose must fail by name rather than be satisfied
    // by whatever the host has.
    if target_is_elf && !lib_subpath.is_empty() {
        let lib_paths: Vec<String> = lib_subpath
            .split(':')
            .map(|p| pkg.join(p).to_string_lossy().to_string())
            .collect();
        let lib_str = lib_paths.join(":");
        if !lib_str.is_empty() {
            let mut parts: Vec<String> = Vec::new();
            if let Some(farm) = farm {
                parts.push(farm.to_string_lossy().into_owned());
            }
            parts.push(lib_str);
            // Preserve the user's pre-existing LD_LIBRARY_PATH as a middle
            // layer, but don't propagate it to the child env.
            let existing = env::var("LD_LIBRARY_PATH").unwrap_or_default();
            if !existing.is_empty() {
                parts.push(existing);
            }
            lib_path = parts.join(":");

            // LD_LIBRARY_PATH is deliberately not set here. The paths go to
            // the linker as --library-path, on the one invocation that needs
            // them, so nothing the app spawns inherits them. Only the
            // bootstrap path, which drives no linker invocation of its own,
            // still sets the variable, and it does so on that command alone
            // (see interp::build_exec_command).

            // Auto-set LIBGL_DRIVERS_PATH and LIBVA_DRIVERS_PATH if any lib dir
            // contains a dri/ subdirectory (both use the same paths)
            let dri_paths: Vec<String> = lib_paths
                .iter()
                .map(|p| Path::new(p).join("dri").to_string_lossy().to_string())
                .filter(|p| Path::new(p).is_dir())
                .collect();
            if !dri_paths.is_empty() {
                let joined = dri_paths.join(":");
                if env::var("LIBGL_DRIVERS_PATH").is_err() {
                    unsafe {
                        env::set_var("LIBGL_DRIVERS_PATH", &joined);
                    }
                }
                if env::var("LIBVA_DRIVERS_PATH").is_err() {
                    unsafe {
                        env::set_var("LIBVA_DRIVERS_PATH", &joined);
                    }
                }
            }

            // Auto-set GBM_BACKENDS_PATH if any lib dir contains a gbm/ subdirectory
            if env::var("GBM_BACKENDS_PATH").is_err() {
                let gbm_paths: Vec<String> = lib_paths
                    .iter()
                    .map(|p| Path::new(p).join("gbm").to_string_lossy().to_string())
                    .filter(|p| Path::new(p).is_dir())
                    .collect();
                if !gbm_paths.is_empty() {
                    unsafe {
                        env::set_var("GBM_BACKENDS_PATH", gbm_paths.join(":"));
                    }
                }
            }
        }
    }

    // Prepend package's share/ to XDG_DATA_DIRS so bundled GSettings schemas,
    // icons, mime types, etc. are discoverable by GLib/GTK. Host dirs are kept
    // so system themes, schemas, and desktop integrations still work.
    setup_xdg_data_dirs(pkg);

    // EGL vendor discovery: merge bundled + host dirs so both Mesa
    // and proprietary drivers (NVIDIA, AMD) are visible to libglvnd.
    if env::var("__EGL_VENDOR_LIBRARY_DIRS").is_err() {
        let mut egl_dirs: Vec<String> = Vec::new();
        let egl_dir = pkg.join("share/glvnd/egl_vendor.d");
        if egl_dir.is_dir() {
            egl_dirs.push(egl_dir.to_string_lossy().into_owned());
        }
        for d in &[
            "/run/opengl-driver/share/glvnd/egl_vendor.d",
            "/etc/glvnd/egl_vendor.d",
            "/usr/share/glvnd/egl_vendor.d",
        ] {
            if Path::new(d).is_dir() {
                egl_dirs.push((*d).to_string());
            }
        }
        if !egl_dirs.is_empty() {
            unsafe {
                env::set_var("__EGL_VENDOR_LIBRARY_DIRS", egl_dirs.join(":"));
            }
        }
    }

    // Auto-set DRIRC_CONFIGDIR if package has DRI config files
    if env::var("DRIRC_CONFIGDIR").is_err() {
        let drirc_dir = pkg.join("share/drirc.d");
        if drirc_dir.is_dir() {
            unsafe {
                env::set_var("DRIRC_CONFIGDIR", drirc_dir.to_string_lossy().as_ref());
            }
        }
    }

    // Auto-set LIBDRM_IDS_PATH if package has libdrm data
    if env::var("LIBDRM_IDS_PATH").is_err() {
        let libdrm_dir = pkg.join("share/libdrm");
        if libdrm_dir.is_dir() {
            unsafe {
                env::set_var("LIBDRM_IDS_PATH", libdrm_dir.to_string_lossy().as_ref());
            }
        }
    }

    // Auto-set XKB_CONFIG_ROOT if package has xkb data
    if env::var("XKB_CONFIG_ROOT").is_err() {
        let xkb_dir = pkg.join("share/X11/xkb");
        if xkb_dir.is_dir() {
            unsafe {
                env::set_var("XKB_CONFIG_ROOT", xkb_dir.to_string_lossy().as_ref());
            }
        }
    }

    // Auto-set LIBDECOR_PLUGIN_DIR if package has libdecor plugins
    if env::var("LIBDECOR_PLUGIN_DIR").is_err() {
        let libdecor_dir = pkg.join("share/libdecor/plugins-1");
        if libdecor_dir.is_dir() {
            unsafe {
                env::set_var(
                    "LIBDECOR_PLUGIN_DIR",
                    libdecor_dir.to_string_lossy().as_ref(),
                );
            }
        }
    }

    // Vulkan ICD discovery: use VK_ADD_DRIVER_FILES to *append* our
    // bundled ICD configs to the loader's default search. Setting
    // VK_DRIVER_FILES would *replace* the search and cut off host GPU
    // drivers (e.g. NVIDIA's ICD on NixOS at /run/opengl-driver/...).
    // Also include well-known host ICD paths that the bundled loader
    // can't find on its own (its compiled-in /etc and /usr paths are
    // scrubbed).
    if env::var("VK_DRIVER_FILES").is_err() && env::var("VK_ADD_DRIVER_FILES").is_err() {
        let mut icd_dirs: Vec<String> = Vec::new();

        let vk_dir = pkg.join("share/vulkan/icd.d");
        if vk_dir.is_dir() {
            icd_dirs.push(vk_dir.to_string_lossy().into_owned());
        }
        // Host ICD locations that the scrubbed loader can't reach.
        for d in &[
            "/run/opengl-driver/share/vulkan/icd.d",
            "/etc/vulkan/icd.d",
            "/usr/share/vulkan/icd.d",
        ] {
            if Path::new(d).is_dir() {
                icd_dirs.push((*d).to_string());
            }
        }

        if !icd_dirs.is_empty() {
            let mut all_files: Vec<String> = Vec::new();
            for dir in &icd_dirs {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for e in entries.filter_map(|e| e.ok()) {
                        if e.path().extension().is_some_and(|ext| ext == "json") {
                            all_files.push(e.path().to_string_lossy().into_owned());
                        }
                    }
                }
            }
            if !all_files.is_empty() {
                unsafe {
                    env::set_var("VK_DRIVER_FILES", all_files.join(":"));
                }
            }
        }
    }

    lib_path
}

/// Check whether `path` is an ELF file (first four bytes `\x7fELF`).
/// Scripts (shebang `#!`) return false; missing files also return false.
pub(crate) fn is_elf_file(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    matches!(f.read(&mut buf), Ok(4)) && buf == *b"\x7fELF"
}

/// Prepend the package's `share/` to `XDG_DATA_DIRS` so GLib/GTK can find
/// bundled GSettings schemas, icons, MIME types, etc. Host dirs are preserved
/// so system themes and desktop integrations still work.
fn setup_xdg_data_dirs(pkg: &Path) {
    let share = pkg.join("share");
    if !share.is_dir() {
        return;
    }

    let pkg_share = share.to_string_lossy();
    let existing = env::var("XDG_DATA_DIRS").unwrap_or_default();

    let new_val = if existing.is_empty() {
        // XDG spec default when unset is /usr/local/share:/usr/share
        format!("{pkg_share}:/usr/local/share:/usr/share")
    } else {
        format!("{pkg_share}:{existing}")
    };

    unsafe {
        env::set_var("XDG_DATA_DIRS", new_val);
    }
}

/// Expand `${NAME}` references in a `.onelf/env` value at runtime.
/// `${ONELF_DIR}` resolves to the package root; any other `${NAME}`
/// resolves to the *live* process environment. POSIX `${NAME:-word}`
/// is supported: if `NAME` is unset *or empty*, the literal `word` is
/// used instead (no nested braces in `word`). This is what makes the
/// default `PATH = "${ONELF_DIR}/bin:${PATH:-/usr/bin:/bin}"` prepend
/// the bundled bin/ while still falling back to system dirs (instead
/// of a dangling empty element) when the inherited PATH is empty.
/// Unterminated `${` is left literal.
fn expand_env_value(val: &str, onelf_dir: &str) -> String {
    let mut out = String::with_capacity(val.len());
    let b = val.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$'
            && i + 1 < b.len()
            && b[i + 1] == b'{'
            && let Some(end) = b[i + 2..].iter().position(|&c| c == b'}')
        {
            let token = &val[i + 2..i + 2 + end];
            let (name, default) = match token.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (token, None),
            };
            let resolved = if name == "ONELF_DIR" {
                Some(onelf_dir.to_string())
            } else {
                env::var(name).ok()
            };
            match resolved {
                Some(ref v) if !v.is_empty() => out.push_str(v),
                _ => out.push_str(default.unwrap_or("")),
            }
            i += 3 + end;
            continue;
        }
        let ch = val[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Apply custom environment variables from `.onelf/env` data.
/// Each line is `KEY=VALUE`. `${ONELF_DIR}` expands to the package
/// root; other `${NAME}` expand against the live environment (see
/// [`expand_env_value`]).
///
/// This is the *first-launch / fallback* env layer. The re-exec-safe
/// layer is the bundled `onelf-env` constructor (injected as a
/// DT_NEEDED of the entrypoint), which re-applies the same `.onelf/env`
/// on every exec including after a sandboxed `clearenv()`. The two are
/// intentionally redundant: when the constructor is present they set
/// identical `KEY=VALUE` pairs (order-independent, last-writer-wins), so
/// double application is a no-op. This runtime pass is still required
/// for packages where the constructor could not be wired (patchelf
/// unavailable at pack time, no onelf-env blob for the target arch, or
/// self-extract binaries that can't take a DT_NEEDED).
pub fn apply_custom_env(env_data: &[u8], onelf_dir: &str) {
    let Ok(text) = std::str::from_utf8(env_data) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let expanded = expand_env_value(val.trim(), onelf_dir);
            unsafe {
                env::set_var(key.trim(), expanded);
            }
        }
    }
}

#[cfg(test)]
mod expand_value_tests {
    use super::expand_env_value;

    #[test]
    fn onelf_dir_live_env_and_unset() {
        // Unique names so parallel tests don't race on the process env.
        unsafe {
            std::env::set_var("ONELF_T_LIVE_9c1", "LV");
            std::env::remove_var("ONELF_T_UNSET_9c1");
        }
        assert_eq!(expand_env_value("${ONELF_DIR}/x", "/root"), "/root/x");
        assert_eq!(expand_env_value("a:${ONELF_T_LIVE_9c1}:b", "/r"), "a:LV:b");
        // Unset -> empty (so `dir:${UNSET}` doesn't keep a literal token).
        assert_eq!(expand_env_value("${ONELF_T_UNSET_9c1}", "/r"), "");
        // The PATH-prepend shape.
        assert_eq!(
            expand_env_value("${ONELF_DIR}/bin:${ONELF_T_LIVE_9c1}", "/R"),
            "/R/bin:LV"
        );
        // Unterminated `${` is left literal.
        assert_eq!(expand_env_value("a${ONELF_DIR", "/r"), "a${ONELF_DIR");
    }

    #[test]
    fn posix_default_word() {
        unsafe {
            std::env::set_var("ONELF_T_SET_d2", "S");
            std::env::set_var("ONELF_T_EMPTY_d2", "");
            std::env::remove_var("ONELF_T_MISSING_d2");
        }
        // Unset -> default word.
        assert_eq!(
            expand_env_value("${ONELF_T_MISSING_d2:-fallback}", "/r"),
            "fallback"
        );
        // Set & non-empty -> the value, default ignored.
        assert_eq!(expand_env_value("${ONELF_T_SET_d2:-fb}", "/r"), "S");
        // Set but empty -> default (POSIX :- semantics).
        assert_eq!(expand_env_value("${ONELF_T_EMPTY_d2:-fb}", "/r"), "fb");
        // ONELF_DIR is non-empty so the default is ignored.
        assert_eq!(expand_env_value("${ONELF_DIR:-x}", "/R"), "/R");
        // The shipped default PATH shape, inherited PATH empty.
        unsafe { std::env::set_var("ONELF_T_PE_d2", "") };
        assert_eq!(
            expand_env_value("${ONELF_DIR}/bin:${ONELF_T_PE_d2:-/usr/bin:/bin}", "/R"),
            "/R/bin:/usr/bin:/bin"
        );
    }
}

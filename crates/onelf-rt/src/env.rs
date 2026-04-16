//! Environment variable setup for the running package.
//!
//! Sets `ONELF_*` variables and configures `LD_LIBRARY_PATH` for packages
//! that bundle shared libraries. Also auto-detects and configures paths for
//! graphics drivers (OpenGL/EGL/Vulkan/VA-API).

use std::env;
use std::path::Path;

pub fn setup_env(
    onelf_dir: &str,
    argv0: &str,
    exec_path: &str,
    entrypoint_name: &str,
    mode: &str,
    lib_subpath: &str,
    target_path: &str,
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

    if onelf_dir.is_empty() {
        return;
    }

    let pkg = Path::new(onelf_dir);

    // Don't set LD_LIBRARY_PATH when the entrypoint is a script (a
    // shebang file that the kernel hands off to a host interpreter).
    // The host interpreter is linked against the host's glibc, but our
    // bundled libs come first on LD_LIBRARY_PATH, so the host's
    // ld-linux.so.2 would end up loading our bundled libc.so.6. Mixing
    // two glibc versions inside one process blows up with a null deref
    // in the loader's final mprotect/prlimit64 sequence. Leave the
    // script's environment clean; the script can export LD_LIBRARY_PATH
    // itself right before it execs bundled binaries.
    let target_is_elf = is_elf_file(target_path);

    // Auto-set LD_LIBRARY_PATH if package has lib directories
    if target_is_elf && !lib_subpath.is_empty() {
        let lib_paths: Vec<String> = lib_subpath
            .split(':')
            .map(|p| pkg.join(p).to_string_lossy().to_string())
            .collect();
        let lib_str = lib_paths.join(":");
        if !lib_str.is_empty() {
            let existing = env::var("LD_LIBRARY_PATH").unwrap_or_default();
            // Assemble the search path as:
            //   <bundled lib dirs> : <existing LD_LIBRARY_PATH> : <host driver/system dirs>
            // Bundled libs win, but GPU / libGL / libcuda / libvulkan and
            // other host-provided userspace drivers are discoverable. Our
            // bundled ld.so has its baked-in paths scrubbed, so drivers
            // that normally live in /usr/lib (or /run/opengl-driver/lib
            // on NixOS) have to be added here explicitly or Cycles/OptiX
            // and similar features won't find their driver libraries.
            let mut parts: Vec<String> = Vec::new();
            parts.push(lib_str.clone());
            if !existing.is_empty() {
                parts.push(existing);
            }
            let host_paths = host_driver_paths();
            if !host_paths.is_empty() {
                parts.push(host_paths.join(":"));
            }
            unsafe {
                env::set_var("LD_LIBRARY_PATH", parts.join(":"));
            }

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

    // Auto-set __EGL_VENDOR_LIBRARY_DIRS if package has EGL vendor configs
    if env::var("__EGL_VENDOR_LIBRARY_DIRS").is_err() {
        let egl_dir = pkg.join("share/glvnd/egl_vendor.d");
        if egl_dir.is_dir() {
            unsafe {
                env::set_var(
                    "__EGL_VENDOR_LIBRARY_DIRS",
                    egl_dir.to_string_lossy().as_ref(),
                );
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

    // Auto-set VK_DRIVER_FILES if package has Vulkan ICD configs
    if env::var("VK_DRIVER_FILES").is_err() {
        let vk_dir = pkg.join("share/vulkan/icd.d");
        if vk_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&vk_dir) {
                let icd_files: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map_or(false, |ext| ext == "json"))
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect();
                if !icd_files.is_empty() {
                    unsafe {
                        env::set_var("VK_DRIVER_FILES", icd_files.join(":"));
                    }
                }
            }
        }
    }
}

/// Check whether `path` is an ELF file (first four bytes `\x7fELF`).
/// Scripts (shebang `#!`) return false; missing files also return false.
fn is_elf_file(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    matches!(f.read(&mut buf), Ok(4)) && buf == *b"\x7fELF"
}

/// Return the host's driver / system library directories that exist on
/// this system, in descending priority order. These are appended to the
/// package's `LD_LIBRARY_PATH` so the bundled loader can locate host-
/// provided GPU userspace drivers (libcuda, libvulkan, libGL, libva)
/// without picking up host libraries that would clash with the bundle's
/// own copies — those come first in the search path.
fn host_driver_paths() -> Vec<String> {
    // NixOS exposes all GPU userspace drivers under /run/opengl-driver,
    // populated from the active nixpkgs `hardware.graphics` closure.
    // On other distros, drivers live in the standard multiarch or lib64
    // paths alongside the rest of the system's runtime libraries.
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

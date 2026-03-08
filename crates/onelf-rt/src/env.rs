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

    // Auto-set LD_LIBRARY_PATH if package has lib directories
    if !lib_subpath.is_empty() {
        let lib_paths: Vec<String> = lib_subpath
            .split(':')
            .map(|p| pkg.join(p).to_string_lossy().to_string())
            .collect();
        let lib_str = lib_paths.join(":");
        if !lib_str.is_empty() {
            let existing = env::var("LD_LIBRARY_PATH").unwrap_or_default();
            unsafe {
                if existing.is_empty() {
                    env::set_var("LD_LIBRARY_PATH", &lib_str);
                } else {
                    env::set_var("LD_LIBRARY_PATH", format!("{lib_str}:{existing}"));
                }
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

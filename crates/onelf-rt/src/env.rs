//! Environment variable setup for the running package.
//!
//! Sets `ONELF_*` variables and computes a `lib_path` string for the
//! dynamic linker's `--library-path` flag. Also auto-detects and configures
//! paths for graphics drivers (OpenGL/EGL/Vulkan/VA-API).
//!
//! `LD_LIBRARY_PATH` is intentionally NOT set. It would be inherited by
//! every child process the packed app spawns, including host binaries
//! (`/bin/sh`, `ssh`, etc.), which corrupts them by mixing the bundled
//! libc/libcrypto with the host loader. Bundled binaries find their
//! libraries through their `$ORIGIN` run path, and the lib path is
//! passed via `--library-path` on a single linker invocation.

use std::env;
use std::path::{Path, PathBuf};

/// `set_var` for a name and value that came from a file: `std` aborts on
/// an empty name, a `=` in the name, or a NUL in either, and a stray line
/// in `.onelf/env` is not worth the whole launch.
pub(crate) fn set_var_checked(key: &str, value: &str) {
    if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
        return;
    }
    // SAFETY: the runtime is single-threaded at this point (before exec)
    unsafe { env::set_var(key, value) };
}

/// Set up environment variables and return a colon-joined lib path
/// string for use with the dynamic linker's `--library-path` flag.
///
/// For shebang scripts (non-ELF target), returns an empty string: the
/// kernel hands off to a host interpreter linked against the host glibc,
/// and pointing it at our bundled libs would mix two glibcs in one
/// process. The bundled binaries a script runs find their libraries
/// through their own run path.
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
    platform_root: Option<&Path>,
    set_lib_dirs: &[PathBuf],
    set_roots: &[PathBuf],
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
        env::set_var(crate::multicall::EXPORTED_VAR, entrypoint_name);
        env::set_var("ONELF_LAUNCH_DIR", &launch_dir);
        env::set_var("ONELF_ACTIVE_MODE", mode);
        // Inherited from a packed app that runs under the host loader and
        // launched this one. Its loader is the answer to that package's
        // resolver, not this one's, and a bootstrapped binary of this
        // package would follow it. The host loader path sets it again on
        // the one command that needs it.
        env::remove_var("ONELF_INTERP");
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
    if !lib_subpath.is_empty() {
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
            parts.push(lib_str.clone());
            // After the bundle's own directories: a set supplies what the
            // bundle left out, and a name the bundle kept stays its own.
            parts.extend(
                set_lib_dirs
                    .iter()
                    .map(|d| d.to_string_lossy().into_owned()),
            );
            // A fetched GL build's directories are never placed here: the
            // path reaches every process the app spawns, and a foreign
            // build's libstdc++ or zlib next to the host's libc would break
            // a host binary. What the app needs from the build comes
            // through the farm, by name, one library at a time.
            // Preserve the user's pre-existing LD_LIBRARY_PATH as a middle
            // layer, but don't propagate it to the child env.
            let existing = env::var("LD_LIBRARY_PATH").unwrap_or_default();
            if !existing.is_empty() {
                parts.push(existing);
            }
            lib_path = parts.join(":");

            // LD_LIBRARY_PATH is deliberately not set here. The paths go to
            // the linker as --library-path, on the one invocation that needs
            // them, so nothing the app spawns inherits them. The farm's and
            // the sets' libraries also show in the bundle's own library
            // directory (see crate::links), where the run path finds them.

            // Kept for scripts written when they had to export it as
            // LD_LIBRARY_PATH. The bundle's own directories hold everything
            // now, so the farm and the sets are not named.
            unsafe {
                env::set_var("ONELF_LIB_PATH", &lib_str);
            }

            // Auto-set LIBGL_DRIVERS_PATH and LIBVA_DRIVERS_PATH if any lib dir
            // contains a dri/ subdirectory (both use the same paths). A
            // fetched GL build's library directories count as well.
            let mut driver_dirs = lib_paths.clone();
            if let Some(root) = platform_root {
                for dir in onelf_format::resolve::GL_BUILD_LIB_DIRS {
                    driver_dirs.push(root.join(dir).to_string_lossy().into_owned());
                }
            }
            let dri_paths: Vec<String> = driver_dirs
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

            // The bundled glibc's charset converters. They bind to that
            // glibc's private symbols, so they are the only ones it can
            // load, and an inherited value naming any others is replaced.
            // A host program the app spawns gets the caller's value back
            // from the exec hooks (see `crate::hostenv`).
            let gconv: Vec<String> = lib_paths
                .iter()
                .map(|p| Path::new(p).join("gconv").to_string_lossy().to_string())
                .filter(|p| Path::new(p).is_dir())
                .collect();
            if !gconv.is_empty() {
                unsafe {
                    env::set_var("GCONV_PATH", gconv.join(":"));
                }
            }

            // Auto-set GBM_BACKENDS_PATH if any lib dir contains a gbm/ subdirectory
            if env::var("GBM_BACKENDS_PATH").is_err() {
                let gbm_paths: Vec<String> = driver_dirs
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
    setup_qt_plugin_path(pkg, set_roots);
    setup_glib_modules(pkg, set_roots);
    setup_fontconfig(pkg, set_roots);
    setup_xdg_data_dirs(pkg, platform_root, set_roots);

    // EGL vendor discovery: merge bundled, fetched and host dirs so both
    // Mesa and proprietary drivers (NVIDIA, AMD) are visible to libglvnd.
    if env::var("__EGL_VENDOR_LIBRARY_DIRS").is_err() {
        let mut egl_dirs: Vec<String> = Vec::new();
        for root in [Some(pkg)].into_iter().chain([platform_root]).flatten() {
            let egl_dir = root.join("share/glvnd/egl_vendor.d");
            if egl_dir.is_dir() {
                egl_dirs.push(egl_dir.to_string_lossy().into_owned());
            }
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

    // Mesa's driver configuration and libdrm's device tables: from the
    // package when it carries them, else from a fetched GL build.
    let data_roots = [Some(pkg), platform_root].into_iter().flatten();
    for (var, rel) in [
        ("DRIRC_CONFIGDIR", "share/drirc.d"),
        ("LIBDRM_IDS_PATH", "share/libdrm"),
    ] {
        if env::var(var).is_ok() {
            continue;
        }
        if let Some(dir) = data_roots.clone().map(|r| r.join(rel)).find(|d| d.is_dir()) {
            unsafe {
                env::set_var(var, dir.to_string_lossy().as_ref());
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

    // The package's libdecor plugins, which only its own libdecor can
    // load, replace an inherited directory.
    let libdecor_dir = pkg.join("share/libdecor/plugins-1");
    if libdecor_dir.is_dir() {
        unsafe {
            env::set_var(
                "LIBDECOR_PLUGIN_DIR",
                libdecor_dir.to_string_lossy().as_ref(),
            );
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

    if target_is_elf {
        lib_path
    } else {
        String::new()
    }
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
/// The package's `share/` goes first, a fetched GL build's after it so
/// the Vulkan loader finds the build's ICD files, then the host's.
/// Point GLib and GTK at the module caches the package and its sets
/// carry.
///
/// Each of these has a path compiled into the library that reads it, and
/// that path is the packaging distribution's. Left alone, a bundle finds
/// the host's image loaders, input methods and GIO modules, which are
/// built against the host's libc: on a musl host they cannot be loaded
/// into a glibc process at all, and on a glibc host they are simply the
/// wrong versions. An inherited value names exactly those modules, so
/// the package's replaces it.
fn setup_glib_modules(pkg: &Path, set_roots: &[PathBuf]) {
    let roots = || std::iter::once(pkg).chain(set_roots.iter().map(PathBuf::as_path));

    // The version directory differs between builds, so the cache is
    // looked for one level down rather than named outright.
    let cache = |base: &str, name: &str| -> Option<PathBuf> {
        roots()
            .flat_map(|root| {
                std::fs::read_dir(root.join(base))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path().join(name))
            })
            .find(|p| p.is_file())
    };
    if let Some(path) =
        cache("lib/gdk-pixbuf-2.0", "loaders.cache").and_then(|c| absolutised(&c, pkg))
    {
        set_path("GDK_PIXBUF_MODULE_FILE", path.as_os_str());
    }
    if let Some(path) = cache("lib/gtk-3.0", "immodules.cache").and_then(|c| absolutised(&c, pkg)) {
        set_path("GTK_IM_MODULE_FILE", path.as_os_str());
    }

    let dirs = |rel: &str| -> Option<std::ffi::OsString> {
        let found: Vec<PathBuf> = roots()
            .map(|root| root.join(rel))
            .filter(|d| d.is_dir())
            .collect();
        (!found.is_empty())
            .then(|| env::join_paths(found).ok())
            .flatten()
    };
    if let Some(gio) = dirs("lib/gio/modules") {
        set_path("GIO_EXTRA_MODULES", &gio);
    }
    // GTK 3's modules, theme engines and print backends. Only GTK 3's:
    // GTK 2 reads the same variable and would take GTK 3's modules for
    // its own.
    if let Some(gtk) = dirs("lib/gtk-3.0") {
        set_path("GTK_PATH", &gtk);
    }
}

/// Point fontconfig at the package's fonts without hiding the host's.
///
/// A package shipping `share/fonts` would otherwise be invisible: the
/// host fontconfig only knows host directories, so bundled fonts never
/// render and apps fall back to whatever the host has. The generated
/// config includes the host's own `fonts.conf` first, then names the
/// package directories, so host fonts stay and package fonts win ties.
///
/// A caller's own `FONTCONFIG_FILE` takes the place of the host's
/// `fonts.conf` in the generated config, so it still applies and the
/// package's fonts are still found.
///
/// The config lives under the 0700 per-uid base because fontconfig
/// reads whatever `FONTCONFIG_FILE` names, and a shared /tmp file would
/// let another local user choose fonts for this app. Font caches need
/// no management: fontconfig writes per-directory caches under the
/// user's cache home, keyed by directory path, and the mount paths are
/// stable across launches.
fn setup_fontconfig(pkg: &Path, set_roots: &[PathBuf]) {
    let inherited = env::var_os("FONTCONFIG_FILE").filter(|f| !f.is_empty());
    let include = inherited
        .as_deref()
        .map_or(Path::new(SYSTEM_FONTS_CONF), Path::new);
    let conf = fontconfig_conf(pkg, set_roots, include);
    let Some(conf) = conf else { return };
    let Some(name) = pkg.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let Some(base) = crate::paths::private_dir() else {
        return;
    };
    // No `onelf-` prefix, so the mountpoint sweep leaves it alone.
    let dir = base.join(format!("fontconfig-{name}"));
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // One file per included config, so two launches given different
    // ones do not overwrite each other's.
    let dest = match &inherited {
        None => dir.join("fonts.conf"),
        Some(file) => dir.join(format!(
            "fonts-{:016x}.conf",
            crate::hostenv::fnv1a(file.as_encoded_bytes())
        )),
    };
    if std::fs::write(&dest, conf).is_err() {
        return;
    }
    set_path("FONTCONFIG_FILE", dest.as_os_str());
}

const SYSTEM_FONTS_CONF: &str = "/etc/fonts/fonts.conf";

/// The text of the generated fonts.conf: `include` first, then every
/// root whose `share/fonts` exists. `None` when no root ships fonts.
fn fontconfig_conf(pkg: &Path, set_roots: &[PathBuf], include: &Path) -> Option<String> {
    let dirs = [pkg]
        .into_iter()
        .chain(set_roots.iter().map(PathBuf::as_path))
        .map(|root| root.join("share/fonts"))
        .filter(|d| d.is_dir());
    let mut conf = format!(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
         <fontconfig>\n\
         \x20 <include ignore_missing=\"yes\">{}</include>\n",
        escape_xml(&include.to_string_lossy())
    );
    let mut any = false;
    for dir in dirs {
        any = true;
        conf.push_str(&format!(
            "  <dir>{}</dir>\n",
            escape_xml(&dir.to_string_lossy())
        ));
    }
    if !any {
        return None;
    }
    conf.push_str("</fontconfig>\n");
    Some(conf)
}

/// Escape the five XML specials; everything else passes through.
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// A copy of `cache` whose module paths are absolute, written where the
/// launch can reach it.
///
/// The packed cache names its modules relative to itself, which is the
/// only form that survives being unpacked anywhere. GTK, though,
/// resolves such a path against the working directory rather than
/// against the cache, and so finds nothing. Spelling the paths out at
/// launch, when the root is finally known, satisfies both.
fn absolutised(cache: &Path, pkg: &Path) -> Option<PathBuf> {
    let dir = cache.parent()?;
    let text = std::fs::read_to_string(cache).ok()?;
    let mut out = String::with_capacity(text.len() + 256);
    let mut rewrote = false;
    for (i, piece) in text.split('"').enumerate() {
        if i > 0 {
            out.push('"');
        }
        // Odd pieces are the quoted ones. A module path is the only kind
        // with a directory in it and a shared object at the end.
        let is_module =
            i % 2 == 1 && piece.contains('/') && piece.contains(".so") && !piece.starts_with('/');
        if is_module {
            out.push_str(&dir.join(piece).to_string_lossy());
            rewrote = true;
        } else {
            out.push_str(piece);
        }
    }
    if !rewrote {
        return Some(cache.to_path_buf());
    }
    // Under the 0700 per-uid base, not shared /tmp: another local user
    // could otherwise swap the cache between this write and GTK's read,
    // and the rewritten cache names modules GTK dlopens. Named after the
    // package root, which carries the package's hash, so a second launch
    // of the same package reuses this rather than leaving another copy
    // behind. No `onelf-` prefix, so the mountpoint sweep leaves it
    // alone (same rule as `resolve_store`).
    let base = crate::paths::private_dir()?;
    let dest_dir = base.join(format!("modules-{}", pkg.file_name()?.to_string_lossy()));
    std::fs::create_dir_all(&dest_dir).ok()?;
    let dest = dest_dir.join(cache.file_name()?);
    std::fs::write(&dest, out).ok()?;
    Some(dest)
}

fn set_path(name: &str, value: &std::ffi::OsStr) {
    // SAFETY: the runtime is single-threaded at this point
    unsafe { env::set_var(name, value) };
}

/// Point Qt at the plugin directories the package and its sets carry.
///
/// A relocatable Qt finds its plugins relative to the executable, which
/// holds while the toolkit sits in the same tree as the application. A
/// shared set moves it out, and Qt then looks under the application's own
/// root, finds no platform plugin and aborts before its first window.
/// Naming the directories outright is what survives either layout. An
/// inherited value is replaced: the plugins it names were built against
/// some other Qt.
fn setup_qt_plugin_path(pkg: &Path, set_roots: &[PathBuf]) {
    let dirs: Vec<String> = std::iter::once(pkg)
        .chain(set_roots.iter().map(PathBuf::as_path))
        .flat_map(|root| ["lib/qt6/plugins", "lib/qt5/plugins"].map(|d| root.join(d)))
        .filter(|d| d.is_dir())
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    if !dirs.is_empty() {
        // SAFETY: the runtime is single-threaded at this point (before exec)
        unsafe { env::set_var("QT_PLUGIN_PATH", dirs.join(":")) };
    }
}

fn setup_xdg_data_dirs(pkg: &Path, platform_root: Option<&Path>, set_roots: &[PathBuf]) {
    let shares: Vec<String> = [Some(pkg), platform_root]
        .into_iter()
        .flatten()
        .chain(set_roots.iter().map(PathBuf::as_path))
        .map(|root| root.join("share"))
        .filter(|share| share.is_dir())
        .map(|share| share.to_string_lossy().into_owned())
        .collect();
    if shares.is_empty() {
        return;
    }

    let pkg_share = shares.join(":");
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
/// identical `KEY=VALUE` pairs (order-independent, last-writer-wins), and
/// a value that refers to its own variable is left alone once applied
/// (see [`already_applied`]), so double application is a no-op. This
/// runtime pass is still required
/// for packages where the constructor could not be wired (patchelf
/// unavailable at pack time, no onelf-env blob for the target arch, or
/// self-extract binaries that can't take a DT_NEEDED).
///
/// An empty `onelf_dir` means there is no package tree, as in memfd
/// mode. A value naming `${ONELF_DIR}` is then skipped, since it would
/// name a path under the filesystem root instead.
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
            let (key, val) = (key.trim(), val.trim());
            if onelf_dir.is_empty()
                && (val.contains("${ONELF_DIR}") || val.contains("${ONELF_DIR:-"))
            {
                continue;
            }
            if already_applied(key, val, onelf_dir) {
                continue;
            }
            set_var_checked(key, &expand_env_value(val, onelf_dir));
        }
    }
}

/// Whether `key` already holds what `val` wraps around its own `${key}`.
///
/// A value such as the default `${ONELF_DIR}/bin:${PATH}` refers to the
/// variable it sets. Applied again, by the `onelf-env` constructor after
/// this pass or by a nested launch of the package, it would add its
/// prefix once more each time. A live value that already starts and ends
/// with what surrounds the reference has had it applied.
fn already_applied(key: &str, val: &str, onelf_dir: &str) -> bool {
    let Some((before, after)) = split_at_self_reference(key, val) else {
        return false;
    };
    let current = env::var(key).unwrap_or_default();
    if current.is_empty() {
        return false;
    }
    let (prefix, suffix) = (
        expand_env_value(before, onelf_dir),
        expand_env_value(after, onelf_dir),
    );
    current.len() >= prefix.len() + suffix.len()
        && current.starts_with(&prefix)
        && current.ends_with(&suffix)
}

/// `val` split around its first `${key}` or `${key:-word}`.
fn split_at_self_reference<'a>(key: &str, val: &'a str) -> Option<(&'a str, &'a str)> {
    let mut from = 0;
    while let Some(at) = val[from..].find("${").map(|i| from + i) {
        let end = at + 2 + val[at + 2..].find('}')?;
        let token = &val[at + 2..end];
        let name = token.split_once(":-").map_or(token, |(name, _)| name);
        if name == key {
            return Some((&val[..at], &val[end + 1..]));
        }
        from = end + 1;
    }
    None
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

#[cfg(test)]
mod apply_custom_env_tests {
    use super::apply_custom_env;

    /// The runtime applies `.onelf/env` and the constructor applies it
    /// again in the entrypoint, so a value naming its own variable has to
    /// come out the same however many times it is applied.
    #[test]
    fn a_self_referential_value_is_applied_once() {
        unsafe { std::env::set_var("ONELF_T_SELF_a1", "/usr/bin") };
        let data = b"ONELF_T_SELF_a1=${ONELF_DIR}/bin:${ONELF_T_SELF_a1:-/bin}\n";
        apply_custom_env(data, "/R");
        apply_custom_env(data, "/R");
        assert_eq!(
            std::env::var("ONELF_T_SELF_a1").as_deref(),
            Ok("/R/bin:/usr/bin")
        );

        unsafe { std::env::remove_var("ONELF_T_SELF_b2") };
        let data = b"ONELF_T_SELF_b2=${ONELF_T_SELF_b2:-/bin}:/R/extra\n";
        apply_custom_env(data, "/R");
        apply_custom_env(data, "/R");
        assert_eq!(
            std::env::var("ONELF_T_SELF_b2").as_deref(),
            Ok("/bin:/R/extra")
        );
    }

    /// Without a package tree a value naming it is skipped, and the rest
    /// are still applied.
    #[test]
    fn without_a_tree_only_values_naming_it_are_skipped() {
        unsafe {
            std::env::set_var("ONELF_T_TREE_c3", "kept");
            std::env::remove_var("ONELF_T_TREE_d4");
        }
        apply_custom_env(
            b"ONELF_T_TREE_c3=${ONELF_DIR}/bin:${ONELF_T_TREE_c3}\nONELF_T_TREE_d4=plain\n",
            "",
        );
        assert_eq!(std::env::var("ONELF_T_TREE_c3").as_deref(), Ok("kept"));
        assert_eq!(std::env::var("ONELF_T_TREE_d4").as_deref(), Ok("plain"));
    }
}

#[cfg(test)]
mod set_var_tests {
    use super::*;

    #[test]
    fn a_bad_env_line_is_skipped_not_fatal() {
        for (k, v) in [("", "x"), ("A=B", "x"), ("A\0", "x"), ("A", "x\0")] {
            set_var_checked(k, v);
        }
        set_var_checked("ONELF_TEST_SET_VAR_CHECKED", "ok");
        assert_eq!(env::var("ONELF_TEST_SET_VAR_CHECKED").as_deref(), Ok("ok"));
    }
}

#[cfg(test)]
mod setup_env_tests {
    use super::*;

    fn call(target_path: &str) -> String {
        setup_env(
            "/pkg",
            "app",
            "/exec",
            "ep",
            "test",
            "lib",
            target_path,
            None,
            None,
            &[],
            &[],
        )
    }

    /// A packed app launched from one that runs under the host loader
    /// inherits that loader's `ONELF_INTERP`, which its own bootstrapped
    /// binaries would otherwise follow whatever its own resolver decided.
    #[test]
    fn an_inherited_interp_override_is_dropped() {
        unsafe { env::set_var("ONELF_INTERP", "/other/package/ld-linux-x86-64.so.2") };
        call("/nonexistent/target");
        assert!(env::var_os("ONELF_INTERP").is_none());
    }

    #[test]
    fn a_script_entrypoint_publishes_the_path_but_returns_none() {
        let dir = env::temp_dir().join(format!("onelf-setup-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("script");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let elf = dir.join("binary");
        std::fs::write(&elf, b"\x7fELF").unwrap();

        unsafe { env::remove_var("ONELF_LIB_PATH") };
        let returned = call(script.to_str().unwrap());
        assert!(returned.is_empty());
        assert!(env::var("ONELF_LIB_PATH").unwrap().starts_with("/pkg/lib"));

        let returned = call(elf.to_str().unwrap());
        assert!(returned.starts_with("/pkg/lib"));
    }
}

#[cfg(test)]
mod fontconfig_conf_tests {
    use super::{SYSTEM_FONTS_CONF, escape_xml, fontconfig_conf};
    use std::path::Path;

    #[test]
    fn lists_only_roots_that_ship_fonts_and_keeps_host_first() {
        let work = std::env::temp_dir().join(format!("onelf-fc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let pkg = work.join("pkg-a");
        let set = work.join("set-b");
        std::fs::create_dir_all(pkg.join("share/fonts")).unwrap();
        std::fs::create_dir_all(set.join("share/mime")).unwrap();

        let conf = fontconfig_conf(
            &pkg,
            std::slice::from_ref(&set),
            Path::new(SYSTEM_FONTS_CONF),
        )
        .expect("pkg ships fonts");

        assert!(conf.contains("<include ignore_missing=\"yes\">/etc/fonts/fonts.conf</include>"));
        assert!(conf.contains(&format!("<dir>{}</dir>", pkg.join("share/fonts").display())));
        assert!(
            !conf.contains("set-b"),
            "a root without fonts is not listed"
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn package_dir_is_listed_before_set_dirs() {
        let work = std::env::temp_dir().join(format!("onelf-fc2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let pkg = work.join("pkg-a");
        let set = work.join("set-b");
        std::fs::create_dir_all(pkg.join("share/fonts")).unwrap();
        std::fs::create_dir_all(set.join("share/fonts")).unwrap();

        let conf = fontconfig_conf(
            &pkg,
            std::slice::from_ref(&set),
            Path::new(SYSTEM_FONTS_CONF),
        )
        .expect("both ship fonts");

        let pkg_at = conf.find(&format!("<dir>{}</dir>", pkg.join("share/fonts").display()));
        let set_at = conf.find(&format!("<dir>{}</dir>", set.join("share/fonts").display()));
        assert!(pkg_at.is_some() && set_at.is_some());
        assert!(pkg_at.unwrap() < set_at.unwrap(), "package fonts win ties");

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn no_root_ships_fonts_means_no_config() {
        let empty = std::env::temp_dir().join(format!("onelf-fc3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(fontconfig_conf(&empty, &[], Path::new(SYSTEM_FONTS_CONF)).is_none());
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn xml_specials_in_paths_are_escaped() {
        assert_eq!(escape_xml("a&b<c>\"d'e"), "a&amp;b&lt;c&gt;&quot;d&apos;e");
    }
}

#[cfg(test)]
mod absolutised_tests {
    use super::absolutised;

    #[test]
    fn lands_under_private_base_not_shared_tmp() {
        // Shared /tmp would let another user swap the cache between the
        // write and GTK's read, so the rewritten copy must live under the
        // 0700 per-uid base.
        let pkg_dir = std::env::temp_dir().join(format!("onelf-abs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pkg_dir);
        let payload_dir = pkg_dir.join("pkg-abcdef01");
        let cache_dir = payload_dir.join("lib/gtk-3.0/3.0.0");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("loaders.cache"),
            "# comment\n\"../loaders/libpixbuf-png.so\"\n",
        )
        .unwrap();

        let got = absolutised(&cache_dir.join("loaders.cache"), &payload_dir).expect("rewritten");

        let base = crate::paths::private_dir().expect("private base exists in tests");
        let expected = base.join("modules-pkg-abcdef01/loaders.cache");
        assert_eq!(got, expected);
        let text = std::fs::read_to_string(&got).unwrap();
        assert!(
            text.contains(
                cache_dir
                    .join("../loaders/libpixbuf-png.so")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(!text.contains("\"../loaders"));

        let _ = std::fs::remove_dir_all(&pkg_dir);
    }
}

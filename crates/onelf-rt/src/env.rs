//! Environment variable setup for the running package.
//!
//! Sets `ONELF_*` variables and computes a `lib_path` string for the
//! dynamic linker's `--library-path` flag. Also auto-detects and configures
//! paths for graphics drivers (OpenGL/EGL/Vulkan/VA-API). The resolved
//! artifact store is exported as `ONELF_PLATFORM_STORE`, a location hint
//! for the processes the app spawns; acquisition never takes a
//! destination from it.
//!
//! Both launch forms apply this machinery: the packed launch drives it on
//! every mode, and the CLI's development run calls the same functions for
//! its AppDir, so the two forms configure one environment the same way.
//!
//! `LD_LIBRARY_PATH` is intentionally NOT set. It would be inherited by
//! every child process the packed app spawns, including host binaries
//! (`/bin/sh`, `ssh`, etc.), which corrupts them by mixing the bundled
//! libc/libcrypto with the host loader. Bundled binaries find their
//! libraries through their `$ORIGIN` run path, and the lib path is
//! passed via `--library-path` on a single linker invocation. The
//! caller's own value is removed from the application's environment, so
//! no host directory is searched ahead of a bundled helper's run path,
//! and the restoration record hands it back to the host programs the app
//! starts.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use std::sync::OnceLock;

use onelf_format::resolve::ActivePin;
use onelf_format::rules::{
    EnvironmentName, Inheritance, PathSelection, ResourceKind, RuntimeBase, RuntimeHookId,
    RuntimePathGroup,
};
use onelf_format::runtime_metadata::RuntimeRuleMetadata;

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

/// Why `name` cannot be a configured environment setting, when it names a
/// dynamic-loader search path.
///
/// `LD_LIBRARY_PATH` and its per-ABI variants are how a process pulls
/// libraries from outside its own tree into its loading, and the search
/// path is the one thing a launch resolves itself: the application loads
/// from its package's own directories and the resolver's decision, and a
/// host program the app spawns loads from the caller's environment. A
/// recipe `[env]` entry naming one of these cannot be honoured without
/// leaking one side into the other, so the setting is refused instead of
/// applied. Framework plugin paths are ordinary variables and stay
/// configurable.
pub fn library_search_override(name: &str) -> Option<&'static str> {
    matches!(
        name,
        "LD_LIBRARY_PATH" | "LD_LIBRARY_PATH_32" | "LD_LIBRARY_PATH_64" | "LD_AOUT_LIBRARY_PATH"
    )
    .then_some("the loader search path is what onelf resolves, not a configurable one")
}

/// The library search path for the application's own libraries: the
/// resolver's link farm first, then the package's library directories,
/// then the selected sets'.
///
/// This one construction serves both launch forms, which hand it to the
/// dynamic linker as `--library-path`. A caller's inherited
/// `LD_LIBRARY_PATH` never joins it: a host directory on the path would
/// hand the application a dependency its package does not carry, and the
/// application's directories on the caller's value would corrupt a host
/// program the caller starts beside this one. [`setup_env`] removes the
/// caller's value from the application's environment, and the
/// restoration record ([`crate::hostenv`]) keeps it for the host programs
/// the app spawns.
///
/// A fetched GL build's directories are never placed here either: the
/// path reaches every process the app spawns, and a foreign build's
/// libstdc++ or zlib next to the host's libc would break a host binary.
/// What the app needs from the build comes through the farm, by name,
/// one library at a time.
///
/// An empty `lib_subpath` names no package libraries and yields an empty
/// path.
pub fn library_search_path(
    pkg_root: &Path,
    lib_subpath: &str,
    farm: Option<&Path>,
    set_lib_dirs: &[PathBuf],
) -> String {
    let bundled = lib_subpath
        .split(':')
        .filter(|p| !p.is_empty())
        .map(|p| pkg_root.join(p).to_string_lossy().into_owned());
    let mut parts: Vec<String> = Vec::new();
    if let Some(farm) = farm {
        // The farm holds the host libraries the resolver chose over their
        // bundled copies, so it has to come first to shadow them. No host
        // directory is ever placed here: a soname the bundle lacks and
        // the resolver did not choose must fail by name rather than be
        // satisfied by whatever the host has.
        parts.push(farm.to_string_lossy().into_owned());
    }
    parts.extend(bundled);
    // After the bundle's own directories: a set supplies what the bundle
    // left out, and a name the bundle kept stays its own.
    parts.extend(
        set_lib_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned()),
    );
    parts.join(":")
}

/// Set up environment variables and return a colon-joined lib path
/// string for use with the dynamic linker's `--library-path` flag.
///
/// The store this launch resolves to is exported as
/// `ONELF_PLATFORM_STORE`, so the processes the app spawns see where the
/// ordinary store resolves and share its builds. The value is a lookup
/// location, never a destination: acquisition derives its store from the
/// target uid, so a privileged launch keeps its own validated store
/// whatever it inherits here.
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

    // The caller's search path would put host directories ahead of every
    // bundled helper's run path and of a kernel-started entrypoint's own
    // directories. The restoration record lists this variable as set at
    // exec, so a host program the app starts gets the caller's value
    // back, and a launch that needs a generated value sets it on the one
    // command that needs it.
    // SAFETY: the runtime is single-threaded at this point (before exec)
    unsafe { env::remove_var("LD_LIBRARY_PATH") };

    let pkg = Path::new(onelf_dir);
    let app_lib_dirs: Vec<PathBuf> = lib_subpath
        .split(':')
        .filter(|p| !p.is_empty())
        .map(|p| pkg.join(p))
        .collect();
    let all_lib_dirs: Vec<PathBuf> = app_lib_dirs
        .iter()
        .cloned()
        .chain(set_lib_dirs.iter().cloned())
        .collect();

    let target_is_elf = is_elf_file(target_path);

    let mut lib_path = String::new();

    // The store resolution acquisition uses, exported so the processes
    // the app spawns share this launch's store. Nothing reads the value
    // back as a destination: a privileged child resolves its own private
    // store exactly as this launch would have.
    if let Some(store) = crate::platform::resolved_store() {
        set_var_checked(crate::platform::STORE_HINT_VAR, &store.to_string_lossy());
    }

    // Build the library search path for ELF entrypoints through the one
    // construction both launch forms share: the application resolves from
    // the farm, its own directories and the sets alone.
    if !lib_subpath.is_empty() {
        let lib_paths: Vec<String> = lib_subpath
            .split(':')
            .map(|p| pkg.join(p).to_string_lossy().to_string())
            .collect();
        let lib_str = lib_paths.join(":");
        if !lib_str.is_empty() {
            lib_path = library_search_path(pkg, lib_subpath, farm, set_lib_dirs);

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
    let active = launch_rules(pkg);
    setup_plugin_dirs(&active, pkg, &app_lib_dirs, set_lib_dirs, set_roots);
    let (hook_lib_dirs, hook_roots) =
        active.with_prepared(pkg, &app_lib_dirs, set_lib_dirs, set_roots);
    run_runtime_hooks(
        &active.hooks,
        &HookContext {
            pkg,
            lib_dirs: &app_lib_dirs,
            set_lib_dirs: &hook_lib_dirs,
            set_roots: &hook_roots,
        },
    );
    setup_terminfo(pkg, set_roots);
    setup_ca_bundle(pkg, set_roots);
    setup_locale(&all_lib_dirs);
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

    setup_vulkan(pkg, platform_root, set_roots, &all_lib_dirs, farm);

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

/// The launch context one registered runtime hook evaluates against.
///
/// Package roots answer path lookups, and the library directories are
/// the dynamic linker's search order.
pub(crate) struct HookContext<'a> {
    /// The application package root.
    pub pkg: &'a Path,
    /// The application's library directories, in search order.
    pub lib_dirs: &'a [PathBuf],
    /// The selected sets' library directories, in search order.
    pub set_lib_dirs: &'a [PathBuf],
    /// The selected sets' tree roots.
    pub set_roots: &'a [PathBuf],
}

/// One registered runtime hook: its closed-registry identity and handler.
struct RuntimeHook {
    /// The identity decoded package metadata names for this handler.
    id: RuntimeHookId,
    /// The handler, taking the launch context.
    run: fn(&HookContext<'_>),
}

/// The registered runtime hooks, in fixed invocation order.
///
/// The registry is closed and bounded: five handlers, declared once and
/// never extended from package data. A launch runs every one without a
/// carried rule record and only the carried IDs with one, always in this
/// order. It is the order the procedural calls used, and each handler
/// keeps the algorithm it had there.
fn runtime_hooks() -> &'static [RuntimeHook] {
    const HOOKS: &[RuntimeHook] = &[
        RuntimeHook {
            id: RuntimeHookId::GdkCacheAbsolutise,
            run: run_gdk_cache_absolutise,
        },
        RuntimeHook {
            id: RuntimeHookId::GioModulePaths,
            run: run_gio_module_paths,
        },
        RuntimeHook {
            id: RuntimeHookId::FontconfigGenerate,
            run: run_fontconfig_generate,
        },
        RuntimeHook {
            id: RuntimeHookId::TclTkLibraries,
            run: run_tcl_tk_libraries,
        },
        RuntimeHook {
            id: RuntimeHookId::AlsaConfigPathNaturalSort,
            run: run_alsa_config_path_natural_sort,
        },
    ];
    HOOKS
}

/// Every registered runtime hook identity, in invocation order: what a
/// launch without a carried rule record runs.
fn all_runtime_hooks() -> Vec<RuntimeHookId> {
    runtime_hooks().iter().map(|hook| hook.id).collect()
}

/// Run the registered runtime hooks `ids` names, in the registry's fixed
/// invocation order whatever order `ids` lists them in.
///
/// Dispatch goes through the by-id lookup, so the launch takes the same
/// path a metadata-declared hook takes.
pub(crate) fn run_runtime_hooks(ids: &[RuntimeHookId], context: &HookContext<'_>) {
    for hook in runtime_hooks() {
        if !ids.contains(&hook.id) {
            continue;
        }
        if let Some(run) = runtime_hook(hook.id) {
            run(context);
        }
    }
}

/// The handler registered for a metadata hook identity, if there is one.
///
/// Decoded package metadata names hooks by [`RuntimeHookId`]; this lookup
/// is how such a hook reaches its handler. Running the whole registry is
/// [`run_runtime_hooks`].
pub(crate) fn runtime_hook(id: RuntimeHookId) -> Option<fn(&HookContext<'_>)> {
    runtime_hooks()
        .iter()
        .find(|hook| hook.id == id)
        .map(|hook| hook.run)
}

/// Point GDK and GTK at the module caches the package and its sets
/// carry, with the module paths absolutised by [`absolutised`].
///
/// Each cache has a path compiled into the library that reads it, and
/// that path is the packaging distribution's. Left alone, a bundle finds
/// the host's image loaders and input methods, which are built against
/// the host's libc: on a musl host they cannot be loaded into a glibc
/// process at all, and on a glibc host they are simply the wrong
/// versions. An inherited value names exactly those modules, so the
/// package's replaces it.
fn run_gdk_cache_absolutise(context: &HookContext<'_>) {
    let roots =
        || std::iter::once(context.pkg).chain(context.set_roots.iter().map(PathBuf::as_path));

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
        cache("lib/gdk-pixbuf-2.0", "loaders.cache").and_then(|c| absolutised(&c, context.pkg))
    {
        set_path("GDK_PIXBUF_MODULE_FILE", path.as_os_str());
    }
    if let Some(path) =
        cache("lib/gtk-3.0", "immodules.cache").and_then(|c| absolutised(&c, context.pkg))
    {
        set_path("GTK_IM_MODULE_FILE", path.as_os_str());
    }
}

/// Give GIO the module directory the package carries, with the remaining
/// directories behind it on `GIO_EXTRA_MODULES`.
///
/// `GIO_MODULE_DIR` replaces GLib's default module directory, so the
/// host's modules, built against the host's GLib, never load into the
/// bundled one. It takes one directory: a set's modules come after the
/// package's on `GIO_EXTRA_MODULES`. A bundle with no modules leaves the
/// default alone, and the host's dconf and gvfs serve it.
fn run_gio_module_paths(context: &HookContext<'_>) {
    let found = |rel: &str| -> Vec<PathBuf> {
        std::iter::once(context.pkg)
            .chain(context.set_roots.iter().map(PathBuf::as_path))
            .map(|root| root.join(rel))
            .filter(|d| d.is_dir())
            .collect()
    };
    let mut gio = found("lib/gio/modules").into_iter();
    if let Some(first) = gio.next() {
        set_path("GIO_MODULE_DIR", first.as_os_str());
        let rest: Vec<PathBuf> = gio.collect();
        if let Ok(rest) = env::join_paths(rest)
            && !rest.is_empty()
        {
            set_path("GIO_EXTRA_MODULES", &rest);
        }
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
fn run_fontconfig_generate(context: &HookContext<'_>) {
    let pkg = context.pkg;
    let set_roots = context.set_roots;
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

/// Where a launch or a development view publishes the evaluated
/// environment input the payload constructor reads back.
pub const VIEW_ENV_FILE: &str = ".onelf/runtime-env";

/// The generated input is capped at one mebibyte, the bound the
/// constructor enforces when it reads the file back; the two numbers must
/// stay equal.
const MAX_VIEW_ENV_BYTES: usize = 1024 * 1024;
/// The most entries the input may carry, as the constructor counts them.
const MAX_VIEW_ENV_ENTRIES: usize = 4096;
/// The longest variable name an entry may carry.
const MAX_VIEW_ENV_NAME: usize = 128;
/// The longest value an entry or scalar record may carry.
const MAX_VIEW_ENV_VALUE: usize = 32 * 1024;
const VIEW_ENV_HEADER: &[u8] = b"ONELF-RUNTIME-ENV 1 ";
const HOST_ENV_VAR: &str = "ONELF_HOST_ENV";
const ROOTS_VAR: &str = "ONELF_ROOTS";

/// One evaluated assignment encoded for the constructor.
struct ViewEnvEntry {
    name: Vec<u8>,
    value: Vec<u8>,
    policy: Inheritance,
    left_set: bool,
}

/// Why a generated input was refused instead of written short.
#[derive(Debug)]
pub enum ViewEnvError {
    /// The whole input would exceed the bound the constructor reads back.
    TooLarge,
}

impl std::fmt::Display for ViewEnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewEnvError::TooLarge => {
                f.write_str("beyond the record bound the constructor reads back")
            }
        }
    }
}

impl std::error::Error for ViewEnvError {}

/// The launch's or a development view's framework mappings, evaluated
/// against the application and set roots the same pass `setup_env`
/// applies them with, so the record carries each mapping's value and
/// inheritance policy.
pub fn view_env_assignments(
    pkg_root: &Path,
    lib_dirs: &[&str],
    set_lib_dirs: &[PathBuf],
    set_roots: &[PathBuf],
) -> Vec<PathAssignment<'static>> {
    let app_lib_dirs: Vec<PathBuf> = lib_dirs.iter().map(|d| pkg_root.join(d)).collect();
    launch_rules(pkg_root).assignments(pkg_root, &app_lib_dirs, set_lib_dirs, set_roots)
}

/// Encode the evaluated per-view environment input.
///
/// Every framework mapping alias is listed with the mapping's own value,
/// whether the launch left the variable set, and the mapping's
/// inheritance policy. Every other variable the launch changed is listed
/// with the value it left, under a replace policy. A variable the launch
/// left unset is not listed: restoration never invents an unset. The
/// active roots and the host-restoration record ride along as scalar
/// records when `now` carries them. A name or value of either kind beyond
/// what the constructor reads back is skipped, since the packaged
/// `.onelf/env` still carries recipe values.
pub fn encode_view_env(
    base: &BTreeMap<OsString, OsString>,
    now: &BTreeMap<OsString, OsString>,
    assignments: &[PathAssignment<'_>],
) -> Result<Vec<u8>, ViewEnvError> {
    let mut mapped = BTreeSet::new();
    let mut entries: Vec<ViewEnvEntry> = Vec::new();
    for assignment in assignments {
        for name in &assignment.variables {
            if !mapped.insert(OsString::from(*name)) {
                continue;
            }
            let value = assignment.value.as_bytes();
            if !fits_view_env(name.as_bytes(), value) {
                continue;
            }
            entries.push(ViewEnvEntry {
                name: name.as_bytes().to_vec(),
                value: value.to_vec(),
                policy: assignment.inheritance,
                left_set: now.contains_key(OsStr::new(name)),
            });
        }
    }
    for (name, value) in now {
        if base.get(name) == Some(value)
            || name.as_bytes() == HOST_ENV_VAR.as_bytes()
            || name.as_bytes() == ROOTS_VAR.as_bytes()
            || mapped.contains(name)
        {
            continue;
        }
        let (name, value) = (name.as_bytes(), value.as_bytes());
        if !fits_view_env(name, value) {
            continue;
        }
        entries.push(ViewEnvEntry {
            name: name.to_vec(),
            value: value.to_vec(),
            policy: Inheritance::Replace,
            left_set: true,
        });
    }
    if entries.len() > MAX_VIEW_ENV_ENTRIES {
        return Err(ViewEnvError::TooLarge);
    }

    let roots = now.get(OsStr::new(ROOTS_VAR)).map(OsString::as_os_str);
    let host_record = now.get(OsStr::new(HOST_ENV_VAR)).map(OsString::as_os_str);
    let mut out = Vec::new();
    put(&mut out, VIEW_ENV_HEADER)?;
    put(&mut out, entries.len().to_string().as_bytes())?;
    put(&mut out, b"\n")?;
    for entry in &entries {
        put(&mut out, &entry.name)?;
        put(&mut out, b"=")?;
        put(
            &mut out,
            match entry.policy {
                Inheritance::Preserve => b".",
                Inheritance::Replace => b"!",
            },
        )?;
        put(&mut out, entry.value.len().to_string().as_bytes())?;
        put(&mut out, b":")?;
        put(&mut out, &entry.value)?;
        put(&mut out, if entry.left_set { b"+" } else { b"-" })?;
        put(&mut out, b";\n")?;
    }
    for (tag, scalar) in [(b'R', roots), (b'H', host_record)] {
        let Some(bytes) = scalar else {
            continue;
        };
        let bytes = bytes.as_bytes();
        if bytes.len() > MAX_VIEW_ENV_VALUE || bytes.contains(&0) {
            continue;
        }
        put(&mut out, &[tag])?;
        put(&mut out, bytes.len().to_string().as_bytes())?;
        put(&mut out, b":")?;
        put(&mut out, bytes)?;
        put(&mut out, b";\n")?;
    }
    Ok(out)
}

/// Whether the constructor reads an entry of `name` and `value` back: a
/// nonempty name within the name cap holding neither NUL nor `=`, and a
/// value within the value cap holding no NUL.
fn fits_view_env(name: &[u8], value: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= MAX_VIEW_ENV_NAME
        && !name.contains(&0)
        && !name.contains(&b'=')
        && value.len() <= MAX_VIEW_ENV_VALUE
        && !value.contains(&0)
}

/// Append `bytes` while the whole input stays within the constructor's
/// bound. The first byte past it refuses the input whole.
fn put(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ViewEnvError> {
    if out.len() + bytes.len() > MAX_VIEW_ENV_BYTES {
        return Err(ViewEnvError::TooLarge);
    }
    out.extend_from_slice(bytes);
    Ok(())
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

/// One party's roots a runtime mapping evaluates against.
///
/// Package roots answer candidates declared against a package root, and
/// library roots are the directories the dynamic linker searches, below
/// which candidates declared against a library root are joined. The
/// application's scope is evaluated first, then the selected sets', so
/// a set fills only what the application lacks.
pub struct Scope<'a> {
    /// Tree roots of one party: the application package, or set roots.
    pub packages: &'a [&'a Path],
    /// Library directories of the same party, in search order.
    pub libraries: &'a [PathBuf],
}

/// One evaluated mapping: a single value for every alias.
pub struct PathAssignment<'a> {
    /// Environment aliases that receive [`PathAssignment::value`].
    pub variables: Vec<&'a str>,
    /// The selected paths joined the way the receiving library reads them.
    pub value: std::ffi::OsString,
    /// What happens to a value the caller already set.
    pub inheritance: Inheritance,
}

/// The roots a candidate of `base` is joined against, in search order.
fn bases<'s>(scope: &'s Scope<'_>, base: RuntimeBase) -> Vec<&'s Path> {
    match base {
        RuntimeBase::PackageRoot => scope.packages.to_vec(),
        RuntimeBase::LibraryRoot => scope.libraries.iter().map(PathBuf::as_path).collect(),
    }
}

/// Whether `path` has the kind the group declares.
fn kind_matches(kind: ResourceKind, path: &Path) -> bool {
    match kind {
        ResourceKind::File => path.is_file(),
        ResourceKind::Directory => path.is_dir(),
    }
}

/// The one path a first-match group selects, scopes in order.
///
/// Candidates evaluate in declaration order, each against its base's
/// roots in scope order, so the application's match outranks a set's
/// even when the set's candidate is declared first.
fn first_match(group: &RuntimePathGroup, scopes: &[Scope<'_>]) -> Vec<PathBuf> {
    for scope in scopes {
        for candidate in &group.candidates {
            for base in bases(scope, candidate.base) {
                let path = base.join(candidate.path.as_str());
                if kind_matches(group.kind, &path) {
                    return vec![path];
                }
            }
        }
    }
    Vec::new()
}

/// Every path an all-match group selects, without duplicates.
///
/// The application's matches all come before the sets', and within a
/// scope the candidates keep their declaration order.
fn all_matches(group: &RuntimePathGroup, scopes: &[Scope<'_>]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for scope in scopes {
        for candidate in &group.candidates {
            for base in bases(scope, candidate.base) {
                let path = base.join(candidate.path.as_str());
                if kind_matches(group.kind, &path) && !found.contains(&path) {
                    found.push(path);
                }
            }
        }
    }
    found
}

/// The path each library root contributes to a per-library-root group.
///
/// Every library root in scope order is answered by its first matching
/// candidate in declaration order, which is how the nested plugin
/// directory of a library root is preferred over the library root's own.
fn first_per_library_root(group: &RuntimePathGroup, scopes: &[Scope<'_>]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for scope in scopes {
        for base in scope.libraries {
            for candidate in &group.candidates {
                let path = base.join(candidate.path.as_str());
                if kind_matches(group.kind, &path) && !found.contains(&path) {
                    found.push(path);
                    break;
                }
            }
        }
    }
    found
}

/// Select and join the paths each mapping finds, in mapping order.
///
/// A mapping that finds no candidate of its declared kind assigns
/// nothing, so it can neither invent a path nor override an inherited
/// value. Probes are bounded by the codec's caps on candidates and
/// aliases times the launch's root count.
pub(crate) fn evaluate_path_groups<'a>(
    groups: impl IntoIterator<Item = &'a RuntimePathGroup>,
    scopes: &[Scope<'_>],
) -> Vec<PathAssignment<'a>> {
    let mut assignments = Vec::new();
    for group in groups {
        let chosen = match group.selection {
            PathSelection::First => first_match(group, scopes),
            PathSelection::All => all_matches(group, scopes),
            PathSelection::FirstPerLibraryRoot => first_per_library_root(group, scopes),
        };
        if chosen.is_empty() {
            continue;
        }
        let Some(value) = env::join_paths(&chosen).ok() else {
            continue;
        };
        assignments.push(PathAssignment {
            variables: group
                .variables
                .iter()
                .map(EnvironmentName::as_str)
                .collect(),
            value,
            inheritance: group.inheritance,
        });
    }
    assignments
}

/// Apply assignments to the process environment.
///
/// A preserve policy keeps a present caller value, including an
/// explicitly empty one; a replace policy overrides it. All aliases of
/// one group follow the group's policy.
pub(crate) fn apply_path_assignments(assignments: &[PathAssignment<'_>]) {
    for assignment in assignments {
        for name in &assignment.variables {
            if assignment.inheritance == Inheritance::Preserve && env::var_os(name).is_some() {
                continue;
            }
            set_path(name, &assignment.value);
        }
    }
}

/// The built-in catalog's runtime path groups, in catalog order.
///
/// These carry the framework knowledge for packages without runtime rule
/// metadata; decoded package groups evaluate through the same engine.
pub(crate) fn builtin_path_groups() -> impl Iterator<Item = &'static RuntimePathGroup> {
    onelf_format::catalog::builtins()
        .iter()
        .flat_map(|descriptor| descriptor.runtime.paths.iter())
}

/// Point the libraries that load plugins or read data from a compiled-in
/// directory at the copies the package, its active prepared subtrees and
/// its sets carry, by evaluating the launch's runtime mappings. The
/// version-dependent Tcl and Tk script libraries, the carried ALSA
/// snippet list, the GDK and GTK module caches, GIO's module directories
/// and fontconfig stay with the registered runtime hooks.
fn setup_plugin_dirs(
    active: &ActiveRules<'_>,
    pkg: &Path,
    lib_dirs: &[PathBuf],
    set_lib_dirs: &[PathBuf],
    set_roots: &[PathBuf],
) {
    apply_path_assignments(&active.assignments(pkg, lib_dirs, set_lib_dirs, set_roots));
}

/// The runtime rule record this process's launch carries, installed once
/// by the launch gate or by a development launch before any environment
/// is applied. Absent, a launch evaluates the built-in catalog.
static CARRIED_RULES: OnceLock<RuntimeRuleMetadata> = OnceLock::new();

/// Install the decoded runtime rule record the package carries, so every
/// later environment evaluation in this process uses its groups and hooks
/// in place of the built-in fallback. Only the first installation counts.
///
/// The record's variable names are classified like the built-in ones:
/// a value still pointing into another package is dropped before the
/// launch applies anything.
pub fn carry_rules(rules: RuntimeRuleMetadata) {
    crate::hostenv::purge_carried(&rules);
    let _ = CARRIED_RULES.set(rules);
}

/// One runtime path group a launch evaluates, with the prepared subtrees
/// of the active pins whose bound records carry it.
struct ActiveGroup<'a> {
    group: &'a RuntimePathGroup,
    prepared: Vec<PathBuf>,
}

/// The runtime groups and hooks one launch evaluates.
///
/// Without a carried record these are the built-in catalog's groups and
/// every registered hook. With one, they are exactly the carried groups
/// and hook IDs: each unbound record contributes its groups, and a record
/// bound to a pin contributes only while that pin is active, adding the
/// pin's prepared subtree to the same rule's group of the same identity.
/// Records arrive in canonical order, so a group's subtrees keep the
/// canonical pin order.
pub(crate) struct ActiveRules<'a> {
    groups: Vec<ActiveGroup<'a>>,
    hooks: Vec<RuntimeHookId>,
    prepared: Vec<PathBuf>,
}

/// The rules this process's launch evaluates over the package at `pkg`
/// and the pins it obtained.
fn launch_rules(pkg: &Path) -> ActiveRules<'static> {
    ActiveRules::new(CARRIED_RULES.get(), pkg, &crate::sets::pins())
}

impl<'a> ActiveRules<'a> {
    /// Resolve `carried`, or the built-ins when it is `None`, against the
    /// package root and the active pins.
    pub(crate) fn new(
        carried: Option<&'a RuntimeRuleMetadata>,
        pkg: &Path,
        pins: &[ActivePin],
    ) -> Self {
        let Some(carried) = carried else {
            return Self {
                groups: builtin_path_groups()
                    .map(|group| ActiveGroup {
                        group,
                        prepared: Vec::new(),
                    })
                    .collect(),
                hooks: all_runtime_hooks(),
                prepared: Vec::new(),
            };
        };
        let mut keys: Vec<(&str, &str)> = Vec::new();
        let mut active = Self {
            groups: Vec::new(),
            hooks: Vec::new(),
            prepared: Vec::new(),
        };
        for record in &carried.rules {
            let root = match &record.binding {
                None => None,
                Some(pin) => {
                    let hex: String = pin.hash.iter().map(|b| format!("{b:02x}")).collect();
                    if !pins
                        .iter()
                        .any(|p| p.label == pin.name.as_str() && p.hash.eq_ignore_ascii_case(&hex))
                    {
                        continue;
                    }
                    Some(pkg.join(pin.prepared_dir()))
                }
            };
            for hook in &record.runtime.hooks {
                if !active.hooks.contains(hook) {
                    active.hooks.push(*hook);
                }
            }
            if let Some(root) = &root
                && !active.prepared.contains(root)
            {
                active.prepared.push(root.clone());
            }
            for group in &record.runtime.paths {
                let key = (record.id.as_str(), group.id.as_str());
                match keys.iter().position(|k| *k == key) {
                    Some(at) => active.groups[at].prepared.extend(root.clone()),
                    None => {
                        keys.push(key);
                        active.groups.push(ActiveGroup {
                            group,
                            prepared: root.clone().into_iter().collect(),
                        });
                    }
                }
            }
        }
        active.prepared.sort();
        active
    }

    /// Evaluate every group in order against the application first, then
    /// the group's active prepared subtrees, then the selected sets. A
    /// prepared subtree answers package-root candidates at its own root
    /// and library-root candidates at the application's library
    /// directories inside it.
    fn assignments(
        &self,
        pkg: &Path,
        lib_dirs: &[PathBuf],
        set_lib_dirs: &[PathBuf],
        set_roots: &[PathBuf],
    ) -> Vec<PathAssignment<'a>> {
        let set_packages: Vec<&Path> = set_roots.iter().map(PathBuf::as_path).collect();
        let mut out = Vec::new();
        for active in &self.groups {
            let prepared: Vec<&Path> = active.prepared.iter().map(PathBuf::as_path).collect();
            let prepared_libs = prepared_lib_dirs(pkg, lib_dirs, &active.prepared);
            let scopes = [
                Scope {
                    packages: std::slice::from_ref(&pkg),
                    libraries: lib_dirs,
                },
                Scope {
                    packages: &prepared,
                    libraries: &prepared_libs,
                },
                Scope {
                    packages: &set_packages,
                    libraries: set_lib_dirs,
                },
            ];
            out.extend(evaluate_path_groups([active.group], &scopes));
        }
        out
    }

    /// The set library directories and roots the runtime hooks search:
    /// every active prepared subtree before the raw set roots.
    fn with_prepared(
        &self,
        pkg: &Path,
        lib_dirs: &[PathBuf],
        set_lib_dirs: &[PathBuf],
        set_roots: &[PathBuf],
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut libs = prepared_lib_dirs(pkg, lib_dirs, &self.prepared);
        libs.extend_from_slice(set_lib_dirs);
        let mut roots = self.prepared.clone();
        roots.extend_from_slice(set_roots);
        (libs, roots)
    }
}

/// The application's library directories `lib_dirs`, rebased from `pkg`
/// into each prepared subtree in `roots`, in subtree order.
fn prepared_lib_dirs(pkg: &Path, lib_dirs: &[PathBuf], roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .flat_map(|root| {
            lib_dirs
                .iter()
                .filter_map(move |dir| dir.strip_prefix(pkg).ok().map(|rel| root.join(rel)))
        })
        .collect()
}

/// Set `TCL_LIBRARY` and `TK_LIBRARY` to the script libraries matching
/// the bundled Tcl and Tk, replacing any inherited value.
fn run_tcl_tk_libraries(context: &HookContext<'_>) {
    let mut libraries: Vec<PathBuf> = context.lib_dirs.to_vec();
    libraries.extend_from_slice(context.set_lib_dirs);
    let roots: Vec<&Path> = std::iter::once(context.pkg)
        .chain(context.set_roots.iter().map(PathBuf::as_path))
        .collect();
    let (tcl, tk) = tcl_tk_libraries(&libraries, &roots);
    if let Some(dir) = tcl {
        set_path("TCL_LIBRARY", dir.as_os_str());
    }
    if let Some(dir) = tk {
        set_path("TK_LIBRARY", dir.as_os_str());
    }
}

/// Set `ALSA_CONFIG_PATH` to the carried configuration and its naturally
/// ordered snippets, keeping a caller's own value.
fn run_alsa_config_path_natural_sort(context: &HookContext<'_>) {
    let roots: Vec<&Path> = std::iter::once(context.pkg)
        .chain(context.set_roots.iter().map(PathBuf::as_path))
        .collect();
    set_alsa_config_path(&roots);
}

/// ALSA reads its top-level config from `ALSA_CONFIG_PATH` when set, and
/// `alsa.conf`'s own `@hooks` then load the host's `/etc/alsa/conf.d` by
/// absolute path after everything this list names, so the host's
/// snippets keep the last word unless the carried `alsa.conf` was
/// patched to load the carried directory too (the bundler's
/// `patch_alsa_conf_hooks`). The list is a fallback for alsa-lib builds
/// whose `alsa.conf` has no conf.d hook at all, which would otherwise
/// never see the carried snippets. A caller-set value is kept. The list
/// is not set at all when a path holds a space: alsa-lib splits this
/// variable on spaces as well as colons, and a broken list loses the
/// carried `alsa.conf` itself.
fn set_alsa_config_path(roots: &[&Path]) {
    if env::var_os("ALSA_CONFIG_PATH").is_some() {
        return;
    }
    for root in roots {
        let alsa = root.join("share/alsa");
        let conf = alsa.join("alsa.conf");
        if !conf.is_file() {
            continue;
        }
        let mut list = vec![conf];
        if let Ok(entries) = std::fs::read_dir(alsa.join("alsa.conf.d")) {
            // Only `*.conf`, as alsa-lib's own directory load does: any
            // other file in the list makes the whole config update fail.
            // The order is alsa-lib's numeric one, where `99-x.conf`
            // loads after `100-y.conf`, not bytewise order.
            let mut confs: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "conf"))
                .collect();
            confs.sort_by(|a, b| {
                natural_cmp(
                    &a.file_name().unwrap_or_default().to_string_lossy(),
                    &b.file_name().unwrap_or_default().to_string_lossy(),
                )
            });
            list.extend(confs);
        }
        let representable = list
            .iter()
            .all(|path| !path.to_string_lossy().contains([' ', '\t']));
        if representable
            && let Ok(joined) = env::join_paths(list.iter().map(|path| path.as_os_str()))
        {
            set_path("ALSA_CONFIG_PATH", &joined);
        }
        return;
    }
}

/// File-name order for the carried snippet list: runs of digits compare
/// as numbers, so `99-x.conf` loads after `100-y.conf`, the opposite of
/// bytewise order. This matches alsa-lib's versionsort for the names
/// distros actually ship, which pad consistently. It differs from glibc's
/// strverscmp when runs of equal value carry different zero padding or a
/// padded run meets an unpadded one (`09-x.conf` against `0-y.conf`);
/// no naming scheme produces those, and any deterministic order does.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (ai, bj) = (i, j);
            i = digit_run(a, i);
            j = digit_run(b, j);
            let av = &a[ai..i];
            let bv = &b[bj..j];
            let zeros_a = av.iter().take_while(|&&d| d == b'0').count();
            let zeros_b = bv.iter().take_while(|&&d| d == b'0').count();
            let ord = av[zeros_a..]
                .len()
                .cmp(&bv[zeros_b..].len())
                .then_with(|| av[zeros_a..].cmp(&bv[zeros_b..]));
            if ord != Ordering::Equal {
                return ord;
            }
        } else if a[i] == b[j] {
            i += 1;
            j += 1;
        } else {
            return a[i].cmp(&b[j]);
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

/// The end of the run of ASCII digits starting at `at`.
fn digit_run(bytes: &[u8], at: usize) -> usize {
    bytes[at..]
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(bytes.len(), |off| at + off)
}

/// The script libraries of a bundled Tcl and Tk, `tcl8.6/` holding
/// `init.tcl` and `tk8.6/` holding `tk.tcl`, below a library directory
/// or under Debian's `share/tcltk/`. The scripts must match the bundled
/// interpreter's version, so these replace an inherited value.
fn tcl_tk_libraries(lib_dirs: &[PathBuf], roots: &[&Path]) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut places: Vec<PathBuf> = lib_dirs.to_vec();
    places.extend(roots.iter().map(|root| root.join("share/tcltk")));
    let find = |prefix: &str, script: &str| {
        places.iter().find_map(|place| {
            let mut names: Vec<PathBuf> = std::fs::read_dir(place)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.strip_prefix(prefix)
                        .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
                })
                .map(|e| e.path())
                .filter(|dir| dir.join(script).is_file())
                .collect();
            names.sort();
            names.pop()
        })
    };
    (find("tcl", "init.tcl"), find("tk", "tk.tcl"))
}

/// Leave the Vulkan loader one ICD file per driver.
///
/// The loader finds the package's ICD files on its own: the package's
/// `share/` is on `XDG_DATA_DIRS`, which it searches after the host's
/// `/etc/vulkan/icd.d`. A host loader, the one the resolver took when the
/// host's `libvulkan` won or the package carries none, is left to that
/// search, which also covers `/usr/local` and whatever else the host
/// configured. The search lists a driver twice when the package and the
/// host both describe it, though, and the loader then opens it twice and
/// shows every device twice. Only then is `VK_DRIVER_FILES` set, to the
/// search's own files less the copy the resolver did not choose.
///
/// A bundled loader was built for another system and may search other
/// directories than this host uses, so it always gets the list.
///
/// A caller's `VK_DRIVER_FILES`, `VK_ICD_FILENAMES` or
/// `VK_ADD_DRIVER_FILES` leaves the choice to the caller.
fn setup_vulkan(
    pkg: &Path,
    platform_root: Option<&Path>,
    set_roots: &[PathBuf],
    lib_dirs: &[PathBuf],
    farm: Option<&Path>,
) {
    if ["VK_DRIVER_FILES", "VK_ICD_FILENAMES", "VK_ADD_DRIVER_FILES"]
        .iter()
        .any(|var| env::var_os(var).is_some())
    {
        return;
    }
    let packaged: Vec<PathBuf> = std::iter::once(pkg)
        .chain(set_roots.iter().map(PathBuf::as_path))
        .map(|root| root.join(VK_ICD_DIR))
        .collect();
    let outside: Vec<PathBuf> = platform_root
        .map(|root| root.join(VK_ICD_DIR))
        .into_iter()
        .chain(loader_search_dirs(|var| env::var_os(var)))
        .collect();
    let in_farm = |name: &str| farm.is_some_and(|f| f.join(name).symlink_metadata().is_ok());
    let carried = |name: &str| lib_dirs.iter().any(|d| d.join(name).exists());
    let host_loader = in_farm(VK_LOADER) || !carried(VK_LOADER);
    let chosen = |driver: &str| !in_farm(driver) && carried(driver);
    if let Some(files) = vulkan_driver_files(&packaged, &outside, host_loader, chosen)
        && let Ok(joined) = env::join_paths(files)
    {
        set_path("VK_DRIVER_FILES", &joined);
    }
}

const VK_ICD_DIR: &str = "share/vulkan/icd.d";
const VK_LOADER: &str = "libvulkan.so.1";

/// The ICD directories the Vulkan loader searches on Linux, in its order,
/// with `var` reading the environment. `/usr/share` and NixOS's
/// `/run/opengl-driver/share` follow, for a loader built elsewhere that
/// knows neither.
fn loader_search_dirs(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let list = |name: &str, default: &str| -> Vec<PathBuf> {
        let value = var(name).filter(|v| !v.is_empty());
        let value = value.unwrap_or_else(|| default.into());
        env::split_paths(&value)
            .filter(|p| p.is_absolute())
            .collect()
    };
    let home = var("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let user = |name: &str, below_home: &str| {
        var(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(below_home)))
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.extend(user("XDG_CONFIG_HOME", ".config"));
    dirs.extend(list("XDG_CONFIG_DIRS", "/etc/xdg"));
    dirs.push(PathBuf::from("/etc"));
    dirs.extend(user("XDG_DATA_HOME", ".local/share"));
    dirs.extend(list("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
    dirs.push(PathBuf::from("/usr/share"));
    dirs.push(PathBuf::from("/run/opengl-driver/share"));
    dirs.into_iter().map(|d| d.join("vulkan/icd.d")).collect()
}

/// The ICD files to name in `VK_DRIVER_FILES`, or `None` when the loader
/// is better left to its own search.
///
/// `packaged` are the package's and its sets' ICD directories, `outside`
/// the rest in the loader's order. A file from the package and one from
/// outside it that name the same driver library describe one driver:
/// the package's file stays when `chosen` says the package's copy of that
/// driver loads, and the outside ones otherwise. Files on the same side
/// are never merged, since a host describes its 32-bit and 64-bit builds
/// of a driver under one file name.
fn vulkan_driver_files(
    packaged: &[PathBuf],
    outside: &[PathBuf],
    host_loader: bool,
    chosen: impl Fn(&str) -> bool,
) -> Option<Vec<PathBuf>> {
    let mut seen_dirs: Vec<PathBuf> = Vec::new();
    let mut icd_files = |dirs: &[PathBuf]| -> Vec<(PathBuf, Option<String>)> {
        let mut out = Vec::new();
        for dir in dirs {
            if seen_dirs.contains(dir) {
                continue;
            }
            seen_dirs.push(dir.clone());
            let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            files.sort();
            out.extend(files.into_iter().map(|path| {
                let driver = std::fs::read_to_string(&path).ok().and_then(|text| {
                    let lib = onelf_format::resolve::json_library_path(&text)?;
                    Some(Path::new(lib).file_name()?.to_string_lossy().into_owned())
                });
                (path, driver)
            }));
        }
        out
    };
    let mut ours = icd_files(packaged);
    let theirs = icd_files(outside);

    let mut kept_theirs = Vec::new();
    let mut dropped = false;
    for (path, driver) in theirs {
        let Some(driver) = driver.filter(|d| ours.iter().any(|(_, o)| o.as_ref() == Some(d)))
        else {
            kept_theirs.push(path);
            continue;
        };
        dropped = true;
        if !chosen(&driver) {
            ours.retain(|(_, o)| o.as_ref() != Some(&driver));
            kept_theirs.push(path);
        }
    }
    let files: Vec<PathBuf> = ours
        .into_iter()
        .map(|(path, _)| path)
        .chain(kept_theirs)
        .collect();
    if files.is_empty() || (host_loader && !dropped) {
        return None;
    }
    Some(files)
}

/// Where glibc looks for locales unless `LOCPATH` says otherwise: the
/// `locale-archive` file there first, then a directory per locale.
const HOST_LOCALE_DIR: &str = "/usr/lib/locale";

/// Let a bundled glibc find the `C.UTF-8` locale the package carries on a
/// host whose locale data it cannot read.
///
/// glibc searches `LOCPATH` ahead of its compiled-in directory and stops
/// reading the archive altogether, so it is set only where there is no
/// archive to lose: a musl host, a minimal container. The host's
/// per-locale directories are still searched after the package's, and a
/// caller's own value is kept.
fn setup_locale(lib_dirs: &[PathBuf]) {
    if env::var_os("LOCPATH").is_some() {
        return;
    }
    if let Some(value) = locale_path(lib_dirs, Path::new(HOST_LOCALE_DIR)) {
        set_path("LOCPATH", &value);
    }
}

/// The `LOCPATH` for a package with `lib_dirs` on a host keeping its
/// locales in `host`, or `None` when the host's archive serves.
fn locale_path(lib_dirs: &[PathBuf], host: &Path) -> Option<std::ffi::OsString> {
    if host.join("locale-archive").is_file() {
        return None;
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in lib_dirs.iter().map(|d| d.join("locale")) {
        let carried = ["C.utf8", "C.UTF-8"].iter().any(|n| dir.join(n).is_dir());
        if carried && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    (!dirs.is_empty())
        .then(|| env::join_paths(dirs).ok())
        .flatten()
}

/// Where distributions keep the CA bundle, relative to the root.
const CA_BUNDLES: &[&str] = &[
    "etc/ssl/certs/ca-certificates.crt",
    "etc/pki/tls/certs/ca-bundle.crt",
    "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "etc/ca-certificates/extracted/tls-ca-bundle.pem",
    "etc/ssl/ca-bundle.pem",
    "etc/pki/tls/cacert.pem",
    "etc/ssl/cert.pem",
    "var/lib/ca-certificates/ca-bundle.pem",
];

/// Name the package's CA bundle when the host has none.
///
/// A bundled TLS library looks for the bundle where its packaging
/// distribution keeps it, and a host that keeps it elsewhere or not at
/// all, a minimal container for one, leaves every connection
/// unverifiable. A host bundle anywhere usual is left to the library, and
/// a caller's `SSL_CERT_FILE` or `CURL_CA_BUNDLE` stops this altogether,
/// since either names the trust the user chose.
fn setup_ca_bundle(pkg: &Path, set_roots: &[PathBuf]) {
    if env::var_os("SSL_CERT_FILE").is_some() || env::var_os("CURL_CA_BUNDLE").is_some() {
        return;
    }
    let roots: Vec<&Path> = std::iter::once(pkg)
        .chain(set_roots.iter().map(PathBuf::as_path))
        .collect();
    if let Some(bundle) = fallback_ca_bundle(Path::new("/"), &roots) {
        set_path("SSL_CERT_FILE", bundle.as_os_str());
        set_path("CURL_CA_BUNDLE", bundle.as_os_str());
    }
}

/// The first CA bundle `roots` carry, when `host` has none of its own.
fn fallback_ca_bundle(host: &Path, roots: &[&Path]) -> Option<PathBuf> {
    if CA_BUNDLES.iter().any(|rel| host.join(rel).is_file()) {
        return None;
    }
    roots
        .iter()
        .flat_map(|root| CA_BUNDLES.iter().map(|rel| root.join(rel)))
        .find(|p| p.is_file())
}

/// Terminal descriptions searched ahead of the package's when the caller
/// names none, so a terminal the host knows keeps the host's entry.
const HOST_TERMINFO: &[&str] = &[
    "/etc/terminfo",
    "/lib/terminfo",
    "/usr/share/terminfo",
    "/usr/lib/terminfo",
];

/// Add the package's terminfo after the host's, for a host that has none.
///
/// ncurses searches `TERMINFO_DIRS` before its compiled-in directory,
/// which belongs to the packaging distribution and may not be where this
/// host keeps its descriptions. The caller's own list, or the host's
/// usual directories, stay first, so `TERMINFO` and `~/.terminfo` keep
/// working and the package only fills in what the host lacks.
fn setup_terminfo(pkg: &Path, set_roots: &[PathBuf]) {
    let bundled: Vec<PathBuf> = std::iter::once(pkg)
        .chain(set_roots.iter().map(PathBuf::as_path))
        .flat_map(|root| ["share/terminfo", "lib/terminfo"].map(|d| root.join(d)))
        .filter(|d| d.is_dir())
        .collect();
    if bundled.is_empty() {
        return;
    }
    let mut dirs: Vec<PathBuf> = match env::var_os("TERMINFO_DIRS").filter(|v| !v.is_empty()) {
        Some(value) => env::split_paths(&value).collect(),
        None => HOST_TERMINFO
            .iter()
            .map(PathBuf::from)
            .filter(|d| d.is_dir())
            .collect(),
    };
    for dir in bundled {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if let Ok(joined) = env::join_paths(dirs) {
        set_path("TERMINFO_DIRS", &joined);
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
mod library_search_tests {
    use super::test_env_lock::ENV_LOCK;
    use super::*;

    #[test]
    fn the_search_path_orders_farm_package_and_sets() {
        let path = library_search_path(
            Path::new("/pkg"),
            "lib:lib/x86_64-linux-gnu",
            Some(Path::new("/farm")),
            &[PathBuf::from("/set/lib")],
        );
        assert_eq!(path, "/farm:/pkg/lib:/pkg/lib/x86_64-linux-gnu:/set/lib");
    }

    #[test]
    fn no_package_libraries_name_no_search_path() {
        assert_eq!(library_search_path(Path::new("/pkg"), "", None, &[]), "");
    }

    /// A caller's inherited `LD_LIBRARY_PATH` never joins the search
    /// path.
    #[test]
    fn a_callers_library_path_never_joins_the_search() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { env::set_var("LD_LIBRARY_PATH", "/caller/priv:/usr/lib") };

        let path = library_search_path(Path::new("/pkg"), "lib", None, &[]);

        assert_eq!(path, "/pkg/lib");
        unsafe { env::remove_var("LD_LIBRARY_PATH") };
    }

    /// The launch returns the shared search path for an ELF target, with
    /// the caller's inherited value absent from it and from the
    /// application's environment.
    #[test]
    fn the_launch_returns_the_shared_path_without_the_callers_value() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = env::temp_dir().join(format!("onelf-libpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let elf = dir.join("binary");
        std::fs::write(&elf, b"\x7fELF").unwrap();
        unsafe { env::set_var("LD_LIBRARY_PATH", "/caller/priv") };

        let path = setup_env(
            "/pkg",
            "app",
            elf.to_str().unwrap(),
            "ep",
            "test",
            "lib",
            elf.to_str().unwrap(),
            Some(Path::new("/farm")),
            None,
            &[PathBuf::from("/set/lib")],
            &[],
        );

        assert_eq!(path, "/farm:/pkg/lib:/set/lib");
        assert!(env::var_os("LD_LIBRARY_PATH").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The loader search variables are refused configuration names, and
    /// the rule codec refuses exactly the same names, so a recipe entry
    /// and a rule cannot slip past one gate what the other refuses.
    #[test]
    fn loader_search_variables_are_refused_configuration_names() {
        for name in [
            "LD_LIBRARY_PATH",
            "LD_LIBRARY_PATH_32",
            "LD_LIBRARY_PATH_64",
            "LD_AOUT_LIBRARY_PATH",
        ] {
            assert!(library_search_override(name).is_some(), "{name}");
            assert!(EnvironmentName::new(name).is_err(), "{name}");
        }
        for name in ["PATH", "HOME", "QT_PLUGIN_PATH", "FONTCONFIG_FILE"] {
            assert!(library_search_override(name).is_none(), "{name}");
            assert!(EnvironmentName::new(name).is_ok(), "{name}");
        }
    }
}

#[cfg(test)]
mod store_hint_tests {
    use std::path::PathBuf;

    use onelf_format::{Block, Entry, EntryKind, Flags, Footer, Manifest, ManifestHeader};

    use super::test_env_lock::ENV_LOCK;
    use super::*;

    const HINT: &str = crate::platform::STORE_HINT_VAR;
    const NO_FETCH: &str = "ONELF_NO_PLATFORM_FETCH";

    /// The one file a test container carries, and its stored bytes.
    const FILE_NAME: &str = "lib/libdemo.so";
    const FILE_CONTENT: &[u8] = b"demo store hint bytes\n";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("onelf-store-hint-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An ordinary launch whose tree carries a `lib` directory.
    fn call() {
        setup_env(
            "/pkg",
            "app",
            "/exec",
            "ep",
            "test",
            "lib",
            "/nonexistent/target",
            None,
            None,
            &[],
            &[],
        );
    }

    /// The store the export resolves to for this process's uid, over the
    /// hint the seam inherits: the decision acquisition itself makes.
    fn resolved(hint: &Path) -> PathBuf {
        crate::platform::owned_store(
            crate::paths::uid(),
            Some(hint),
            crate::cache::base_dir,
            crate::paths::private_dir,
        )
        .expect("the uid's store resolves")
    }

    /// Where the store keeps the build `digest` names, under label `set`.
    fn stored_in(store: &Path, digest: &str) -> PathBuf {
        store.join("set").join(format!("{digest}.onelf"))
    }

    /// A minimal stored-mode container the runtime loader can open,
    /// together with the digest that names it in a store. The file sits
    /// at `lib/libdemo.so` as the entry tree's second node.
    fn container() -> (Vec<u8>, String) {
        let prefix = b"onelf store hint fixture";
        let mut string_table: Vec<u8> = vec![0];
        let dir_name_offset = string_table.len() as u32;
        string_table.extend_from_slice(b"lib");
        string_table.push(0);
        let name_offset = string_table.len() as u32;
        string_table.extend_from_slice(b"libdemo.so");
        string_table.push(0);

        let hash = *blake3::hash(FILE_CONTENT).as_bytes();
        let dir = Entry {
            kind: EntryKind::Dir,
            parent: u32::MAX,
            name: dir_name_offset,
            mode: 0o755,
            mtime_secs: 0,
            mtime_nsec: 0,
            content_hash: [0; 32],
            blocks: Vec::new(),
            symlink_target: 0,
        };
        let file = Entry {
            kind: EntryKind::File,
            parent: 0,
            name: name_offset,
            mode: 0o755,
            mtime_secs: 0,
            mtime_nsec: 0,
            content_hash: hash,
            blocks: vec![Block {
                payload_offset: 0,
                compressed_size: FILE_CONTENT.len() as u64,
                original_size: FILE_CONTENT.len() as u64,
                content_hash: hash,
            }],
            symlink_target: 0,
        };
        let manifest = Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: 2,
                string_table_size: string_table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: *blake3::hash(prefix).as_bytes(),
                footer_hash: [0; 32],
            },
            entrypoints: Vec::new(),
            entries: vec![dir, file],
            lib_dir_offsets: Vec::new(),
            string_table,
        };
        let raw = manifest.serialize().unwrap();
        let compressed = zstd::bulk::compress(&raw, 1).unwrap();
        let manifest_offset = prefix.len() as u64;
        let payload_offset = manifest_offset + compressed.len() as u64;
        let footer = Footer {
            format_version: onelf_format::footer::FORMAT_VERSION,
            flags: Flags::STORED,
            manifest_offset,
            manifest_compressed: compressed.len() as u64,
            manifest_original: raw.len() as u64,
            payload_offset,
            payload_size: FILE_CONTENT.len() as u64,
            dict_offset: 0,
            dict_size: 0,
            manifest_checksum: onelf_format::manifest::manifest_checksum(&raw).unwrap(),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(FILE_CONTENT);
        footer.write_to(&mut bytes).unwrap();
        let digest = blake3::hash(&bytes).to_hex().to_string();
        (bytes, digest)
    }

    /// The verified payload of [`FILE_NAME`] as the loader reads it back.
    /// The build `store_build` serves, extracted through the verified
    /// handle into the ordinary cache.
    fn extracted_content(artifact: &crate::platform::VerifiedArtifact) -> Vec<u8> {
        let (root, _lock) = crate::platform::extract_build(artifact).expect("the build extracts");
        std::fs::read(root.join(FILE_NAME)).expect("the extracted file is present")
    }

    /// The store the launch exports, by privilege: an ordinary uid keeps
    /// the hint it inherited, a privileged uid its own private base.
    fn expected_export(hint: &Path) -> String {
        if crate::paths::uid() == 0 {
            crate::paths::private_dir()
                .unwrap()
                .join("platform")
                .to_string_lossy()
                .into_owned()
        } else {
            hint.to_string_lossy().into_owned()
        }
    }

    /// An explicit relocation passes through for an ordinary uid; a
    /// privileged launch exports its own private base in its place, so
    /// no inherited user-chosen path survives a UID transition.
    #[test]
    fn an_explicit_hint_is_exported_as_the_uid_resolves_it() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("explicit");
        let hint = dir.join("store");
        unsafe { env::set_var(HINT, &hint) };

        call();

        assert_eq!(
            env::var(HINT).unwrap(),
            expected_export(&hint),
            "the export is the resolved store, not the raw inheritance"
        );

        unsafe { env::remove_var(HINT) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without an explicit store, the launch exports the resolution
    /// acquisition would use, so children of a default launch see the
    /// store too and share its builds.
    #[test]
    fn the_default_resolution_is_exported_without_an_inherited_hint() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { env::remove_var(HINT) };

        call();

        let got = env::var(HINT).unwrap();
        if crate::paths::uid() == 0 {
            assert_eq!(got, expected_export(&PathBuf::from("/unused")));
        } else {
            let base = onelf_format::cache_layout::resolve_root(
                crate::paths::uid(),
                crate::paths::private_dir(),
            )
            .expect("the test cache base resolves");
            assert_eq!(got, base.join("platform").to_string_lossy());
        }
    }

    /// A launch that never acquires must leave the persistent cache
    /// untouched: the export names the location the environment proposes
    /// without creating it, and acquisition creates the root only when a
    /// build is actually stored.
    #[test]
    fn the_export_never_establishes_the_persistent_cache() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        if crate::paths::uid() == 0 {
            // Root resolves its private base, not the persistent cache.
            return;
        }
        let home = temp_dir("nocache");
        unsafe { env::set_var("HOME", &home) };
        unsafe { env::remove_var("XDG_CACHE_HOME") };
        unsafe { env::remove_var(HINT) };
        let root = home.join(".cache").join("onelf");
        let _ = std::fs::remove_dir_all(&root);

        call();

        let got = env::var(HINT).unwrap();
        assert_eq!(got, root.join("platform").to_string_lossy());
        assert!(!root.exists(), "the export created the persistent cache");
    }

    /// A preserved escalation environment and a stripped one produce the
    /// same export: the decision keys on the real uid, never on the
    /// variables a privilege transition may or may not keep.
    #[test]
    fn preserved_and_stripped_escalation_environments_export_identically() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("sudo");
        let hint = dir.join("store");
        let escalation = [
            ("SUDO_USER", "caller"),
            ("SUDO_UID", "1000"),
            ("SUDO_GID", "1000"),
            ("SUDO_COMMAND", "/usr/bin/onelf run app"),
            ("PKEXEC_UID", "1000"),
        ];
        for (name, value) in escalation {
            unsafe { env::set_var(name, value) };
        }
        unsafe { env::set_var(HINT, &hint) };
        call();
        let preserved = env::var(HINT).unwrap();

        for (name, _) in escalation {
            unsafe { env::remove_var(name) };
        }
        call();
        let stripped = env::var(HINT).unwrap();

        assert_eq!(preserved, stripped);
        assert_eq!(stripped, expected_export(&hint));

        unsafe { env::remove_var(HINT) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exported location is where acquisition looks, and only
    /// verification decides what may be served from it: a corrupted
    /// artifact at the exported name is refused on its digest, the
    /// loader never sees the bytes, and the store is left as it was.
    #[test]
    fn a_corrupted_artifact_at_the_exported_store_is_rejected() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("corrupt");
        let hint = dir.join("store");
        let (bytes, digest) = container();
        let store = resolved(&hint);
        let stored = stored_in(&store, &digest);
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let mut damaged = bytes.clone();
        damaged[5] ^= 0xff;
        std::fs::write(&stored, &damaged).unwrap();

        unsafe {
            env::set_var(HINT, &hint);
            env::set_var(NO_FETCH, "1");
        }
        let pin = crate::platform::Pin {
            label: "set".into(),
            url: format!("file://{}", dir.join("absent-source").display()),
            blake3: digest,
        };
        let err = crate::platform::store_build(&pin).unwrap_err();

        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !err.contains("not a usable build"),
            "the loader must not run on unverified bytes: {err}"
        );
        assert_eq!(
            std::fs::read(&stored).unwrap(),
            damaged,
            "the exported store is left alone"
        );

        unsafe {
            env::remove_var(HINT);
            env::remove_var(NO_FETCH);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fetch prohibition still permits a safe cached import: the
    /// artifact already at the exported location verifies against the
    /// pin and is served without any fetch, though the configured source
    /// could not have served the build at all.
    #[test]
    fn fetch_prohibition_still_imports_a_verified_cached_artifact() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("cached-import");
        let hint = dir.join("store");
        let (bytes, digest) = container();
        let store = resolved(&hint);
        let stored = stored_in(&store, &digest);
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, &bytes).unwrap();

        unsafe {
            env::set_var(HINT, &hint);
            env::set_var(NO_FETCH, "1");
        }
        let pin = crate::platform::Pin {
            label: "set".into(),
            url: format!("file://{}", dir.join("absent-source").display()),
            blake3: digest,
        };
        let artifact = crate::platform::store_build(&pin).expect("the cached import is allowed");

        assert_eq!(artifact.path(), stored.as_path());
        assert_eq!(extracted_content(&artifact), FILE_CONTENT);

        unsafe {
            env::remove_var(HINT);
            env::remove_var(NO_FETCH);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With the build missing from the exported location, fetch
    /// prohibition stops the source fetch and names itself, leaving the
    /// store without the build.
    #[test]
    fn a_missing_artifact_with_fetch_disabled_names_the_prohibition() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("prohibited");
        let hint = dir.join("store");
        let source = dir.join("source");
        std::fs::write(&source, b"bytes no pin of this test names").unwrap();
        let store = resolved(&hint);

        unsafe {
            env::set_var(HINT, &hint);
            env::set_var(NO_FETCH, "1");
        }
        let pin = crate::platform::Pin {
            label: "set".into(),
            url: format!("file://{}", source.display()),
            blake3: "0".repeat(64),
        };
        let err = crate::platform::store_build(&pin).unwrap_err();

        assert!(err.contains("fetching is disabled"), "{err}");
        assert!(err.contains(NO_FETCH), "{err}");
        assert!(!stored_in(&store, &pin.blake3).exists());

        unsafe {
            env::remove_var(HINT);
            env::remove_var(NO_FETCH);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With fetching permitted but the source unreachable, the
    /// acquisition through the exported store fails with the source's
    /// reason and publishes nothing: the error every required set exits
    /// with and every optional GL build warns with, unchanged.
    #[test]
    fn an_unreachable_source_fails_acquisition_through_the_exported_store() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("unreachable");
        let hint = dir.join("store");
        let store = resolved(&hint);
        let stored = stored_in(&store, &"0".repeat(64));

        unsafe {
            env::set_var(HINT, &hint);
            env::remove_var(NO_FETCH);
        }
        let pin = crate::platform::Pin {
            label: "set".into(),
            url: format!("file://{}", dir.join("missing-source").display()),
            blake3: "0".repeat(64),
        };
        let err = crate::platform::store_build(&pin).unwrap_err();

        assert!(err.contains("missing-source"), "{err}");
        assert!(!stored.exists(), "an unreachable source publishes nothing");

        unsafe { env::remove_var(HINT) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pinned GL build reaches acquisition the same way, and a build
    /// that cannot be had comes back as a reported outcome, the shape
    /// the launch turns into a warning while it goes on without a GL
    /// stack.
    #[test]
    fn an_unusable_pinned_gl_build_reports_instead_of_aborting() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("gl-optional");
        let hint = dir.join("store");
        let pkg = dir.join("pkg");
        std::fs::create_dir_all(pkg.join(".onelf")).unwrap();
        std::fs::write(
            pkg.join(crate::platform::PIN_FILE),
            format!(
                "label = \"gl\"\nurl = \"file://{}\"\nblake3 = \"{}\"\n",
                dir.join("missing-source").display(),
                "0".repeat(64)
            ),
        )
        .unwrap();

        unsafe { env::set_var(HINT, &hint) };
        let outcome = crate::platform::obtain(&pkg);

        assert!(outcome.is_some_and(|r| r.is_err()));

        unsafe { env::remove_var(HINT) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The export replaces, never accumulates: repeated launches of the
    /// same package under the same inheritance export one identical
    /// resolved store, and a launch that inherits the previous launch's
    /// export re-exports that same resolution.
    #[test]
    fn repeated_launches_reexport_one_identical_resolved_store() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("idempotent");
        let hint = dir.join("store");
        unsafe { env::set_var(HINT, &hint) };

        call();
        let first = env::var(HINT).unwrap();
        call();
        let second = env::var(HINT).unwrap();

        assert_eq!(first, second);
        assert_eq!(first, expected_export(&hint));

        unsafe { env::remove_var(HINT) };
        let _ = std::fs::remove_dir_all(&dir);
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
mod test_env_lock {
    use std::sync::Mutex;

    /// The environment is process-global and the test runner runs tests
    /// in parallel: every test in this file that sets or asserts a
    /// variable holds this lock while it does.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());
}

#[cfg(test)]
mod plugin_dir_tests {
    use super::test_env_lock::ENV_LOCK;
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("onelf-plugins-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    /// Module directories replace what the caller set, data paths fill
    /// only what the caller left unset, and a set's modules follow the
    /// package's. The registry pass behind the same roots keeps the
    /// caller's ALSA value and sets the Tcl script library.
    #[test]
    fn modules_replace_and_data_fills() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("rule");
        let pkg = work.join("pkg");
        let set = work.join("set");
        let lib = pkg.join("lib");
        let arch = pkg.join("lib/x86_64-linux-gnu");
        let set_lib = set.join("lib");
        for dir in [
            lib.join("gstreamer-1.0"),
            set_lib.join("gstreamer-1.0"),
            arch.join("girepository-1.0"),
            lib.join("alsa-lib"),
            pkg.join("share/alsa"),
            pkg.join("share/X11/locale"),
            lib.join("tcl8.6"),
            lib.join("libheif"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        touch(&pkg.join("share/alsa/alsa.conf"));
        touch(&lib.join("gstreamer-1.0/gst-plugin-scanner"));
        touch(&lib.join("tcl8.6/init.tcl"));
        unsafe {
            env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", "/usr/lib/gstreamer-1.0");
            env::set_var("ALSA_CONFIG_PATH", "/home/a/my-alsa.conf");
            env::set_var("ALSA_CONFIG_DIR", "/usr/share/alsa");
            env::remove_var("XLOCALEDIR");
            env::remove_var("PIPEWIRE_MODULE_DIR");
        }

        let lib_dirs = [lib.clone(), arch.clone()];
        let set_libs = [set_lib.clone()];
        let set_roots = [set.clone()];
        setup_plugin_dirs(
            &ActiveRules::new(None, &pkg, &[]),
            &pkg,
            &lib_dirs,
            &set_libs,
            &set_roots,
        );
        run_runtime_hooks(
            &all_runtime_hooks(),
            &HookContext {
                pkg: &pkg,
                lib_dirs: &lib_dirs,
                set_lib_dirs: &set_libs,
                set_roots: &set_roots,
            },
        );

        let var = |name| env::var(name).unwrap_or_default();
        assert_eq!(
            var("GST_PLUGIN_SYSTEM_PATH_1_0"),
            format!(
                "{}:{}",
                lib.join("gstreamer-1.0").display(),
                set_lib.join("gstreamer-1.0").display()
            )
        );
        assert_eq!(
            var("GST_PLUGIN_SCANNER"),
            lib.join("gstreamer-1.0/gst-plugin-scanner")
                .to_string_lossy()
        );
        assert_eq!(
            var("GI_TYPELIB_PATH"),
            arch.join("girepository-1.0").to_string_lossy()
        );
        assert_eq!(
            var("ALSA_PLUGIN_DIR"),
            lib.join("alsa-lib").to_string_lossy()
        );
        assert_eq!(var("TCL_LIBRARY"), lib.join("tcl8.6").to_string_lossy());
        assert_eq!(
            var("LIBHEIF_PLUGIN_PATH"),
            lib.join("libheif").to_string_lossy()
        );
        assert!(env::var_os("PIPEWIRE_MODULE_DIR").is_none());
        assert_eq!(var("ALSA_CONFIG_PATH"), "/home/a/my-alsa.conf");
        // The carried alsa.conf hook is anchored at `{ @func datadir }`,
        // so the carried directory replaces an inherited value.
        assert_eq!(
            var("ALSA_CONFIG_DIR"),
            pkg.join("share/alsa").to_string_lossy()
        );
        assert_eq!(
            var("XLOCALEDIR"),
            pkg.join("share/X11/locale").to_string_lossy()
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// `alsa.conf` first, then the carried `alsa.conf.d` files in
    /// alsa-lib's numeric name order, `*.conf` only. The list is a
    /// fallback for alsa-lib builds with no conf.d hook: against a hook,
    /// the carried `alsa.conf` patch decides, not this order.
    #[test]
    fn alsa_config_path_appends_the_carried_conf_d() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("alsaconf");
        let pkg = work.join("pkg");
        let alsa = pkg.join("share/alsa");
        touch(&alsa.join("alsa.conf"));
        touch(&alsa.join("alsa.conf.d/99-pulseaudio-default.conf"));
        touch(&alsa.join("alsa.conf.d/100-extra.conf"));
        touch(&alsa.join("alsa.conf.d/README"));
        std::fs::create_dir_all(alsa.join("alsa.conf.d/10-not-a-file")).unwrap();
        unsafe { env::remove_var("ALSA_CONFIG_PATH") };

        set_alsa_config_path(&[pkg.as_path()]);

        let paths: Vec<String> = env::split_paths(&env::var_os("ALSA_CONFIG_PATH").unwrap())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            [
                alsa.join("alsa.conf").to_string_lossy().into_owned(),
                alsa.join("alsa.conf.d/99-pulseaudio-default.conf")
                    .to_string_lossy()
                    .into_owned(),
                alsa.join("alsa.conf.d/100-extra.conf")
                    .to_string_lossy()
                    .into_owned(),
            ]
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// A path alsa-lib cannot parse is worse than no list: the variable
    /// splits on spaces as well as colons, so a bundle below a directory
    /// with a space in its name stays unset and the carried `alsa.conf`
    /// is still found through `ALSA_CONFIG_DIR`.
    #[test]
    fn alsa_config_path_is_skipped_when_a_path_holds_a_space() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("alsaconf-space");
        let pkg = work.join("My App");
        let alsa = pkg.join("share/alsa");
        touch(&alsa.join("alsa.conf"));
        unsafe { env::remove_var("ALSA_CONFIG_PATH") };

        set_alsa_config_path(&[pkg.as_path()]);

        assert!(env::var_os("ALSA_CONFIG_PATH").is_none());

        let _ = std::fs::remove_dir_all(&work);
    }

    /// Without a carried `alsa.conf` the variable is left unset so ALSA
    /// falls back to its compiled-in data directory.
    #[test]
    fn alsa_config_path_needs_a_carried_alsa_conf() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("alsaconf-missing");
        let pkg = work.join("pkg");
        std::fs::create_dir_all(pkg.join("share/alsa")).unwrap();
        unsafe { env::remove_var("ALSA_CONFIG_PATH") };

        set_alsa_config_path(&[pkg.as_path()]);

        assert!(env::var_os("ALSA_CONFIG_PATH").is_none());

        let _ = std::fs::remove_dir_all(&work);
    }

    /// Numeric order, the way alsa-lib sorts its snippet directory:
    /// `99-x.conf` after `100-y.conf`, which bytewise order inverts.
    #[test]
    fn natural_order_puts_nine_before_100() {
        use std::cmp::Ordering::{Equal, Less};
        assert_eq!(natural_cmp("99-x.conf", "100-y.conf"), Less);
        assert_eq!(natural_cmp("50-a.conf", "99-b.conf"), Less);
        assert_eq!(natural_cmp("abc.conf", "abd.conf"), Less);
        assert_eq!(natural_cmp("10-x.conf", "10-x.conf"), Equal);
    }

    /// The caller's terminfo stays ahead of the package's, which is added
    /// once however often the package is launched from itself.
    #[test]
    fn terminfo_comes_after_the_callers() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("terminfo");
        let pkg = work.join("pkg");
        std::fs::create_dir_all(pkg.join("share/terminfo")).unwrap();
        unsafe { env::set_var("TERMINFO_DIRS", ":/home/a/.local/terminfo") };

        setup_terminfo(&pkg, &[]);
        setup_terminfo(&pkg, &[]);

        assert_eq!(
            env::var("TERMINFO_DIRS").unwrap(),
            format!(
                ":/home/a/.local/terminfo:{}",
                pkg.join("share/terminfo").display()
            )
        );
        let _ = std::fs::remove_dir_all(&work);
    }
}

#[cfg(test)]
mod registry_tests {
    use super::test_env_lock::ENV_LOCK;
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("onelf-registry-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    /// Every file below `root`, as sorted relative paths, so a test can
    /// detect any write a handler makes into the payload.
    fn files_below(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        out.sort();
        out
    }

    /// The registry is closed and ordered: exactly the five runtime hook
    /// identities, once each, in declaration order, and every identity is
    /// reachable through the by-id lookup.
    #[test]
    fn the_registry_holds_every_hook_once_in_a_fixed_order() {
        let ids: Vec<RuntimeHookId> = runtime_hooks().iter().map(|hook| hook.id).collect();
        assert_eq!(
            ids,
            vec![
                RuntimeHookId::GdkCacheAbsolutise,
                RuntimeHookId::GioModulePaths,
                RuntimeHookId::FontconfigGenerate,
                RuntimeHookId::TclTkLibraries,
                RuntimeHookId::AlsaConfigPathNaturalSort,
            ]
        );
        assert_eq!(runtime_hooks().len(), 5);
        for id in ids {
            assert!(runtime_hook(id).is_some(), "{id:?} has no handler");
        }
    }

    /// One registry pass reproduces every previously procedural outcome
    /// and writes nothing into the payload tree.
    #[test]
    fn one_registry_pass_produces_every_handler_outcome() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("golden");
        let pkg = work.join("regpkg");
        let lib = pkg.join("lib");
        let version = lib.join("gdk-pixbuf-2.0/2.10.0");
        mkdir(&version);
        mkdir(&lib.join("gtk-3.0/3.0.0"));
        mkdir(&lib.join("gio/modules"));
        mkdir(&pkg.join("share/fonts"));
        mkdir(&lib.join("tcl8.6"));
        mkdir(&lib.join("tk8.6"));
        mkdir(&pkg.join("share/alsa/alsa.conf.d"));
        touch(&lib.join("tcl8.6/init.tcl"));
        touch(&lib.join("tk8.6/tk.tcl"));
        touch(&pkg.join("share/alsa/alsa.conf"));
        touch(&pkg.join("share/alsa/alsa.conf.d/100-extra.conf"));
        touch(&pkg.join("share/alsa/alsa.conf.d/99-pulse.conf"));
        std::fs::write(
            version.join("loaders.cache"),
            "\"../loaders/libpixbuf-png.so\"\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("gtk-3.0/3.0.0/immodules.cache"),
            "\"../im-modules/libim-gif.so\"\n",
        )
        .unwrap();
        unsafe {
            for name in [
                "GDK_PIXBUF_MODULE_FILE",
                "GTK_IM_MODULE_FILE",
                "GIO_MODULE_DIR",
                "GIO_EXTRA_MODULES",
                "FONTCONFIG_FILE",
                "TCL_LIBRARY",
                "TK_LIBRARY",
                "ALSA_CONFIG_PATH",
            ] {
                env::remove_var(name);
            }
        }
        let payload_before = files_below(&pkg);

        run_runtime_hooks(
            &all_runtime_hooks(),
            &HookContext {
                pkg: &pkg,
                lib_dirs: std::slice::from_ref(&lib),
                set_lib_dirs: &[],
                set_roots: &[],
            },
        );

        let base = crate::paths::private_dir().expect("private base exists in tests");
        let var = |name: &str| env::var(name).unwrap_or_default();
        assert_eq!(
            env::var_os("GDK_PIXBUF_MODULE_FILE"),
            Some(base.join("modules-regpkg/loaders.cache").into_os_string())
        );
        assert_eq!(
            env::var_os("GTK_IM_MODULE_FILE"),
            Some(base.join("modules-regpkg/immodules.cache").into_os_string())
        );
        assert_eq!(
            var("GIO_MODULE_DIR"),
            lib.join("gio/modules").to_string_lossy()
        );
        assert!(env::var_os("GIO_EXTRA_MODULES").is_none());
        let fontconfig = PathBuf::from(env::var_os("FONTCONFIG_FILE").unwrap());
        assert!(fontconfig.starts_with(base.join("fontconfig-regpkg")));
        let conf = std::fs::read_to_string(&fontconfig).unwrap();
        assert!(conf.contains("<include ignore_missing=\"yes\">/etc/fonts/fonts.conf</include>"));
        assert!(conf.contains(&format!("<dir>{}</dir>", pkg.join("share/fonts").display())));
        assert_eq!(var("TCL_LIBRARY"), lib.join("tcl8.6").to_string_lossy());
        assert_eq!(var("TK_LIBRARY"), lib.join("tk8.6").to_string_lossy());
        assert_eq!(
            env::split_paths(&env::var_os("ALSA_CONFIG_PATH").unwrap())
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                pkg.join("share/alsa/alsa.conf")
                    .to_string_lossy()
                    .into_owned(),
                pkg.join("share/alsa/alsa.conf.d/99-pulse.conf")
                    .to_string_lossy()
                    .into_owned(),
                pkg.join("share/alsa/alsa.conf.d/100-extra.conf")
                    .to_string_lossy()
                    .into_owned(),
            ]
        );
        assert_eq!(files_below(&pkg), payload_before);

        let _ = std::fs::remove_dir_all(&work);
    }

    /// Running the cache handler twice leaves one private copy with the
    /// same bytes and the same variable value, and the carried cache is
    /// never rewritten.
    #[test]
    fn cache_absolutising_is_idempotent_across_passes() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("idem");
        let pkg = work.join("idempkg");
        let cache_dir = pkg.join("lib/gdk-pixbuf-2.0/2.10.0");
        mkdir(&cache_dir);
        let carried = cache_dir.join("loaders.cache");
        let text = "# GdkPixbuf Image Loader Modules file\n\"png\" \"image/png\" \"\"\n\"../loaders/libpixbuf-png.so\"\n";
        std::fs::write(&carried, text).unwrap();
        unsafe { env::remove_var("GDK_PIXBUF_MODULE_FILE") };
        let context = HookContext {
            pkg: &pkg,
            lib_dirs: &[],
            set_lib_dirs: &[],
            set_roots: &[],
        };

        run_gdk_cache_absolutise(&context);

        let base = crate::paths::private_dir().expect("private base exists in tests");
        let dest = base.join("modules-idempkg/loaders.cache");
        let first = env::var_os("GDK_PIXBUF_MODULE_FILE").unwrap();
        assert_eq!(first, dest.clone().into_os_string());
        let first_bytes = std::fs::read(&dest).unwrap();
        let rewritten = cache_dir
            .join("../loaders/libpixbuf-png.so")
            .to_string_lossy()
            .into_owned();
        assert!(String::from_utf8_lossy(&first_bytes).contains(&rewritten));

        run_gdk_cache_absolutise(&context);

        assert_eq!(env::var_os("GDK_PIXBUF_MODULE_FILE"), Some(first));
        assert_eq!(std::fs::read(&dest).unwrap(), first_bytes);
        assert_eq!(std::fs::read(&carried).unwrap(), text.as_bytes());
        let copies = std::fs::read_dir(base.join("modules-idempkg"))
            .unwrap()
            .count();
        assert_eq!(copies, 1);

        let _ = std::fs::remove_dir_all(&work);
    }

    /// The generated fontconfig file lands in the package's private
    /// state, reached through the by-id lookup, and the payload tree
    /// gains nothing.
    #[test]
    fn fontconfig_output_stays_in_private_state() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("fc");
        let pkg = work.join("fcpkg");
        mkdir(&pkg.join("share/fonts"));
        touch(&pkg.join("share/fonts/decoy.ttf"));
        unsafe { env::remove_var("FONTCONFIG_FILE") };
        let payload_before = files_below(&pkg);
        let run = runtime_hook(RuntimeHookId::FontconfigGenerate).expect("registered");

        run(&HookContext {
            pkg: &pkg,
            lib_dirs: &[],
            set_lib_dirs: &[],
            set_roots: &[],
        });

        let base = crate::paths::private_dir().expect("private base exists in tests");
        let written = PathBuf::from(env::var_os("FONTCONFIG_FILE").unwrap());
        assert!(written.starts_with(base.join("fontconfig-fcpkg")));
        assert!(written.is_file());
        assert_eq!(files_below(&pkg), payload_before);

        let _ = std::fs::remove_dir_all(&work);
    }

    /// The registry keeps a caller's `ALSA_CONFIG_PATH` ahead of the
    /// carried configuration and snippets.
    #[test]
    fn the_registry_keeps_a_user_alsa_override() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("alsa");
        let pkg = work.join("alsapkg");
        mkdir(&pkg.join("share/alsa/alsa.conf.d"));
        touch(&pkg.join("share/alsa/alsa.conf"));
        touch(&pkg.join("share/alsa/alsa.conf.d/50-carried.conf"));
        unsafe { env::set_var("ALSA_CONFIG_PATH", "/home/a/my-alsa.conf") };

        run_alsa_config_path_natural_sort(&HookContext {
            pkg: &pkg,
            lib_dirs: &[],
            set_lib_dirs: &[],
            set_roots: &[],
        });

        assert_eq!(
            env::var("ALSA_CONFIG_PATH").as_deref(),
            Ok("/home/a/my-alsa.conf")
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// A path holding a space makes the whole ALSA list unrepresentable:
    /// the registry leaves the variable unset rather than emit a broken
    /// list.
    #[test]
    fn the_registry_skips_an_alsa_list_with_a_space() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("alsa-space");
        let pkg = work.join("My App");
        mkdir(&pkg.join("share/alsa"));
        touch(&pkg.join("share/alsa/alsa.conf"));
        unsafe { env::remove_var("ALSA_CONFIG_PATH") };

        run_alsa_config_path_natural_sort(&HookContext {
            pkg: &pkg,
            lib_dirs: &[],
            set_lib_dirs: &[],
            set_roots: &[],
        });

        assert!(env::var_os("ALSA_CONFIG_PATH").is_none());

        let _ = std::fs::remove_dir_all(&work);
    }
}

#[cfg(test)]
mod path_mapping_tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use onelf_format::rules::{RelativePath, RuleId, RuntimeCandidate};

    use super::test_env_lock::ENV_LOCK;
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("onelf-mapping-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    fn group(
        id: &str,
        kind: ResourceKind,
        selection: PathSelection,
        inheritance: Inheritance,
        candidates: &[(RuntimeBase, &str)],
        variables: &[&str],
    ) -> RuntimePathGroup {
        RuntimePathGroup {
            id: RuleId::new(id).unwrap(),
            kind,
            candidates: candidates
                .iter()
                .map(|(base, path)| RuntimeCandidate {
                    base: *base,
                    path: RelativePath::new(path).unwrap(),
                })
                .collect(),
            variables: variables
                .iter()
                .map(|name| EnvironmentName::new(name).unwrap())
                .collect(),
            selection,
            inheritance,
        }
    }

    /// A set candidate declared before the application's own loses: the
    /// scopes are searched application first, whatever the declaration
    /// order says.
    #[test]
    fn first_selection_prefers_the_application_over_an_earlier_set_candidate() {
        let work = tree("first");
        let (app, set) = (work.join("app"), work.join("set"));
        mkdir(&app.join("b"));
        mkdir(&set.join("a"));
        mkdir(&set.join("b"));
        let set_roots = vec![set.as_path()];
        let scopes = [
            Scope {
                packages: &[app.as_path()],
                libraries: &[],
            },
            Scope {
                packages: &set_roots,
                libraries: &[],
            },
        ];
        let groups = [group(
            "g",
            ResourceKind::Directory,
            PathSelection::First,
            Inheritance::Replace,
            &[
                (RuntimeBase::PackageRoot, "a"),
                (RuntimeBase::PackageRoot, "b"),
            ],
            &["ONELFTEST_FIRST_G"],
        )];

        let got = evaluate_path_groups(&groups, &scopes);

        assert_eq!(got.len(), 1);
        assert_eq!(got[0].variables, ["ONELFTEST_FIRST_G"]);
        assert_eq!(got[0].value, app.join("b").into_os_string());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// An all-match group lists the application's matches before the
    /// sets', keeps declaration order within a root, and never lists one
    /// path twice.
    #[test]
    fn all_selection_orders_application_before_sets_and_deduplicates() {
        let work = tree("all");
        let (app, set) = (work.join("app"), work.join("set"));
        mkdir(&app.join("m"));
        mkdir(&app.join("second"));
        mkdir(&app.join("lib/m"));
        mkdir(&set.join("m"));
        let (set_roots, app_libs) = (vec![set.as_path()], vec![app.join("lib")]);
        let scopes = [
            Scope {
                packages: &[app.as_path()],
                libraries: &app_libs,
            },
            Scope {
                packages: &set_roots,
                libraries: &[],
            },
        ];
        let groups = [
            group(
                "dedup",
                ResourceKind::Directory,
                PathSelection::All,
                Inheritance::Replace,
                &[
                    (RuntimeBase::LibraryRoot, "m"),
                    (RuntimeBase::LibraryRoot, "m"),
                ],
                &["ONELFTEST_ALL_A"],
            ),
            group(
                "ordered",
                ResourceKind::Directory,
                PathSelection::All,
                Inheritance::Replace,
                &[
                    (RuntimeBase::PackageRoot, "second"),
                    (RuntimeBase::PackageRoot, "m"),
                ],
                &["ONELFTEST_ALL_B"],
            ),
        ];

        let got = evaluate_path_groups(&groups, &scopes);

        assert_eq!(got[0].value, app.join("lib/m").into_os_string());
        assert_eq!(
            got[1].value,
            env::join_paths([app.join("second"), app.join("m"), set.join("m")]).unwrap()
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Each library root contributes its first existing alternative, so
    /// the nested plugin directory wins where a library root carries one
    /// and the flat directory serves the others.
    #[test]
    fn first_per_library_root_prefers_the_nested_plugin_directory() {
        let work = tree("heif");
        let (lib_a, lib_b, set_lib) =
            (work.join("a/lib"), work.join("b/lib"), work.join("set/lib"));
        mkdir(&lib_a.join("libheif/plugins"));
        mkdir(&lib_b.join("libheif"));
        mkdir(&set_lib.join("libheif"));
        let (app_libs, set_libs) = (vec![lib_a.clone(), lib_b.clone()], vec![set_lib.clone()]);
        let scopes = [
            Scope {
                packages: &[],
                libraries: &app_libs,
            },
            Scope {
                packages: &[],
                libraries: &set_libs,
            },
        ];
        let groups = [group(
            "plugins",
            ResourceKind::Directory,
            PathSelection::FirstPerLibraryRoot,
            Inheritance::Replace,
            &[
                (RuntimeBase::LibraryRoot, "libheif/plugins"),
                (RuntimeBase::LibraryRoot, "libheif"),
            ],
            &["ONELFTEST_HEIF"],
        )];

        let got = evaluate_path_groups(&groups, &scopes);

        assert_eq!(
            got[0].value,
            env::join_paths([
                lib_a.join("libheif/plugins"),
                lib_b.join("libheif"),
                set_lib.join("libheif"),
            ])
            .unwrap()
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A candidate must have the declared kind: a directory cannot
    /// satisfy a file group nor a file a directory group, and a later
    /// candidate of the right kind still serves.
    #[test]
    fn a_candidate_of_the_wrong_kind_is_skipped() {
        let work = tree("kinds");
        let app = work.join("app");
        mkdir(&app.join("data"));
        touch(&app.join("real.mgc"));
        touch(&app.join("plain"));
        let scopes = [Scope {
            packages: &[app.as_path()],
            libraries: &[],
        }];
        let files = [group(
            "db",
            ResourceKind::File,
            PathSelection::First,
            Inheritance::Preserve,
            &[
                (RuntimeBase::PackageRoot, "data"),
                (RuntimeBase::PackageRoot, "real.mgc"),
            ],
            &["ONELFTEST_KIND_FILE"],
        )];
        let dirs = [group(
            "solo",
            ResourceKind::Directory,
            PathSelection::First,
            Inheritance::Replace,
            &[(RuntimeBase::PackageRoot, "plain")],
            &["ONELFTEST_KIND_DIR"],
        )];

        let got = evaluate_path_groups(&files, &scopes);
        assert_eq!(got[0].value, app.join("real.mgc").into_os_string());
        assert!(evaluate_path_groups(&dirs, &scopes).is_empty());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// No present candidate means no assignment: the group can neither
    /// invent a path nor override an inherited value.
    #[test]
    fn a_group_without_a_present_candidate_assigns_nothing() {
        let work = tree("absent");
        let app = work.join("app");
        mkdir(&app);
        let scopes = [Scope {
            packages: &[app.as_path()],
            libraries: &[],
        }];
        let groups = [group(
            "messages",
            ResourceKind::Directory,
            PathSelection::First,
            Inheritance::Preserve,
            &[(RuntimeBase::PackageRoot, "share/locale")],
            &["ONELFTEST_ABSENT"],
        )];

        assert!(evaluate_path_groups(&groups, &scopes).is_empty());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Only the roots the launch passes in are searched: a set that is
    /// not active can neither satisfy a group nor leak a path.
    #[test]
    fn an_unselected_set_never_contributes() {
        let work = tree("inactive");
        let (app, active, inactive) =
            (work.join("app"), work.join("active"), work.join("inactive"));
        mkdir(&inactive.join("share/locale"));
        let set_roots = vec![active.as_path()];
        let scopes = [
            Scope {
                packages: &[app.as_path()],
                libraries: &[],
            },
            Scope {
                packages: &set_roots,
                libraries: &[],
            },
        ];
        let groups = [group(
            "messages",
            ResourceKind::Directory,
            PathSelection::First,
            Inheritance::Preserve,
            &[(RuntimeBase::PackageRoot, "share/locale")],
            &["ONELFTEST_INACTIVE"],
        )];

        assert!(evaluate_path_groups(&groups, &scopes).is_empty());

        mkdir(&active.join("share/locale"));
        let got = evaluate_path_groups(&groups, &scopes);
        assert_eq!(got[0].value, active.join("share/locale").into_os_string());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Every variable the replaced path tables set has a mapping in the
    /// built-in catalog, so no behavior was silently dropped, and the
    /// catalog exercises all three selection modes.
    #[test]
    fn the_builtin_catalog_covers_the_replaced_table_variables() {
        let names: BTreeSet<&str> = builtin_path_groups()
            .flat_map(|group| group.variables.iter().map(EnvironmentName::as_str))
            .collect();
        let expected = [
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_SCANNER_1_0",
            "GST_PLUGIN_SCANNER",
            "PIPEWIRE_MODULE_DIR",
            "SPA_PLUGIN_DIR",
            "GI_TYPELIB_PATH",
            "BABL_PATH",
            "GEGL_PATH",
            "LIBHEIF_PLUGIN_PATH",
            "ALSA_PLUGIN_DIR",
            "ALSA_CONFIG_DIR",
            "TEXTDOMAINDIR",
            "XLOCALEDIR",
            "MAGIC",
            "QT_PLUGIN_PATH",
            "GTK_PATH",
            "XKB_CONFIG_ROOT",
        ];
        for name in expected {
            assert!(names.contains(name), "{name} has no mapping");
        }
        assert!(builtin_path_groups().any(|g| g.selection == PathSelection::FirstPerLibraryRoot));
        assert!(builtin_path_groups().any(|g| g.selection == PathSelection::All));
        assert!(builtin_path_groups().any(|g| g.kind == ResourceKind::File));
    }

    /// Every runtime mapping the catalog declares evaluates end to end:
    /// with only its first candidate present, each of its aliases
    /// receives that candidate's path, under every selection mode the
    /// catalog uses.
    #[test]
    fn every_catalog_mapping_evaluates_its_first_present_candidate() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut checked = 0usize;
        for descriptor in onelf_format::catalog::builtins() {
            for group in &descriptor.runtime.paths {
                let work = tree(&format!(
                    "eval-{}-{}",
                    descriptor.id.as_str(),
                    group.id.as_str()
                ));
                let pkg = work.join("pkg");
                let lib = pkg.join("lib");
                mkdir(&lib);
                let first = &group.candidates[0];
                let path = match first.base {
                    RuntimeBase::PackageRoot => pkg.clone(),
                    RuntimeBase::LibraryRoot => lib.clone(),
                }
                .join(first.path.as_str());
                match group.kind {
                    ResourceKind::File => touch(&path),
                    ResourceKind::Directory => mkdir(&path),
                }
                let pkg_ref = pkg.as_path();
                let lib_ref = lib.clone();
                let scopes = [Scope {
                    packages: std::slice::from_ref(&pkg_ref),
                    libraries: std::slice::from_ref(&lib_ref),
                }];
                for variable in &group.variables {
                    unsafe { env::remove_var(variable.as_str()) };
                }

                let assignments = evaluate_path_groups(std::iter::once(group), &scopes);
                apply_path_assignments(&assignments);
                let got = assignments;

                assert_eq!(
                    got.len(),
                    1,
                    "{}/{} assigns nothing with its first candidate present",
                    descriptor.id.as_str(),
                    group.id.as_str()
                );
                let assigned: Vec<&str> = got[0].variables.to_vec();
                let want: Vec<&str> = group.variables.iter().map(|v| v.as_str()).collect();
                assert_eq!(assigned, want, "{}", descriptor.id.as_str());
                assert_eq!(
                    got[0].value,
                    path.clone().into_os_string(),
                    "{}/{}",
                    descriptor.id.as_str(),
                    group.id.as_str()
                );
                for variable in &group.variables {
                    assert_eq!(
                        env::var_os(variable.as_str()),
                        Some(got[0].value.clone()),
                        "{}",
                        variable.as_str()
                    );
                    unsafe { env::remove_var(variable.as_str()) };
                }
                checked += 1;
                let _ = std::fs::remove_dir_all(&work);
            }
        }
        assert!(
            checked >= 15,
            "the catalog declares only {checked} mappings"
        );
    }

    /// The built-in mappings reproduce the outcomes of the path tables
    /// they replaced for every framework those tables covered: aliases,
    /// kinds, per-library-root nesting, application-before-set order,
    /// replace for module lists and preserve for caller-chosen data.
    #[test]
    fn builtin_mappings_reproduce_the_replaced_framework_tables() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("golden");
        let (pkg, set) = (work.join("pkg"), work.join("set"));
        let lib = pkg.join("lib");
        let arch = pkg.join("lib/x86_64-linux-gnu");
        let set_lib = set.join("lib");
        for dir in [
            lib.join("gstreamer-1.0"),
            set_lib.join("gstreamer-1.0"),
            lib.join("pipewire-0.3"),
            lib.join("spa-0.2"),
            lib.join("babl-0.1"),
            lib.join("gegl-0.4"),
            arch.join("girepository-1.0"),
            lib.join("gtk-3.0"),
            set_lib.join("gtk-3.0"),
            lib.join("qt6/plugins"),
            lib.join("qt5/plugins"),
            lib.join("alsa-lib"),
            lib.join("libheif/plugins"),
            set_lib.join("libheif"),
            pkg.join("share/alsa"),
            pkg.join("share/locale"),
            set.join("share/locale"),
            pkg.join("share/X11/locale"),
            pkg.join("share/X11/xkb"),
        ] {
            mkdir(&dir);
        }
        touch(&lib.join("gstreamer-1.0/gst-plugin-scanner"));
        touch(&pkg.join("share/file/misc/magic.mgc"));
        let table = [
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_SCANNER_1_0",
            "GST_PLUGIN_SCANNER",
            "PIPEWIRE_MODULE_DIR",
            "SPA_PLUGIN_DIR",
            "GI_TYPELIB_PATH",
            "BABL_PATH",
            "GEGL_PATH",
            "LIBHEIF_PLUGIN_PATH",
            "ALSA_PLUGIN_DIR",
            "ALSA_CONFIG_DIR",
            "TEXTDOMAINDIR",
            "XLOCALEDIR",
            "XKB_CONFIG_ROOT",
            "MAGIC",
            "QT_PLUGIN_PATH",
            "GTK_PATH",
            "TCL_LIBRARY",
            "TK_LIBRARY",
            "ALSA_CONFIG_PATH",
        ];
        for name in table {
            unsafe { env::remove_var(name) };
        }
        // A caller value on a replace-policy list is overridden; a
        // caller value on a preserve-policy data path is kept.
        unsafe { env::set_var("GST_PLUGIN_SYSTEM_PATH", "/usr/lib/gstreamer-1.0") };
        unsafe { env::set_var("MAGIC", "/caller/magic.mgc") };

        setup_plugin_dirs(
            &ActiveRules::new(None, &pkg, &[]),
            &pkg,
            &[lib.clone(), arch.clone()],
            std::slice::from_ref(&set_lib),
            &[set],
        );

        let expected = [
            (
                "GST_PLUGIN_SYSTEM_PATH_1_0",
                format!(
                    "{}:{}",
                    lib.join("gstreamer-1.0").display(),
                    set_lib.join("gstreamer-1.0").display()
                ),
            ),
            (
                "GST_PLUGIN_SYSTEM_PATH",
                format!(
                    "{}:{}",
                    lib.join("gstreamer-1.0").display(),
                    set_lib.join("gstreamer-1.0").display()
                ),
            ),
            (
                "GST_PLUGIN_SCANNER_1_0",
                lib.join("gstreamer-1.0/gst-plugin-scanner")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "GST_PLUGIN_SCANNER",
                lib.join("gstreamer-1.0/gst-plugin-scanner")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "PIPEWIRE_MODULE_DIR",
                lib.join("pipewire-0.3").to_string_lossy().into_owned(),
            ),
            (
                "SPA_PLUGIN_DIR",
                lib.join("spa-0.2").to_string_lossy().into_owned(),
            ),
            (
                "GI_TYPELIB_PATH",
                arch.join("girepository-1.0").to_string_lossy().into_owned(),
            ),
            (
                "BABL_PATH",
                lib.join("babl-0.1").to_string_lossy().into_owned(),
            ),
            (
                "GEGL_PATH",
                lib.join("gegl-0.4").to_string_lossy().into_owned(),
            ),
            (
                "GTK_PATH",
                format!(
                    "{}:{}",
                    lib.join("gtk-3.0").display(),
                    set_lib.join("gtk-3.0").display()
                ),
            ),
            (
                "QT_PLUGIN_PATH",
                format!(
                    "{}:{}",
                    lib.join("qt6/plugins").display(),
                    lib.join("qt5/plugins").display()
                ),
            ),
            (
                "ALSA_PLUGIN_DIR",
                lib.join("alsa-lib").to_string_lossy().into_owned(),
            ),
            (
                "LIBHEIF_PLUGIN_PATH",
                format!(
                    "{}:{}",
                    lib.join("libheif/plugins").display(),
                    set_lib.join("libheif").display()
                ),
            ),
            (
                "ALSA_CONFIG_DIR",
                pkg.join("share/alsa").to_string_lossy().into_owned(),
            ),
            (
                "TEXTDOMAINDIR",
                pkg.join("share/locale").to_string_lossy().into_owned(),
            ),
            (
                "XLOCALEDIR",
                pkg.join("share/X11/locale").to_string_lossy().into_owned(),
            ),
            (
                "XKB_CONFIG_ROOT",
                pkg.join("share/X11/xkb").to_string_lossy().into_owned(),
            ),
            ("MAGIC", "/caller/magic.mgc".to_string()),
        ];
        for (name, value) in expected {
            assert_eq!(env::var(name).as_deref(), Ok(value.as_str()), "{name}");
        }
        for name in ["TCL_LIBRARY", "TK_LIBRARY", "ALSA_CONFIG_PATH"] {
            assert!(env::var_os(name).is_none(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Both scanner aliases follow the `libexec` staging layout too, and
    /// the Debian layout below a set's library directory serves when the
    /// application carries neither.
    #[test]
    fn the_scanner_aliases_follow_the_alternative_layouts() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let work = tree("scanner");
        let pkg = work.join("pkg");
        touch(&pkg.join("libexec/gstreamer-1.0/gst-plugin-scanner"));
        unsafe {
            env::remove_var("GST_PLUGIN_SCANNER_1_0");
            env::remove_var("GST_PLUGIN_SCANNER");
        }

        setup_plugin_dirs(
            &ActiveRules::new(None, &pkg, &[]),
            &pkg,
            &[pkg.join("lib")],
            &[],
            &[],
        );

        let libexec = pkg
            .join("libexec/gstreamer-1.0/gst-plugin-scanner")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            env::var("GST_PLUGIN_SCANNER_1_0").as_deref(),
            Ok(libexec.as_str())
        );
        assert_eq!(
            env::var("GST_PLUGIN_SCANNER").as_deref(),
            Ok(libexec.as_str())
        );

        let pkg = work.join("pkg2");
        let set = work.join("set");
        let set_lib = set.join("lib");
        touch(&set_lib.join("gstreamer1.0/gstreamer-1.0/gst-plugin-scanner"));
        unsafe {
            env::remove_var("GST_PLUGIN_SCANNER_1_0");
            env::remove_var("GST_PLUGIN_SCANNER");
        }

        setup_plugin_dirs(
            &ActiveRules::new(None, &pkg, &[]),
            &pkg,
            std::slice::from_ref(&pkg.join("lib")),
            std::slice::from_ref(&set_lib),
            &[set],
        );

        let debian = set_lib
            .join("gstreamer1.0/gstreamer-1.0/gst-plugin-scanner")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            env::var("GST_PLUGIN_SCANNER_1_0").as_deref(),
            Ok(debian.as_str())
        );
        assert_eq!(
            env::var("GST_PLUGIN_SCANNER").as_deref(),
            Ok(debian.as_str())
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Preserve keeps a present value, even an explicitly empty one,
    /// while replace overrides it.
    #[test]
    fn preserve_keeps_present_values_and_replace_overrides() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            env::set_var("ONELF_T_KEEP_a1", "/kept");
            env::set_var("ONELF_T_EMPTY_a2", "");
            env::set_var("ONELF_T_OLD_a3", "/old");
            env::remove_var("ONELF_T_FILL_a4");
        }
        let kept = PathAssignment {
            variables: vec!["ONELF_T_KEEP_a1"],
            value: "/new".into(),
            inheritance: Inheritance::Preserve,
        };
        let empty = PathAssignment {
            variables: vec!["ONELF_T_EMPTY_a2"],
            value: "/new".into(),
            inheritance: Inheritance::Preserve,
        };
        let replaced = PathAssignment {
            variables: vec!["ONELF_T_OLD_a3"],
            value: "/new".into(),
            inheritance: Inheritance::Replace,
        };
        let filled = PathAssignment {
            variables: vec!["ONELF_T_FILL_a4"],
            value: "/new".into(),
            inheritance: Inheritance::Replace,
        };

        apply_path_assignments(&[kept, empty, replaced, filled]);

        assert_eq!(env::var("ONELF_T_KEEP_a1").as_deref(), Ok("/kept"));
        assert_eq!(env::var("ONELF_T_EMPTY_a2").as_deref(), Ok(""));
        assert_eq!(env::var("ONELF_T_OLD_a3").as_deref(), Ok("/new"));
        assert_eq!(env::var("ONELF_T_FILL_a4").as_deref(), Ok("/new"));
        unsafe {
            for name in [
                "ONELF_T_KEEP_a1",
                "ONELF_T_EMPTY_a2",
                "ONELF_T_OLD_a3",
                "ONELF_T_FILL_a4",
            ] {
                env::remove_var(name);
            }
        }
    }
}

#[cfg(test)]
mod vulkan_tests {
    use super::*;

    fn icd(dir: &Path, file: &str, library: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(file);
        std::fs::write(
            &path,
            format!("{{\"ICD\": {{\"library_path\": \"{library}\"}}}}"),
        )
        .unwrap();
        path
    }

    /// A driver the package and the host both describe is listed once,
    /// from whichever side the resolver chose, and a host loader with no
    /// such driver is left to its own search.
    #[test]
    fn one_icd_per_driver_and_the_loaders_search_otherwise() {
        let work = env::temp_dir().join(format!("onelf-vk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let (pkg, host) = (work.join("pkg"), work.join("host"));
        let pkg_radeon = icd(&pkg, "radeon_icd.json", "libvulkan_radeon.so");
        let host_radeon = icd(&host, "radeon_icd.json", "/usr/lib/libvulkan_radeon.so");
        let nvidia = icd(&host, "nvidia_icd.json", "libGLX_nvidia.so.0");
        let (packaged, outside) = ([pkg.clone()], [host.clone(), pkg.clone()]);

        let bundled_radeon = |d: &str| d == "libvulkan_radeon.so";
        assert_eq!(
            vulkan_driver_files(&packaged, &outside, true, bundled_radeon),
            Some(vec![pkg_radeon.clone(), nvidia.clone()])
        );
        assert_eq!(
            vulkan_driver_files(&packaged, &outside, true, |_| false),
            Some(vec![nvidia.clone(), host_radeon])
        );

        let host_radeon_32 = icd(
            &host,
            "radeon_icd.i686.json",
            "/usr/lib32/libvulkan_radeon.so",
        );
        assert_eq!(
            vulkan_driver_files(&packaged, &outside, true, bundled_radeon),
            Some(vec![pkg_radeon.clone(), nvidia.clone()])
        );
        std::fs::remove_file(&host_radeon_32).unwrap();

        std::fs::remove_file(&pkg_radeon).unwrap();
        let lvp = icd(&pkg, "lvp_icd.json", "libvulkan_lvp.so");
        assert_eq!(
            vulkan_driver_files(&packaged, &outside, true, |_| true),
            None
        );
        assert_eq!(
            vulkan_driver_files(&packaged, &outside, false, |_| true).map(|f| f.len()),
            Some(3)
        );
        assert!(
            vulkan_driver_files(&packaged, &outside, false, |_| true)
                .unwrap()
                .starts_with(&[lvp])
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// A bundled loader is given what the host's loader would search,
    /// `/usr/local` and the XDG directories included.
    #[test]
    fn the_search_follows_the_xdg_directories() {
        let vars = |name: &str| match name {
            "HOME" => Some("/home/a".into()),
            "XDG_DATA_DIRS" => Some("/pkg/share:/usr/local/share:/usr/share".into()),
            _ => None,
        };
        let dirs: Vec<String> = loader_search_dirs(vars)
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            dirs,
            [
                "/home/a/.config/vulkan/icd.d",
                "/etc/xdg/vulkan/icd.d",
                "/etc/vulkan/icd.d",
                "/home/a/.local/share/vulkan/icd.d",
                "/pkg/share/vulkan/icd.d",
                "/usr/local/share/vulkan/icd.d",
                "/usr/share/vulkan/icd.d",
                "/usr/share/vulkan/icd.d",
                "/run/opengl-driver/share/vulkan/icd.d",
            ]
        );
    }
}

#[cfg(test)]
mod locale_tests {
    use super::*;

    /// The package's locales are named only for a host without an
    /// archive.
    #[test]
    fn locpath_only_without_a_host_archive() {
        let work = env::temp_dir().join(format!("onelf-locpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let (host, lib) = (work.join("host/usr/lib/locale"), work.join("pkg/lib"));
        std::fs::create_dir_all(lib.join("locale/C.utf8")).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        let libs = [lib.clone(), work.join("pkg/lib/x86_64-linux-gnu")];

        assert_eq!(
            locale_path(&libs, &host),
            Some(lib.join("locale").into_os_string())
        );

        std::fs::write(host.join("locale-archive"), "").unwrap();
        assert_eq!(locale_path(&libs, &host), None);

        let _ = std::fs::remove_dir_all(&work);
    }

    /// The development view's mapping evaluation matches the launch's
    /// own: an application root with a Qt plugin directory yields the
    /// aliases the catalog binds to it naming the application's
    /// directory, and a set root contributes its directory after the
    /// application's.
    #[test]
    fn a_dev_view_evaluates_the_catalog_mappings_like_a_launch() {
        let work = std::env::temp_dir().join(format!("onelf-vieweval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let (pkg, set) = (work.join("pkg"), work.join("set"));
        std::fs::create_dir_all(pkg.join("lib/qt6/plugins")).unwrap();
        std::fs::create_dir_all(set.join("lib/qt6/plugins")).unwrap();

        let alone = view_env_assignments(&pkg, &["lib"], &[], &[]);
        let hit = alone
            .iter()
            .find(|a| a.variables.contains(&"QT_PLUGIN_PATH"))
            .expect("the application's Qt mapping");
        assert_eq!(hit.value, pkg.join("lib/qt6/plugins"));

        let set_libs = crate::sets::lib_dirs(&set);
        let both = view_env_assignments(&pkg, &["lib"], &set_libs, std::slice::from_ref(&set));
        let hit = both
            .iter()
            .find(|a| a.variables.contains(&"QT_PLUGIN_PATH"))
            .expect("the set's Qt mapping");
        assert_eq!(
            hit.value,
            std::ffi::OsString::from(format!(
                "{}:{}",
                pkg.join("lib/qt6/plugins").display(),
                set.join("lib/qt6/plugins").display()
            ))
        );

        let _ = std::fs::remove_dir_all(&work);
    }
}

#[cfg(test)]
mod ca_bundle_tests {
    use super::*;

    /// The package's bundle stands in only for a host without one.
    #[test]
    fn only_a_host_without_a_bundle_gets_the_packages() {
        let work = env::temp_dir().join(format!("onelf-ca-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let (host, pkg) = (work.join("host"), work.join("pkg"));
        let carried = pkg.join("etc/ssl/certs/ca-certificates.crt");
        std::fs::create_dir_all(carried.parent().unwrap()).unwrap();
        std::fs::write(&carried, "pem").unwrap();
        std::fs::create_dir_all(host.join("etc/ssl")).unwrap();

        assert_eq!(fallback_ca_bundle(&host, &[&pkg]), Some(carried));

        std::fs::write(host.join("etc/ssl/cert.pem"), "pem").unwrap();
        assert_eq!(fallback_ca_bundle(&host, &[&pkg]), None);

        let _ = std::fs::remove_dir_all(&work);
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

#[cfg(test)]
mod view_env_tests {
    use super::{HOST_ENV_VAR, ROOTS_VAR, VIEW_ENV_HEADER, encode_view_env};
    use crate::env::PathAssignment;
    use onelf_format::rules::Inheritance;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(*name), OsString::from(*value)))
            .collect()
    }

    fn assignments<'a>(items: &[(&'a str, &'a str, Inheritance)]) -> Vec<PathAssignment<'a>> {
        items
            .iter()
            .map(|(name, value, inheritance)| PathAssignment {
                variables: vec![name],
                value: (*value).into(),
                inheritance: *inheritance,
            })
            .collect()
    }

    /// The one entry line the encoder writes for these fields.
    fn expected_entry(name: &str, policy: char, value: &str, left_set: bool) -> String {
        let marker = if left_set { '+' } else { '-' };
        format!("{name}={policy}{}:{value}{marker};\n", value.len())
    }

    #[test]
    fn the_generated_input_lists_changed_and_mapped_variables() {
        let base = map(&[
            ("UNCHANGED", "same"),
            ("MAPPED", "caller"),
            ("CHANGED", "old"),
        ]);
        let now = map(&[
            ("UNCHANGED", "same"),
            ("MAPPED", "caller"),
            ("CHANGED", "new"),
            ("HOOKOUT", "/private/font.conf"),
            (HOST_ENV_VAR, "rec"),
            (ROOTS_VAR, "/view"),
        ]);
        let maps = assignments(&[("MAPPED", "/view/plugins", Inheritance::Preserve)]);
        let bytes = encode_view_env(&base, &now, &maps).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ONELF-RUNTIME-ENV 1 3\n"));
        assert!(text.contains(&expected_entry("MAPPED", '.', "/view/plugins", true)));
        assert!(text.contains(&expected_entry("CHANGED", '!', "new", true)));
        assert!(text.contains(&expected_entry("HOOKOUT", '!', "/private/font.conf", true)));
        assert!(!text.contains("UNCHANGED"));
        assert!(!text.contains("ONELF_HOST_ENV="));
        assert!(text.ends_with("R5:/view;\nH3:rec;\n"));
    }

    #[test]
    fn an_unchanged_launch_still_records_its_mappings() {
        let env_state = map(&[("MAPPED", "caller"), ("OTHER", "same")]);
        let maps = assignments(&[("MAPPED", "/view/plugins", Inheritance::Replace)]);
        let bytes = encode_view_env(&env_state, &env_state, &maps).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ONELF-RUNTIME-ENV 1 1\n"));
        assert!(text.contains(&expected_entry("MAPPED", '!', "/view/plugins", true)));
        assert!(!text.contains("OTHER"));

        let plain = map(&[("OTHER", "same")]);
        let bytes = encode_view_env(&plain, &plain, &[]).unwrap();
        assert!(bytes.starts_with(VIEW_ENV_HEADER));
        assert!(bytes.ends_with(b"0\n"));
    }

    #[test]
    fn a_value_beyond_the_per_entry_bounds_is_skipped() {
        let base = map(&[]);
        let now = map(&[
            ("HUGE", &"x".repeat(40_000)),
            (&"L".repeat(200), "value"),
            ("KEPT", "fine"),
        ]);
        let bytes = encode_view_env(&base, &now, &[]).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ONELF-RUNTIME-ENV 1 1\n"));
        assert!(text.contains(&expected_entry("KEPT", '!', "fine", true)));
        assert!(!text.contains("HUGE"));

        let huge = "x".repeat(40_000);
        let maps = assignments(&[
            ("MAPPED_HUGE", huge.as_str(), Inheritance::Replace),
            ("MAPPED_KEPT", "/view/plugins", Inheritance::Replace),
        ]);
        let bytes = encode_view_env(&base, &base, &maps).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ONELF-RUNTIME-ENV 1 1\n"), "{text}");
        assert!(text.contains(&expected_entry("MAPPED_KEPT", '!', "/view/plugins", false)));
        assert!(!text.contains("MAPPED_HUGE"));
    }

    /// The encoder's output for a fixed launch is the byte-level golden
    /// record the payload constructor's parser is tested against, so the
    /// writer and the reader cannot drift apart unnoticed.
    #[test]
    fn the_encoded_input_matches_the_golden_record_the_constructor_parses() {
        let base = map(&[("MAPPED_KEEP", "/caller/plugins"), ("CHANGED", "old")]);
        let now = map(&[
            ("MAPPED_KEEP", "/caller/plugins"),
            ("MAPPED_LINE", "/view/a\n/view/b"),
            ("CHANGED", "new"),
            (ROOTS_VAR, "/view:/set"),
            (HOST_ENV_VAR, "CHANGED=0123456789abcdef3:old;"),
        ]);
        let maps = assignments(&[
            ("MAPPED_KEEP", "/view/plugins", Inheritance::Preserve),
            ("MAPPED_LINE", "/view/a\n/view/b", Inheritance::Replace),
            ("MAPPED_NONE", "/view/none", Inheritance::Preserve),
        ]);
        let bytes = encode_view_env(&base, &now, &maps).unwrap();
        let golden: &[u8] = include_bytes!("../tests/fixtures/runtime-env.golden");
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            String::from_utf8_lossy(golden)
        );
    }

    #[test]
    fn an_input_beyond_the_total_cap_is_refused_not_truncated() {
        let base = BTreeMap::new();
        let mut now = BTreeMap::new();
        for i in 0..4000u32 {
            now.insert(
                OsString::from(format!("V{i:04}")),
                OsString::from(format!("{:0300}", "")),
            );
        }
        let got = encode_view_env(&base, &now, &[]);
        assert!(matches!(got, Err(super::ViewEnvError::TooLarge)));
    }
}

#[cfg(test)]
mod carried_rule_tests {
    use super::*;
    use onelf_format::rules::{RelativePath, RuleId, RuntimeCandidate, RuntimeRules};
    use onelf_format::runtime_metadata::{RuntimeRuleRecord, SetPin};

    fn work(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("onelf-carried-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn group(
        id: &str,
        variable: &str,
        kind: ResourceKind,
        candidates: &[(RuntimeBase, &str)],
    ) -> RuntimePathGroup {
        RuntimePathGroup {
            id: RuleId::new(id).unwrap(),
            kind,
            candidates: candidates
                .iter()
                .map(|(base, path)| RuntimeCandidate {
                    base: *base,
                    path: RelativePath::new(path).unwrap(),
                })
                .collect(),
            variables: vec![EnvironmentName::new(variable).unwrap()],
            selection: PathSelection::First,
            inheritance: Inheritance::Replace,
        }
    }

    fn record(
        id: &str,
        binding: Option<SetPin>,
        paths: Vec<RuntimePathGroup>,
        hooks: Vec<RuntimeHookId>,
    ) -> RuntimeRuleRecord {
        RuntimeRuleRecord {
            id: RuleId::new(id).unwrap(),
            binding,
            runtime: RuntimeRules { paths, hooks },
        }
    }

    fn values(assignments: &[PathAssignment<'_>]) -> Vec<(String, String)> {
        assignments
            .iter()
            .flat_map(|a| {
                a.variables
                    .iter()
                    .map(|v| (v.to_string(), a.value.to_string_lossy().into_owned()))
            })
            .collect()
    }

    /// A carried record replaces the built-in catalog whole: its own
    /// groups map and its hooks are the only ones a launch runs, while a
    /// built-in group the record leaves out maps nothing, however
    /// present its resource is. Absent, the built-ins and every hook
    /// apply.
    #[test]
    fn a_carried_record_replaces_the_builtins_whole() {
        let pkg = work("replace");
        std::fs::create_dir_all(pkg.join("share/alsa")).unwrap();
        std::fs::create_dir_all(pkg.join("share/vendor/data")).unwrap();
        let lib = [pkg.join("lib")];
        let carried = RuntimeRuleMetadata {
            rules: vec![record(
                "vendor",
                None,
                vec![group(
                    "data",
                    "VENDOR_DATA_DIR",
                    ResourceKind::Directory,
                    &[(RuntimeBase::PackageRoot, "share/vendor/data")],
                )],
                vec![RuntimeHookId::FontconfigGenerate],
            )],
        };

        let active = ActiveRules::new(Some(&carried), &pkg, &[]);
        assert_eq!(
            values(&active.assignments(&pkg, &lib, &[], &[])),
            [(
                "VENDOR_DATA_DIR".to_string(),
                pkg.join("share/vendor/data").to_string_lossy().into_owned()
            )]
        );
        assert_eq!(active.hooks, [RuntimeHookId::FontconfigGenerate]);

        let fallback = ActiveRules::new(None, &pkg, &[]);
        let mapped = values(&fallback.assignments(&pkg, &lib, &[], &[]));
        assert!(mapped.iter().any(|(name, _)| name == "ALSA_CONFIG_DIR"));
        assert!(!mapped.iter().any(|(name, _)| name == "VENDOR_DATA_DIR"));
        assert_eq!(fallback.hooks, all_runtime_hooks());
        let _ = std::fs::remove_dir_all(&pkg);
    }

    /// A record bound to a pin adds that pin's prepared subtree to its
    /// rule's group only while the pin is active. The application's own
    /// resource wins first, the subtree answers before the raw set root
    /// even when the set holds an earlier candidate, and an inactive or
    /// differently pinned build of the same set leaves the subtree
    /// dormant.
    #[test]
    fn a_bound_record_activates_only_for_its_pin_ahead_of_the_raw_set() {
        let work = work("bound");
        let pkg = work.join("pkg");
        let set = work.join("set");
        let pin = SetPin {
            name: RuleId::new("toolkit").unwrap(),
            hash: [0xab; 32],
        };
        let prepared = pkg.join(pin.prepared_dir());
        touch(&prepared.join("libexec/tool"));
        touch(&set.join("lib/tool"));
        let tool = || {
            group(
                "tool",
                "VENDOR_TOOL",
                ResourceKind::File,
                &[
                    (RuntimeBase::LibraryRoot, "tool"),
                    (RuntimeBase::PackageRoot, "libexec/tool"),
                ],
            )
        };
        let carried = RuntimeRuleMetadata {
            rules: vec![
                record("vendor", None, vec![tool()], Vec::new()),
                record("vendor", Some(pin.clone()), vec![tool()], Vec::new()),
            ],
        };
        let lib = [pkg.join("lib")];
        let set_libs = [set.join("lib")];
        let roots = [set.clone()];
        let chosen = |pins: &[ActivePin]| {
            let active = ActiveRules::new(Some(&carried), &pkg, pins);
            values(&active.assignments(&pkg, &lib, &set_libs, &roots))
                .into_iter()
                .map(|(_, value)| value)
                .collect::<Vec<_>>()
        };
        let selected = ActivePin {
            label: "toolkit".to_string(),
            hash: "ab".repeat(32),
        };
        let other = ActivePin {
            label: "toolkit".to_string(),
            hash: "cd".repeat(32),
        };
        let raw = set.join("lib/tool").to_string_lossy().into_owned();

        assert_eq!(
            chosen(std::slice::from_ref(&selected)),
            [prepared.join("libexec/tool").to_string_lossy().into_owned()]
        );
        assert_eq!(chosen(&[other]), std::slice::from_ref(&raw));
        assert_eq!(chosen(&[]), [raw]);
        let active = ActiveRules::new(Some(&carried), &pkg, std::slice::from_ref(&selected));
        let (libs, hook_roots) = active.with_prepared(&pkg, &lib, &set_libs, &roots);
        assert_eq!(hook_roots, [prepared.clone(), set.clone()]);
        assert_eq!(libs, [prepared.join("lib"), set.join("lib")]);

        touch(&pkg.join("lib/tool"));
        assert_eq!(
            chosen(&[selected]),
            [pkg.join("lib/tool").to_string_lossy().into_owned()]
        );
        let _ = std::fs::remove_dir_all(&work);
    }
}

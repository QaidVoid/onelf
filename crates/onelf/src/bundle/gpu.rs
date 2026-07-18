//! GPU / Wayland / GTK framework bundling: Mesa GL/EGL/GBM libraries, DRI
//! and Vulkan drivers with their vendor JSON configs, Wayland client
//! libraries, and GSettings schema compilation.

use super::*;

// ---------------------------------------------------------------------------
// GPU asset bundling
// ---------------------------------------------------------------------------

const DRI_SEARCH_PATHS: &[&str] = &[
    "/usr/lib/dri",
    "/usr/lib64/dri",
    "/usr/lib/x86_64-linux-gnu/dri",
    "/usr/lib/aarch64-linux-gnu/dri",
    "/usr/lib/arm-linux-gnueabihf/dri",
];

const GBM_SEARCH_PATHS: &[&str] = &[
    "/usr/lib/gbm",
    "/usr/lib64/gbm",
    "/usr/lib/x86_64-linux-gnu/gbm",
    "/usr/lib/aarch64-linux-gnu/gbm",
    "/usr/lib/arm-linux-gnueabihf/gbm",
];

const EGL_SEARCH_PATHS: &[&str] = &["/usr/share/glvnd/egl_vendor.d"];

// `/etc` first so an admin's local ICD manifest wins over the distro default
// when copy_vendor_json deduplicates by filename.
const VK_SEARCH_PATHS: &[&str] = &["/etc/vulkan/icd.d", "/usr/share/vulkan/icd.d"];

/// Bundle GPU drivers and vendor configs so OpenGL/Vulkan/EGL apps work portably.
pub(crate) fn bundle_gpu(
    directory: &Path,
    lib_dir: &Path,
    extra_search: &[PathBuf],
    excludes: &[&str],
    target_class: Option<u8>,
    target_machine: Option<u16>,
    dry_run: bool,
    strip: bool,
    include_gl: bool,
    include_dri: bool,
    include_vulkan: bool,
) -> io::Result<()> {
    eprintln!("{} GPU drivers...", color::bold("Bundling"));

    // The target class/machine are passed in from the --target-respecting
    // scan; only RPATH dirs are collected from the on-disk binaries here.
    let elf_files = find_elf_files(directory);

    // Collect RPATH dirs from the app binaries. These point to the exact
    // library versions the app was built against. On NixOS this ensures we
    // pick DRI drivers from the same Mesa as the bundled libGL.so.
    let mut rpath_dri: Vec<PathBuf> = Vec::new();
    let mut rpath_gbm: Vec<PathBuf> = Vec::new();
    let mut rpath_egl: Vec<PathBuf> = Vec::new();
    let mut rpath_vk: Vec<PathBuf> = Vec::new();
    for elf in &elf_files {
        for rdir in parse_rpaths(elf) {
            let dri = rdir.join("dri");
            if dri.is_dir() && !rpath_dri.contains(&dri) {
                rpath_dri.push(dri);
            }
            let gbm = rdir.join("gbm");
            if gbm.is_dir() && !rpath_gbm.contains(&gbm) {
                rpath_gbm.push(gbm);
            }
            // EGL/Vulkan configs are in share/, which is a sibling of lib/
            if let Some(parent) = rdir.parent() {
                let egl = parent.join("share/glvnd/egl_vendor.d");
                if egl.is_dir() && !rpath_egl.contains(&egl) {
                    rpath_egl.push(egl);
                }
                let vk = parent.join("share/vulkan/icd.d");
                if vk.is_dir() && !rpath_vk.contains(&vk) {
                    rpath_vk.push(vk);
                }
            }
        }
    }

    // RPATH-derived dirs go first so they win over system/store-wide scan.
    // This ensures DRI drivers match the Mesa version the app links against.
    let mut dri_dirs = rpath_dri;
    let mut gbm_dirs = rpath_gbm;
    let mut egl_dirs = rpath_egl;
    let mut vk_dirs = rpath_vk;

    // Then standard system paths
    dri_dirs.extend(DRI_SEARCH_PATHS.iter().map(PathBuf::from));
    gbm_dirs.extend(GBM_SEARCH_PATHS.iter().map(PathBuf::from));
    egl_dirs.extend(EGL_SEARCH_PATHS.iter().map(PathBuf::from));
    vk_dirs.extend(VK_SEARCH_PATHS.iter().map(PathBuf::from));

    // Add extra search paths with dri/ and gbm/ subdirs
    for dir in extra_search {
        let dri = dir.join("dri");
        if dri.is_dir() && !dri_dirs.contains(&dri) {
            dri_dirs.push(dri);
        }
        let gbm = dir.join("gbm");
        if gbm.is_dir() && !gbm_dirs.contains(&gbm) {
            gbm_dirs.push(gbm);
        }
    }

    // NixOS: scan store closures for GPU asset directories (lowest priority)
    let store_paths = if Path::new("/nix/store").is_dir() {
        nix_closure_roots()
    } else {
        Vec::new()
    };
    if !store_paths.is_empty() {
        for sp in &store_paths {
            let sp = PathBuf::from(sp);
            let dri = sp.join("lib/dri");
            if dri.is_dir() && !dri_dirs.contains(&dri) {
                dri_dirs.push(dri);
            }
            let gbm = sp.join("lib/gbm");
            if gbm.is_dir() && !gbm_dirs.contains(&gbm) {
                gbm_dirs.push(gbm);
            }
            let egl = sp.join("share/glvnd/egl_vendor.d");
            if egl.is_dir() && !egl_dirs.contains(&egl) {
                egl_dirs.push(egl);
            }
            let vk = sp.join("share/vulkan/icd.d");
            if vk.is_dir() && !vk_dirs.contains(&vk) {
                vk_dirs.push(vk);
            }
        }
    }

    let lib_dest = directory.join(lib_dir);

    // Collect lib directories that contain DRI drivers - these are the Mesa
    // installation directories. We pull implementation libraries from them.
    let mesa_lib_dirs: Vec<PathBuf> = dri_dirs
        .iter()
        .filter_map(|dri_path| {
            // dri_path is e.g. /nix/store/HASH-mesa/lib/dri -> parent is lib/
            let parent = dri_path.parent()?;
            if parent.is_dir() {
                Some(parent.to_path_buf())
            } else {
                None
            }
        })
        .collect();

    // Search dirs for Mesa impl + glvnd dispatch libs.
    // mesa_lib_dirs first (version-matched), then RPATHs, system paths,
    // NixOS store, and extra dirs so glvnd from a separate package is found.
    let mut gl_search_dirs = mesa_lib_dirs.clone();
    for dir in build_lib_search_dirs(&elf_files, extra_search, &store_paths) {
        if !gl_search_dirs.contains(&dir) {
            gl_search_dirs.push(dir);
        }
    }

    let mut gpu_total_bytes = 0u64;

    // 1. Mesa implementation + glvnd dispatch libraries
    let mut mesa_count = 0;
    if include_gl {
        let all_gl: Vec<&str> = MESA_IMPL_PREFIXES
            .iter()
            .chain(GLVND_PREFIXES.iter())
            .copied()
            .collect();
        let (count, bytes) = copy_prefixed_libs(
            &gl_search_dirs,
            &all_gl,
            &lib_dest,
            target_class,
            target_machine,
            excludes,
            dry_run,
            strip,
        )?;
        mesa_count = count;
        gpu_total_bytes += bytes;
        if count > 0 {
            eprintln!(
                "  {} {} Mesa/glvnd lib(s) ({})",
                verb_str(dry_run),
                count,
                format_size(bytes)
            );
        }
    }

    // Remove conflicting GL libraries the application ships in subdirectories
    // (e.g. an old monolithic Mesa libGL.so) so they don't shadow the glvnd
    // libs above. Only once replacements were actually bundled, so a host
    // without Mesa never strips the app's own GL with nothing to fall back on.
    if include_gl && mesa_count > 0 {
        remove_conflicting_gl_libs(directory, &lib_dest, dry_run);
    }

    // 2. DRI drivers (only with --dri)
    let mut dri_count = 0;
    if include_dri {
        let dri_filter = driver_filter(target_machine, DRI_DRIVERS_X86, DRI_DRIVERS_ARM);
        let dri_dest = lib_dest.join("dri");
        let (count, bytes) = copy_so_dir(
            &dri_dirs,
            &dri_dest,
            target_class,
            target_machine,
            dri_filter,
            excludes,
            dry_run,
            strip,
        )?;
        dri_count = count;
        gpu_total_bytes += bytes;
        if count > 0 {
            eprintln!(
                "  {} {} DRI driver(s) ({})",
                verb_str(dry_run),
                count,
                format_size(bytes)
            );
        }
    }

    // 3. GBM backends (with --gl)
    let mut gbm_count = 0;
    if include_gl {
        let gbm_dest = lib_dest.join("gbm");
        let (count, bytes) = copy_so_dir(
            &gbm_dirs,
            &gbm_dest,
            target_class,
            target_machine,
            None,
            excludes,
            dry_run,
            strip,
        )?;
        gbm_count = count;
        gpu_total_bytes += bytes;
        if count > 0 {
            eprintln!(
                "  {} {} GBM backend(s) ({})",
                verb_str(dry_run),
                count,
                format_size(bytes)
            );
        }
    }

    // 4. EGL vendor configs (with --gl)
    let mut egl_count = 0;
    if include_gl {
        let egl_dest = directory.join("share/glvnd/egl_vendor.d");
        let (count, bytes) = copy_vendor_json(
            &egl_dirs,
            &egl_dest,
            &lib_dest,
            target_class,
            target_machine,
            None,
            excludes,
            dry_run,
        )?;
        egl_count = count;
        gpu_total_bytes += bytes;
        if count > 0 {
            eprintln!(
                "  {} {} EGL vendor config(s) ({})",
                verb_str(dry_run),
                count,
                format_size(bytes)
            );
        }
    }

    // 5. Vulkan ICD configs (only with --vulkan)
    let mut vk_count = 0;
    if include_vulkan {
        let vk_filter = driver_filter(target_machine, VULKAN_DRIVERS_X86, VULKAN_DRIVERS_ARM);
        let vk_dest = directory.join("share/vulkan/icd.d");
        let (count, bytes) = copy_vendor_json(
            &vk_dirs,
            &vk_dest,
            &lib_dest,
            target_class,
            target_machine,
            vk_filter,
            excludes,
            dry_run,
        )?;
        vk_count = count;
        gpu_total_bytes += bytes;
        if count > 0 {
            eprintln!(
                "  {} {} Vulkan ICD config(s) ({})",
                verb_str(dry_run),
                count,
                format_size(bytes)
            );
        }
    }

    // 6. Mesa data files (drirc.d configs and libdrm GPU tables)
    let mut data_count = 0u64;
    if include_gl || include_dri {
        // Find Mesa share directories from the same paths we found DRI drivers
        let share_dirs: Vec<PathBuf> = mesa_lib_dirs
            .iter()
            .filter_map(|lib_dir| {
                // lib_dir is e.g. /nix/store/HASH-mesa/lib -> parent has share/
                lib_dir.parent().map(|p| p.join("share"))
            })
            .filter(|p| p.is_dir())
            .collect();

        // Also check standard system paths
        let mut all_share = share_dirs;
        for path in &["/usr/share", "/usr/local/share"] {
            let p = PathBuf::from(path);
            if p.is_dir() && !all_share.contains(&p) {
                all_share.push(p);
            }
        }

        // Copy drirc.d/
        for share in &all_share {
            let drirc = share.join("drirc.d");
            if drirc.is_dir() {
                let dest = directory.join("share/drirc.d");
                let count = copy_data_dir(&drirc, &dest, dry_run)?;
                data_count += count;
                if count > 0 {
                    break;
                }
            }
        }

        // Copy libdrm/
        for share in &all_share {
            let libdrm = share.join("libdrm");
            if libdrm.is_dir() {
                let dest = directory.join("share/libdrm");
                let count = copy_data_dir(&libdrm, &dest, dry_run)?;
                data_count += count;
                if count > 0 {
                    break;
                }
            }
        }

        if data_count > 0 {
            eprintln!("  {} {} Mesa data file(s)", verb_str(dry_run), data_count);
        }
    }

    let total_count =
        mesa_count + dri_count + gbm_count + egl_count + vk_count + data_count as usize;
    if total_count == 0 {
        eprintln!(
            "  {} no GPU assets found on this system",
            color::bold_red("warning:")
        );
    } else if gpu_total_bytes > 0 {
        eprintln!(
            "  {} {}",
            color::bold("GPU total:"),
            format_size(gpu_total_bytes)
        );
    }

    Ok(())
}

/// Copy `.so` files from source directories into `dest`, filtering by ELF class
/// and optionally by an architecture-specific name allowlist.
/// Returns (files_copied, total_bytes).
fn copy_so_dir(
    src_dirs: &[PathBuf],
    dest: &Path,
    target_class: Option<u8>,
    target_machine: Option<u16>,
    name_filter: Option<&[&str]>,
    excludes: &[&str],
    dry_run: bool,
    strip: bool,
) -> io::Result<(usize, u64)> {
    copy_libs(
        src_dirs,
        dest,
        |name| {
            name.contains(".so")
                && name_filter.is_none_or(|allowed| allowed.iter().any(|a| name.starts_with(a)))
        },
        target_class,
        target_machine,
        excludes,
        dry_run,
        strip,
    )
}

/// Mesa implementation libs loaded via dlopen by libglvnd (not in DT_NEEDED).
const MESA_IMPL_PREFIXES: &[&str] = &[
    "libGLX_mesa.so",
    "libEGL_mesa.so",
    "libglapi.so",
    "libgbm.so",
    "libxatracker.so",
];

/// glvnd dispatch libs. Bundled alongside Mesa to ensure version consistency
/// and to replace any incompatible versions shipped by the app.
const GLVND_PREFIXES: &[&str] = &[
    "libGL.so",
    "libGLX.so",
    "libEGL.so",
    "libGLESv2.so",
    "libOpenGL.so",
    "libGLdispatch.so",
];

/// All GL-related prefixes that should be removed from app subdirectories
/// when --gl replaces them with the system's glvnd/Mesa stack.
const ALL_GL_PREFIXES: &[&str] = &[
    // glvnd dispatch
    "libGL.so",
    "libGLX.so",
    "libEGL.so",
    "libGLESv2.so",
    "libOpenGL.so",
    "libGLdispatch.so",
    // Mesa impl
    "libGLX_mesa.so",
    "libEGL_mesa.so",
    "libglapi.so",
    "libgbm.so",
    "libxatracker.so",
    // utility
    "libGLU.so",
];

/// Remove GL libraries from subdirectories of `directory` that would conflict
/// with the glvnd/Mesa libs we copy into `lib_dest`. Files in `lib_dest`
/// itself are skipped (they get overwritten by copy_prefixed_libs).
fn remove_conflicting_gl_libs(directory: &Path, lib_dest: &Path, dry_run: bool) {
    let lib_dest_canon = fs::canonicalize(lib_dest).unwrap_or_else(|_| {
        // lib_dest may not exist yet; build an absolute path manually
        fs::canonicalize(directory)
            .unwrap_or_else(|_| directory.to_path_buf())
            .join(&lib_dest.strip_prefix(directory).unwrap_or(lib_dest))
    });

    let mut to_remove: Vec<PathBuf> = Vec::new();
    collect_gl_conflicts(directory, &lib_dest_canon, &mut to_remove);

    for path in &to_remove {
        let rel = path.strip_prefix(directory).unwrap_or(path);
        let label = if path.is_symlink() && !path.exists() {
            "dangling symlink"
        } else {
            "conflicts with bundled glvnd"
        };
        eprintln!(
            "  {} {} ({})",
            color::bold_red("Removing"),
            rel.display(),
            label,
        );
        if !dry_run {
            let _ = fs::remove_file(path);
        }
    }
}

/// Recursively find GL-related files and symlinks to remove, skipping
/// files directly in lib_dest (those get overwritten by copy_prefixed_libs).
fn collect_gl_conflicts(dir: &Path, lib_dest_canon: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let is_symlink = path.is_symlink();

        if path.is_dir() && !is_symlink {
            // Always recurse — even into lib_dest so we catch its subdirectories
            collect_gl_conflicts(&path, lib_dest_canon, out);
            continue;
        }

        if !is_symlink && !path.is_file() {
            continue;
        }

        let name = match path.file_name() {
            Some(n) => n.to_string_lossy(),
            None => continue,
        };
        if !ALL_GL_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }

        // Skip files directly in lib_dest (those get overwritten by copy_prefixed_libs)
        if let Some(parent) = path.parent() {
            let parent_canon = fs::canonicalize(parent).unwrap_or(parent.to_path_buf());
            if parent_canon == *lib_dest_canon {
                continue;
            }
        }

        out.push(path);
    }
}

/// Wayland client libraries that may be dlopen'd or version-mismatched.
const WAYLAND_LIB_PREFIXES: &[&str] = &[
    "libwayland-client.so",
    "libwayland-server.so",
    "libwayland-cursor.so",
    "libwayland-egl.so",
    "libdecor-0.so",
    "libxkbcommon.so",
];

/// Bundle Wayland client libraries and libdecor plugins.
pub(crate) fn bundle_wayland(
    directory: &Path,
    lib_dir: &Path,
    extra_search: &[PathBuf],
    excludes: &[&str],
    target_class: Option<u8>,
    target_machine: Option<u16>,
    dry_run: bool,
    strip: bool,
) -> io::Result<()> {
    eprintln!("{} Wayland libraries...", color::bold("Bundling"));

    // Target class/machine are passed in; only search dirs come from the
    // on-disk binaries here.
    let elf_files = find_elf_files(directory);

    let nix_paths = if Path::new("/nix/store").is_dir() {
        nix_closure_roots()
    } else {
        Vec::new()
    };
    let search_dirs = build_lib_search_dirs(&elf_files, extra_search, &nix_paths);
    let lib_dest = directory.join(lib_dir);

    // Copy Wayland libraries
    let (copied, total_bytes) = copy_prefixed_libs(
        &search_dirs,
        WAYLAND_LIB_PREFIXES,
        &lib_dest,
        target_class,
        target_machine,
        excludes,
        dry_run,
        strip,
    )?;
    if copied > 0 {
        eprintln!(
            "  {} {} Wayland lib(s) ({})",
            verb_str(dry_run),
            copied,
            format_size(total_bytes)
        );
    }

    // Copy libdecor plugins from libdecor/plugins-1/ subdirs
    let plugin_dirs: Vec<PathBuf> = search_dirs
        .iter()
        .map(|d| d.join("libdecor/plugins-1"))
        .filter(|d| d.is_dir())
        .collect();

    let plugin_dest = directory.join("share/libdecor/plugins-1");
    let (plugin_count, _) = copy_so_dir(
        &plugin_dirs,
        &plugin_dest,
        target_class,
        target_machine,
        None,
        excludes,
        dry_run,
        strip,
    )?;
    if plugin_count > 0 {
        eprintln!(
            "  {} {} libdecor plugin(s)",
            verb_str(dry_run),
            plugin_count
        );
    }

    if copied == 0 && plugin_count == 0 {
        eprintln!(
            "  {} no Wayland libraries found on this system",
            color::bold_red("warning:")
        );
    }

    Ok(())
}

/// Bundle GSettings compiled schemas so GTK/GLib apps don't crash with
/// "No GSettings schemas are installed on the system".
///
/// Collects `.gschema.xml` files from all discoverable schema directories
/// (system, NixOS store, XDG_DATA_DIRS) and compiles them into a single
/// `gschemas.compiled` using `glib-compile-schemas`.
pub(crate) fn bundle_gtk_data(directory: &Path, dry_run: bool) -> io::Result<()> {
    eprintln!("{} GTK data...", color::bold("Bundling"));

    let dest = directory.join("share/glib-2.0/schemas");
    if dest.join("gschemas.compiled").exists() {
        eprintln!("  {} already present", color::dim("gschemas.compiled"));
        return Ok(());
    }

    // Collect all schema source directories
    let mut schema_dirs: Vec<PathBuf> = Vec::new();

    // Standard paths (non-NixOS distros)
    for path in &[
        "/usr/share/glib-2.0/schemas",
        "/usr/local/share/glib-2.0/schemas",
    ] {
        let p = PathBuf::from(path);
        if p.is_dir() && !schema_dirs.contains(&p) {
            schema_dirs.push(p);
        }
    }

    // NixOS: scan store closures for schema dirs
    if Path::new("/nix/store").is_dir() {
        for sp in &nix_closure_roots() {
            let p = PathBuf::from(sp);
            // Standard layout
            let standard = p.join("share/glib-2.0/schemas");
            if standard.is_dir() && !schema_dirs.contains(&standard) {
                schema_dirs.push(standard);
            }
            // NixOS layout: share/gsettings-schemas/<pkg>/glib-2.0/schemas/
            let gs_dir = p.join("share/gsettings-schemas");
            if gs_dir.is_dir() {
                if let Ok(entries) = fs::read_dir(&gs_dir) {
                    for entry in entries.filter_map(Result::ok) {
                        let schemas = entry.path().join("glib-2.0/schemas");
                        if schemas.is_dir() && !schema_dirs.contains(&schemas) {
                            schema_dirs.push(schemas);
                        }
                    }
                }
            }
        }
    }

    // XDG_DATA_DIRS (including NixOS gsettings-schemas subdirs)
    if let Ok(xdg) = std::env::var("XDG_DATA_DIRS") {
        for dir in xdg.split(':').filter(|d| !d.is_empty()) {
            let schemas = PathBuf::from(dir).join("glib-2.0/schemas");
            if schemas.is_dir() && !schema_dirs.contains(&schemas) {
                schema_dirs.push(schemas);
            }
            let gs_dir = PathBuf::from(dir).join("gsettings-schemas");
            if gs_dir.is_dir() {
                if let Ok(entries) = fs::read_dir(&gs_dir) {
                    for entry in entries.filter_map(Result::ok) {
                        let schemas = entry.path().join("glib-2.0/schemas");
                        if schemas.is_dir() && !schema_dirs.contains(&schemas) {
                            schema_dirs.push(schemas);
                        }
                    }
                }
            }
        }
    }

    if schema_dirs.is_empty() {
        eprintln!(
            "  {} no GSettings schema directories found",
            color::bold_red("warning:")
        );
        return Ok(());
    }

    // Collect all .gschema.xml files into a temp dir, then compile. The name
    // is per-process so it can never collide with (and delete) pre-existing
    // app data, and concurrent bundle runs don't clobber each other.
    let tmp = directory.join(format!(".onelf-schemas-tmp-{}", std::process::id()));
    if !dry_run {
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
    }

    let mut xml_count = 0usize;
    let mut seen: HashSet<String> = HashSet::new();
    for schema_dir in &schema_dirs {
        let Ok(entries) = fs::read_dir(schema_dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = match path.file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => continue,
            };
            if !name.ends_with(".gschema.xml")
                && !name.ends_with(".enums.xml")
                && !name.ends_with(".gschema.override")
            {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            if !dry_run {
                fs::copy(&path, tmp.join(&name))?;
            }
            xml_count += 1;
        }
    }

    if xml_count == 0 {
        eprintln!(
            "  {} no .gschema.xml files found",
            color::bold_red("warning:")
        );
        // Only clean up the temp dir we actually created (never in dry-run,
        // where `tmp` was not made and could name pre-existing app data).
        if !dry_run {
            let _ = fs::remove_dir_all(&tmp);
        }
        return Ok(());
    }

    eprintln!(
        "  Collected {} schema XML files from {} source(s)",
        xml_count,
        schema_dirs.len()
    );

    if dry_run {
        eprintln!(
            "  {} compile {} schema files",
            color::bold("Would"),
            xml_count
        );
        return Ok(());
    }

    // Compile schemas (find glib-compile-schemas, may not be in PATH on NixOS)
    let compiler = find_glib_compile_schemas();
    fs::create_dir_all(&dest)?;
    let output = Command::new(&compiler)
        .arg("--targetdir")
        .arg(&dest)
        .arg(&tmp)
        .output();

    let _ = fs::remove_dir_all(&tmp);

    match output {
        Ok(out) if out.status.success() => {
            let size = fs::metadata(dest.join("gschemas.compiled"))
                .map(|m| m.len())
                .unwrap_or(0);
            eprintln!(
                "  {} GSettings schemas ({}, {} sources)",
                color::bold_green("Compiled"),
                format_size(size),
                xml_count
            );
        }
        Ok(out) => {
            eprintln!(
                "  {} glib-compile-schemas failed: {}",
                color::bold_red("error:"),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!(
                "  {} glib-compile-schemas not found: {e}",
                color::bold_red("error:")
            );
            eprintln!("  hint: install glib development tools");
        }
    }

    Ok(())
}

/// Find `glib-compile-schemas` binary. On NixOS it's in glib-dev which may
/// not be in PATH, so we search the nix store.
fn find_glib_compile_schemas() -> PathBuf {
    // Try PATH first
    if let Ok(output) = Command::new("which").arg("glib-compile-schemas").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
    }

    // NixOS: search store for glib-*-dev/bin/glib-compile-schemas. Pick
    // the highest version deterministically (parsed from the store name)
    // instead of whichever read_dir surfaces first.
    if Path::new("/nix/store").is_dir() {
        if let Ok(entries) = fs::read_dir("/nix/store") {
            let mut candidates: Vec<(Vec<u32>, PathBuf)> = Vec::new();
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.contains("glib-") && name.ends_with("-dev") {
                    let candidate = entry.path().join("bin/glib-compile-schemas");
                    if candidate.is_file() {
                        let version = name
                            .rsplit_once("glib-")
                            .and_then(|(_, rest)| rest.strip_suffix("-dev"))
                            .map(|v| v.split('.').filter_map(|p| p.parse().ok()).collect())
                            .unwrap_or_default();
                        candidates.push((version, candidate));
                    }
                }
            }
            // Sort by (version, path); the last is the highest version with
            // a stable path tiebreak.
            candidates.sort();
            if let Some((_, path)) = candidates.pop() {
                return path;
            }
        }
    }

    // Fallback — let Command::new fail with a clear error
    PathBuf::from("glib-compile-schemas")
}

/// Copy vendor JSON configs (EGL or Vulkan ICD), rewriting `library_path` to
/// filename-only and copying the referenced `.so` into `lib_dest`.
/// When `driver_filter` is Some, only copies configs whose library matches
/// the architecture-specific allowlist.
fn copy_vendor_json(
    src_dirs: &[PathBuf],
    json_dest: &Path,
    lib_dest: &Path,
    target_class: Option<u8>,
    target_machine: Option<u16>,
    driver_filter: Option<&[&str]>,
    excludes: &[&str],
    dry_run: bool,
) -> io::Result<(usize, u64)> {
    let mut copied = 0usize;
    let mut total_bytes = 0u64;
    let mut seen: HashSet<String> = HashSet::new();

    for dir in src_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => continue,
            };
            if !name.ends_with(".json") {
                continue;
            }
            // Dedup check only; the name is recorded as handled below, once
            // the entry has passed its filters and is actually copied.
            if seen.contains(&name) {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let (rewritten, so_path) = rewrite_library_path(&content, &path);

            // A vendor JSON's `library_path` is either an absolute path to the
            // implementation `.so` (Vulkan ICDs, NixOS configs) or a bare
            // soname like `libEGL_mesa.so.0` (the Mesa EGL vendor config). In
            // the bare-soname case the path is not resolvable on disk and the
            // library is bundled by the Mesa/glvnd step instead, so we copy the
            // `.so` here only when the JSON points at a real file. We must NOT
            // drop the JSON when it does not: doing so previously skipped the
            // Mesa EGL vendor config entirely (its `read_elf_class` on the
            // unresolvable path returned None, failing the class check), so
            // libglvnd had no vendor to load in a clean environment such as a
            // distrobox container and EGL came up with zero backends.
            if let Some(ref so_src) = so_path {
                let resolved = fs::canonicalize(so_src).unwrap_or_else(|_| so_src.clone());
                let so_name = resolved
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();

                // Architecture-specific driver filter (matches on filename, so
                // it works for both absolute paths and bare sonames).
                if let Some(allowed) = driver_filter
                    && !allowed.iter().any(|a| so_name.starts_with(a))
                {
                    continue;
                }

                // Honor user --exclude patterns for the driver soname.
                if is_excluded(&so_name, excludes) {
                    continue;
                }

                if resolved.is_file() {
                    if let Some(tc) = target_class
                        && read_elf_class(&resolved) != Some(tc)
                    {
                        continue;
                    }
                    if let Some(tm) = target_machine
                        && read_elf_machine(&resolved) != Some(tm)
                    {
                        continue;
                    }
                    let so_size = fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
                    eprintln!(
                        "  {} <- {} ({})",
                        color::bold_green(&so_name),
                        resolved.display(),
                        color::dim(&format_size(so_size))
                    );
                    if !dry_run {
                        fs::create_dir_all(lib_dest)?;
                        let dest_so = lib_dest.join(&so_name);
                        if dest_so.is_symlink() {
                            let _ = fs::remove_file(&dest_so);
                        }
                        // Overwrite unconditionally (like every other copy
                        // path) so a stale driver .so from a previous run
                        // isn't paired with a freshly-written JSON manifest,
                        // but never copy the file onto itself (an $ORIGIN dir
                        // that resolved to the bundle's own lib dir).
                        if !is_same_file(&resolved, &dest_so) {
                            fs::copy(&resolved, &dest_so)?;
                            let _ = fs::set_permissions(&dest_so, PermissionsExt::from_mode(0o755));
                            normalize_mtime(&dest_so);
                        }
                    }
                    total_bytes += so_size;
                }
            }

            // Recorded as handled only now that every filter has passed, so a
            // wrong-arch or excluded config in an earlier directory never
            // shadows a valid same-named one later.
            seen.insert(name.clone());
            eprintln!("  {} <- {}", color::bold_green(&name), path.display());
            if !dry_run {
                fs::create_dir_all(json_dest)?;
                let dest_json = json_dest.join(&name);
                ensure_writable(&dest_json);
                fs::write(&dest_json, &rewritten)?;
            }
            copied += 1;
        }
    }
    Ok((copied, total_bytes))
}

/// Find `"library_path"` in a JSON string and rewrite absolute paths to filename-only.
/// Returns (rewritten_content, Option<resolved_so_path>).
fn rewrite_library_path(content: &str, json_path: &Path) -> (String, Option<PathBuf>) {
    // Match: "library_path" : "some/path"
    // Simple approach: find the key, extract the value, rewrite if absolute
    let key = "\"library_path\"";
    let Some(key_pos) = content.find(key) else {
        return (content.to_string(), None);
    };
    let after_key = &content[key_pos + key.len()..];

    // Skip whitespace and colon
    let after_colon = match after_key.find(':') {
        Some(i) => &after_key[i + 1..],
        None => return (content.to_string(), None),
    };

    // Find opening quote
    let Some(open_quote) = after_colon.find('"') else {
        return (content.to_string(), None);
    };
    let value_start = after_colon[open_quote + 1..].as_ptr() as usize - content.as_ptr() as usize;

    // Find closing quote
    let value_slice = &content[value_start..];
    let Some(close_quote) = value_slice.find('"') else {
        return (content.to_string(), None);
    };

    let lib_path_str = &content[value_start..value_start + close_quote];
    let lib_path = Path::new(lib_path_str);

    // Resolve relative paths against the JSON file's directory
    let resolved = if lib_path.is_absolute() {
        PathBuf::from(lib_path_str)
    } else {
        let dir = json_path.parent().unwrap_or(Path::new("."));
        dir.join(lib_path_str)
    };

    // Canonicalize so the rewritten `library_path` names the file exactly as
    // copy_vendor_json copies it (it canonicalizes too). A versioned symlink
    // (e.g. libGLX_nvidia.so.0 -> ...so.535.x) would otherwise leave the
    // manifest pointing at a filename the bundle doesn't contain. Falls back
    // to the raw path for bare sonames that don't resolve on disk.
    let canonical = fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
    let filename = canonical
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    // Rewrite the content: replace the path with just the filename
    let mut rewritten = String::with_capacity(content.len());
    rewritten.push_str(&content[..value_start]);
    rewritten.push_str(&filename);
    rewritten.push_str(&content[value_start + close_quote..]);

    (rewritten, Some(canonical))
}

/// Copy all files from a data directory into `dest`. Returns number of files copied.
fn copy_data_dir(src: &Path, dest: &Path, dry_run: bool) -> io::Result<u64> {
    let mut count = 0u64;
    let entries = fs::read_dir(src)?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap();
        eprintln!(
            "  {} <- {}",
            color::bold_green(&name.to_string_lossy()),
            path.display()
        );
        if !dry_run {
            fs::create_dir_all(dest)?;
            let dest_path = dest.join(name);
            ensure_writable(&dest_path);
            fs::copy(&path, &dest_path)?;
            let _ = fs::set_permissions(&dest_path, PermissionsExt::from_mode(0o644));
            normalize_mtime(&dest_path);
        }
        count += 1;
    }
    Ok(count)
}

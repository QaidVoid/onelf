//! Shared library bundling for ONELF packages.
//!
//! Scans ELF binaries in a directory for shared library dependencies,
//! resolves them via ldconfig cache, standard paths, or NixOS store
//! scanning, and copies them into a lib directory for self-contained
//! packaging.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

mod color {
    use std::io::IsTerminal;
    use std::sync::OnceLock;

    static ENABLED: OnceLock<bool> = OnceLock::new();

    fn enabled() -> bool {
        *ENABLED.get_or_init(|| {
            std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
        })
    }

    pub fn bold(s: &str) -> String {
        if enabled() {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn red(s: &str) -> String {
        if enabled() {
            format!("\x1b[31m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn cyan(s: &str) -> String {
        if enabled() {
            format!("\x1b[36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn dim(s: &str) -> String {
        if enabled() {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold_green(s: &str) -> String {
        if enabled() {
            format!("\x1b[1;32m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold_red(s: &str) -> String {
        if enabled() {
            format!("\x1b[1;31m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

pub(crate) fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Ensure a file is writable so it can be overwritten on re-runs.
/// No-op if the file doesn't exist yet.
fn ensure_writable(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o200 == 0 {
            let _ = fs::set_permissions(path, PermissionsExt::from_mode(mode | 0o200));
        }
    }
}

fn verb_str(dry_run: bool) -> String {
    if dry_run {
        color::bold("Would copy")
    } else {
        color::bold_green("Copied")
    }
}

/// Build a search path list from RPATH dirs, standard system paths,
/// NixOS store closures, and user-provided extra paths.
fn build_lib_search_dirs(
    elf_files: &[PathBuf],
    extra_search: &[PathBuf],
    nix_store_paths: &[String],
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // RPATH dirs from app binaries (highest priority)
    for elf in elf_files {
        for rdir in parse_rpaths(elf) {
            if rdir.is_dir() && !dirs.contains(&rdir) {
                dirs.push(rdir);
            }
        }
    }

    // Standard system lib paths
    for path in STANDARD_LIB_PATHS {
        let p = PathBuf::from(path);
        if p.is_dir() && !dirs.contains(&p) {
            dirs.push(p);
        }
    }

    // NixOS store lib dirs
    for sp in nix_store_paths {
        let lib = PathBuf::from(sp).join("lib");
        if lib.is_dir() && !dirs.contains(&lib) {
            dirs.push(lib);
        }
    }

    // User-provided extra search paths
    for dir in extra_search {
        if dir.is_dir() && !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }

    dirs
}

/// Copy libraries matching any of `prefixes` (prefix match on filename) from
/// `search_dirs` into `dest`. Resolves symlinks, deduplicates by filename,
/// and filters by ELF class. Returns (files_copied, total_bytes).
fn copy_prefixed_libs(
    search_dirs: &[PathBuf],
    prefixes: &[&str],
    dest: &Path,
    target_class: Option<u8>,
    dry_run: bool,
    strip: bool,
) -> io::Result<(usize, u64)> {
    let mut copied = 0usize;
    let mut total_bytes = 0u64;
    let mut seen: HashSet<String> = HashSet::new();

    for dir in search_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_file() && !path.is_symlink() {
                continue;
            }
            let name = match path.file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => continue,
            };
            if !prefixes.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            let resolved = fs::canonicalize(&path).unwrap_or(path.clone());
            if !resolved.is_file() {
                continue;
            }
            if let Some(tc) = target_class {
                if read_elf_class(&resolved) != Some(tc) {
                    continue;
                }
            }
            let size = fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
            eprintln!(
                "  {} <- {} ({})",
                color::bold_green(&name),
                resolved.display(),
                color::dim(&format_size(size))
            );
            if !dry_run {
                fs::create_dir_all(dest)?;
                let dest_path = dest.join(&name);
                ensure_writable(&dest_path);
                fs::copy(&resolved, &dest_path)?;
                let _ = fs::set_permissions(&dest_path, PermissionsExt::from_mode(0o755));
                if strip {
                    strip_debug(&dest_path);
                }
            }
            copied += 1;
            total_bytes += size;
        }
    }
    Ok((copied, total_bytes))
}

const DEFAULT_EXCLUDES: &[&str] = &[
    "libnss_",
    "libcuda.so",
    "libnvidia",
    "libamdhip64.so",
    "libze_loader.so",
    "linux-vdso.so",
];

const STANDARD_LIB_PATHS: &[&str] = &[
    "/usr/lib",
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib/arm-linux-gnueabihf",
    "/lib",
    "/lib64",
    "/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
];

pub struct BundleOptions {
    pub directory: PathBuf,
    pub target: Option<PathBuf>,
    pub lib_dir: PathBuf,
    pub exclude: Vec<String>,
    pub include: Vec<String>,
    pub search_path: Vec<PathBuf>,
    pub dry_run: bool,
    pub recursive: bool,
    pub gl: bool,
    pub dri: bool,
    pub vulkan: bool,
    pub wayland: bool,
    pub gtk: bool,
    /// Suppress a framework even when auto-detection or an explicit flag
    /// would otherwise enable it. Opt-out wins over both.
    pub no_gl: bool,
    pub no_dri: bool,
    pub no_vulkan: bool,
    pub no_wayland: bool,
    pub no_gtk: bool,
    pub strip: bool,
    pub strict_libc: bool,
    pub scan_dlopen: bool,
    /// Additional sonames added to the dlopen scan allow-list.
    pub dlopen_extra: Vec<String>,
}

/// Strip debug symbols from a shared library (best-effort).
fn strip_debug(path: &Path) {
    match Command::new("strip")
        .arg("--strip-unneeded")
        .arg(path)
        .output()
    {
        Ok(out) if !out.status.success() => {
            eprintln!(
                "  {} strip failed for {}: {}",
                color::bold_red("warning:"),
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!(
                "  {} strip failed for {}: {e}",
                color::bold_red("warning:"),
                path.display()
            );
        }
        _ => {}
    }
}

pub fn bundle_libs(opts: &BundleOptions) -> io::Result<()> {
    if !opts.directory.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{}: not a directory", opts.directory.display()),
        ));
    }

    // Auto-detect frameworks from the input binaries' DT_NEEDED entries.
    // User-provided flags are OR'd with detected flags so explicit opt-ins win
    // but the tool does the right thing when the user passes nothing. A matching
    // no_* opt-out always wins, so a user can drop a framework that detection or
    // an explicit flag would otherwise pull in (e.g. a GUI-capable binary that
    // they only ship as a TUI).
    let detected = detect_frameworks(&opts.directory, opts.target.as_deref());
    let want_gl = (opts.gl || detected.gl) && !opts.no_gl;
    let want_dri = (opts.dri || detected.dri) && !opts.no_dri;
    let want_vulkan = (opts.vulkan || detected.vulkan) && !opts.no_vulkan;
    let want_wayland = (opts.wayland || detected.wayland) && !opts.no_wayland;
    let want_gtk = (opts.gtk || detected.gtk) && !opts.no_gtk;

    // Report frameworks detection turned on that the user did not request,
    // and frameworks the user explicitly suppressed, so the outcome is visible.
    let auto: Vec<&str> = [
        (detected.gl && !opts.gl && want_gl, "gl"),
        (detected.dri && !opts.dri && want_dri, "dri"),
        (detected.vulkan && !opts.vulkan && want_vulkan, "vulkan"),
        (detected.wayland && !opts.wayland && want_wayland, "wayland"),
        (detected.gtk && !opts.gtk && want_gtk, "gtk"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    if !auto.is_empty() {
        eprintln!(
            "  {} auto-enabled: {}",
            color::bold("Frameworks:"),
            auto.join(", ")
        );
    }
    let suppressed: Vec<&str> = [
        (detected.gl && opts.no_gl, "gl"),
        (detected.dri && opts.no_dri, "dri"),
        (detected.vulkan && opts.no_vulkan, "vulkan"),
        (detected.wayland && opts.no_wayland, "wayland"),
        (detected.gtk && opts.no_gtk, "gtk"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    if !suppressed.is_empty() {
        eprintln!(
            "  {} suppressed: {}",
            color::bold("Frameworks:"),
            suppressed.join(", ")
        );
    }

    // Bundle GPU assets first so DRI driver .so files are present when
    // find_elf_files runs, letting the main loop resolve their transitive deps.
    if want_gl || want_dri || want_vulkan {
        bundle_gpu(
            &opts.directory,
            &opts.lib_dir,
            &opts.search_path,
            opts.dry_run,
            opts.strip,
            want_gl,
            want_dri,
            want_vulkan,
        )?;
    }

    if want_wayland {
        bundle_wayland(
            &opts.directory,
            &opts.lib_dir,
            &opts.search_path,
            opts.dry_run,
            opts.strip,
        )?;
    }

    if want_gtk {
        bundle_gtk_data(&opts.directory, opts.dry_run)?;
    }

    let excludes: Vec<&str> = DEFAULT_EXCLUDES
        .iter()
        .copied()
        .chain(opts.exclude.iter().map(|s| s.as_str()))
        .collect();

    let elf_files = if let Some(ref target) = opts.target {
        let path = if target.is_absolute() {
            target.clone()
        } else {
            opts.directory.join(target)
        };
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: not a file", path.display()),
            ));
        }
        vec![path]
    } else {
        find_elf_files(&opts.directory)
    };

    if elf_files.is_empty() {
        eprintln!(
            "{} no ELF files found in {}",
            color::bold_red("warning:"),
            opts.directory.display()
        );
        return Ok(());
    }

    eprintln!(
        "{} {} ELF file(s)...",
        color::bold("Scanning"),
        elf_files.len()
    );

    // Track soname -> first file that requires it (for diagnostics)
    let mut needed_by: HashMap<String, String> = HashMap::new();
    let mut rpath_dirs: Vec<PathBuf> = Vec::new();
    for path in &elf_files {
        let requirer = path
            .strip_prefix(&opts.directory)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        match parse_needed(path) {
            Ok(libs) => {
                for lib in libs {
                    needed_by.entry(lib).or_insert_with(|| requirer.clone());
                }
            }
            Err(e) => {
                eprintln!("warning: {}: {e}", path.display());
            }
        }
        // Also include the ELF interpreter itself. Distros that ship a
        // stub loader (notably NixOS) have a PT_INTERP path that exists
        // but won't actually run foreign binaries, so the runtime needs
        // a real loader in the bundle to sidestep the stub.
        if let Some(interp) = parse_interp(path) {
            if let Some(name) = Path::new(&interp).file_name().and_then(|n| n.to_str()) {
                needed_by
                    .entry(name.to_string())
                    .or_insert_with(|| format!("{requirer} (PT_INTERP)"));
            }
        }
        // Collect RPATH/RUNPATH directories from input binaries
        for dir in parse_rpaths(path) {
            if !rpath_dirs.contains(&dir) {
                rpath_dirs.push(dir);
            }
        }
    }

    // Add explicitly included libs (e.g. dlopen'd libraries)
    for lib in &opts.include {
        needed_by
            .entry(lib.clone())
            .or_insert_with(|| "--include".into());
    }

    // Opt-in dlopen scan: match string literals against a known allow-list
    // of commonly dlopen'd sonames (GL, Wayland, Vulkan, audio, etc.) and
    // queue the hits as if the user had passed them via --include.
    if opts.scan_dlopen {
        let mut scanned: HashSet<String> = HashSet::new();
        for path in &elf_files {
            if let Ok(hits) = scan_dlopen(path, &opts.dlopen_extra) {
                for soname in hits {
                    if scanned.insert(soname.clone()) {
                        let requirer = path
                            .strip_prefix(&opts.directory)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .into_owned();
                        let label = format!("--scan-dlopen in {requirer}");
                        needed_by.entry(soname).or_insert(label);
                    }
                }
            }
        }
        if !scanned.is_empty() {
            eprintln!(
                "  {} {} dlopen candidate(s): {}",
                color::bold("Scanned:"),
                scanned.len(),
                scanned.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
    }

    // Filter excluded
    needed_by.retain(|soname, _| !is_excluded(soname, &excludes));

    // Filter libs already present in the directory tree
    let existing = find_existing_libs(&opts.directory);
    needed_by.retain(|soname, _| !existing.contains(soname));

    if needed_by.is_empty() {
        eprintln!("All dependencies satisfied, nothing to bundle.");
        // PT_INTERP + RUNPATH rewrites still need to run. A prior bundle
        // may have left stale paths (e.g. from an older onelf version)
        // that either don't resolve under the current CWD policy or
        // still rely on LD_LIBRARY_PATH.
        if !opts.dry_run {
            let lib_dest = opts.directory.join(&opts.lib_dir);
            let mut rewritten = 0usize;
            let mut unguaranteed: Vec<PathBuf> = Vec::new();
            let mut self_extract: Vec<PathBuf> = Vec::new();
            for path in find_elf_files(&opts.directory) {
                let perms = fs::metadata(&path)
                    .map(|m| m.permissions().mode())
                    .unwrap_or(0o755);
                let needs_chmod = perms & 0o200 == 0;
                if needs_chmod {
                    let _ = fs::set_permissions(
                        &path,
                        std::os::unix::fs::PermissionsExt::from_mode(perms | 0o200),
                    );
                }
                tally_origin_runpath(&path, &mut rewritten, &mut unguaranteed, &mut self_extract);
                let _ = scrub_nix_store_paths(&path);
                let _ = strip_absolute_needed(&path);
                if needs_chmod {
                    let _ = fs::set_permissions(
                        &path,
                        std::os::unix::fs::PermissionsExt::from_mode(perms),
                    );
                }
            }
            if rewritten > 0 {
                eprintln!(
                    "{} RUNPATH to $ORIGIN/../lib in {} binaries",
                    color::bold("Rewrote"),
                    rewritten
                );
            }
            report_unguaranteed_runpath(&unguaranteed, &self_extract);
            match inject_bootstraps(&opts.directory, &lib_dest) {
                Ok(n) if n > 0 => eprintln!(
                    "{} AT_EXECFN bootstrap into {} binaries",
                    color::bold("Injected"),
                    n
                ),
                Ok(_) => {}
                Err(e) => eprintln!(
                    "{} bootstrap injection failed: {e}",
                    color::bold_red("warning:"),
                ),
            }
        }
        return Ok(());
    }

    // Determine target ELF class (32-bit vs 64-bit) from the input binaries
    let target_class = elf_files.iter().find_map(|f| read_elf_class(f));

    // Determine target libc family from PT_INTERP. Used to skip spurious
    // cross-libc transitive dependencies (e.g. libgcc_s on a glibc host pulls
    // in libc.so.6 + ld-linux, which can't be used by a musl-linked binary).
    let target_libc = elf_files
        .iter()
        .find_map(|f| parse_interp(f).as_deref().and_then(libc_family_from_interp));

    // Drop sonames from the initial queue that belong to the wrong libc family.
    if let Some(target) = target_libc {
        needed_by.retain(|soname, _| libc_family_of_soname(soname).is_none_or(|fam| fam == target));
    }

    let mut ldconfig_cache = build_lib_cache();
    let mut search_paths: Vec<PathBuf> = opts.search_path.clone();
    search_paths.extend(rpath_dirs);
    let lib_dest = opts.directory.join(&opts.lib_dir);

    let mut copied: Vec<(String, PathBuf, u64, String)> = Vec::new();
    let mut not_found: Vec<(String, String)> = Vec::new();
    let mut already_processed: HashSet<String> = HashSet::new();
    let mut expanded_nix: HashSet<PathBuf> = HashSet::new();
    // BLAKE3(content) -> soname, so aliases with identical bytes symlink instead of copy.
    let mut bundled_by_hash: HashMap<[u8; 32], String> = HashMap::new();
    let mut queue: Vec<String> = needed_by.keys().cloned().collect();
    queue.sort();

    // On NixOS: pre-expand cache for libs already in the dest dir from previous runs,
    // so their transitive nix deps are discoverable.
    if Path::new("/nix/store").is_dir() {
        for lib_name in find_existing_libs(&lib_dest) {
            if let Some(src) = locate_lib(&lib_name, &ldconfig_cache, &search_paths, target_class) {
                let resolved = fs::canonicalize(&src).unwrap_or(src);
                expand_nix_cache(&resolved, &mut ldconfig_cache, &mut expanded_nix);
            }
        }
    }

    while let Some(soname) = queue.pop() {
        if already_processed.contains(&soname) || is_excluded(&soname, &excludes) {
            continue;
        }
        already_processed.insert(soname.clone());

        // Skip if already in directory tree (may have been copied in a previous iteration)
        if lib_dest.join(&soname).exists() {
            continue;
        }

        let requirer = needed_by
            .get(&soname)
            .cloned()
            .unwrap_or_else(|| "?".into());

        match locate_lib(&soname, &ldconfig_cache, &search_paths, target_class) {
            Some(src) => {
                let resolved = fs::canonicalize(&src).unwrap_or(src.clone());
                let size = fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
                let dest = lib_dest.join(&soname);

                // Check libc family of this candidate before copying: if it
                // mismatches the target and --strict-libc is set, skip.
                let lib_needed = parse_needed(&resolved).unwrap_or_default();
                let lib_family = lib_needed.iter().find_map(|d| libc_family_of_soname(d));
                let mismatch = matches!(
                    (target_libc, lib_family),
                    (Some(t), Some(f)) if t != f
                );
                if mismatch {
                    let msg = format!(
                        "{} links against {:?} libc but target is {:?}",
                        soname,
                        lib_family.unwrap(),
                        target_libc.unwrap()
                    );
                    if opts.strict_libc {
                        eprintln!(
                            "  {} skipping {} ({})",
                            color::bold_red("skip:"),
                            color::cyan(&soname),
                            msg,
                        );
                        not_found.push((soname.clone(), format!("{requirer} ({msg})")));
                        continue;
                    }
                    eprintln!(
                        "  {} {}; this bundle may not work at runtime",
                        color::bold_red("warning:"),
                        msg,
                    );
                }

                // On NixOS: expand cache with this store path's closure
                // so transitive deps (e.g. libsndfile for libpulsecommon) are found
                expand_nix_cache(&resolved, &mut ldconfig_cache, &mut expanded_nix);

                let content_hash: Option<[u8; 32]> = fs::read(&resolved)
                    .ok()
                    .map(|bytes| blake3::hash(&bytes).into());
                if let Some(hash) = content_hash {
                    if let Some(existing_name) = bundled_by_hash.get(&hash).cloned() {
                        eprintln!(
                            "  {} {} -> {} (alias for {}, {})",
                            color::bold_green("Linked"),
                            soname,
                            existing_name,
                            color::cyan(&requirer),
                            color::dim(&format_size(size))
                        );
                        if !opts.dry_run {
                            fs::create_dir_all(&lib_dest)?;
                            let dest = lib_dest.join(&soname);
                            if dest.exists() || dest.is_symlink() {
                                let _ = fs::remove_file(&dest);
                            }
                            if let Err(e) = std::os::unix::fs::symlink(&existing_name, &dest) {
                                eprintln!(
                                    "  {} failed to symlink {} -> {}: {e}",
                                    color::bold_red("warning:"),
                                    soname,
                                    existing_name
                                );
                            }
                        }
                        continue;
                    }
                }

                eprintln!(
                    "  {} <- {} (needed by {}, {})",
                    color::bold_green(&soname),
                    resolved.display(),
                    color::cyan(&requirer),
                    color::dim(&format_size(size))
                );
                if !opts.dry_run {
                    fs::create_dir_all(&lib_dest)?;
                    ensure_writable(&dest);
                    fs::copy(&resolved, &dest)?;
                    let _ = fs::set_permissions(
                        &dest,
                        std::os::unix::fs::PermissionsExt::from_mode(0o755),
                    );
                    // Strip hardcoded RPATH/RUNPATH so the bundled lib uses
                    // LD_LIBRARY_PATH (set by the runtime) instead of absolute paths
                    if let Err(e) = set_origin_runpath(&dest) {
                        eprintln!(
                            "  {} failed to rewrite RUNPATH of {}: {e}",
                            color::bold_red("warning:"),
                            soname
                        );
                    }
                    // The dynamic loader itself ships with baked-in absolute
                    // paths (ld.so.cache location, preload hook, fallback
                    // library dirs). On the packer's system those resolve to
                    // real files (e.g. /nix/store/.../glibc/etc/ld.so.cache);
                    // on someone else's system they're dead paths at best and
                    // wrong-content paths at worst. Scrub before shipping.
                    if is_dynamic_loader(&soname) {
                        if let Err(e) = scrub_loader_paths(&dest) {
                            eprintln!(
                                "  {} failed to scrub loader paths in {}: {e}",
                                color::bold_red("warning:"),
                                soname
                            );
                        }
                    }
                    if opts.strip {
                        strip_debug(&dest);
                    }
                }

                if let Some(hash) = content_hash {
                    bundled_by_hash.insert(hash, soname.clone());
                }
                copied.push((soname.clone(), resolved.clone(), size, requirer));

                // Collect RPATHs from resolved lib for transitive dep resolution
                for dir in parse_rpaths(&resolved) {
                    if !search_paths.contains(&dir) {
                        search_paths.push(dir);
                    }
                }

                // Resolve transitive dependencies
                if opts.recursive {
                    for dep in lib_needed {
                        if already_processed.contains(&dep)
                            || is_excluded(&dep, &excludes)
                            || existing.contains(&dep)
                        {
                            continue;
                        }
                        // Target's libc is already queued via its direct NEEDED;
                        // any libc-family transitive is either wrong-family or a redundant alias.
                        if libc_family_of_soname(&dep).is_some() {
                            continue;
                        }
                        needed_by
                            .entry(dep.clone())
                            .or_insert_with(|| soname.clone());
                        queue.push(dep);
                    }
                }
            }
            None => {
                not_found.push((soname, requirer));
            }
        }
    }

    // Summary
    copied.sort_by(|a, b| a.0.cmp(&b.0));
    not_found.sort();

    let total_size: u64 = copied.iter().map(|(_, _, s, _)| s).sum();

    if opts.dry_run {
        eprintln!(
            "\n{} would copy {} libraries ({})",
            color::bold("Dry run:"),
            color::bold_green(&copied.len().to_string()),
            color::bold(&format_size(total_size))
        );
    } else if !copied.is_empty() {
        eprintln!(
            "\n{} {} libraries ({}) to {}",
            color::bold_green("Copied"),
            copied.len(),
            color::bold(&format_size(total_size)),
            lib_dest.display()
        );
    }

    if !not_found.is_empty() {
        eprintln!("\n{} ({})", color::bold_red("Not found"), not_found.len());
        for (lib, requirer) in &not_found {
            eprintln!(
                "  {} {}",
                color::red(lib),
                color::dim(&format!("(needed by {})", color::cyan(requirer)))
            );
        }
    }

    // Ensure each ELF's PT_INTERP basename exists in lib_dest as a file or
    // symlink. On musl the loader is referenced as ld-musl-*.so.1 but bundled
    // as libc.musl-*.so.1 (both are names for the same file on disk); without
    // the alias the kernel can't find the interpreter at runtime.
    if !opts.dry_run {
        let mut interp_names: Vec<String> = elf_files
            .iter()
            .filter_map(|p| {
                parse_interp(p).and_then(|i| {
                    Path::new(&i)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
            })
            .collect();
        interp_names.sort();
        interp_names.dedup();

        for interp_name in interp_names {
            let target = lib_dest.join(&interp_name);
            if target.exists() || target.is_symlink() {
                continue;
            }
            let Some(libc_name) = libc_alias_for(&interp_name) else {
                continue;
            };
            let libc_path = lib_dest.join(&libc_name);
            if !libc_path.exists() {
                continue;
            }
            if let Err(e) = std::os::unix::fs::symlink(&libc_name, &target) {
                eprintln!(
                    "  {} failed to create {} -> {}: {e}",
                    color::bold_red("warning:"),
                    interp_name,
                    libc_name
                );
            } else {
                eprintln!(
                    "  {} {} -> {}",
                    color::bold_green("Linked"),
                    interp_name,
                    libc_name
                );
            }
        }
    }

    // Strip RPATHs from all ELF files in the directory for portability.
    // Hardcoded absolute paths (e.g. /nix/store/...) won't exist on the
    // target system; LD_LIBRARY_PATH (set by the runtime) is used instead.
    if !opts.dry_run {
        let mut rewritten = 0usize;
        let mut scrubbed = 0usize;
        let mut unguaranteed: Vec<PathBuf> = Vec::new();
        let mut self_extract: Vec<PathBuf> = Vec::new();
        for path in find_elf_files(&opts.directory) {
            let perms = fs::metadata(&path)
                .map(|m| m.permissions().mode())
                .unwrap_or(0o755);
            let needs_chmod = perms & 0o200 == 0;
            if needs_chmod {
                let _ = fs::set_permissions(
                    &path,
                    std::os::unix::fs::PermissionsExt::from_mode(perms | 0o200),
                );
            }
            tally_origin_runpath(&path, &mut rewritten, &mut unguaranteed, &mut self_extract);
            let before = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let _ = scrub_nix_store_paths(&path);
            let _ = strip_absolute_needed(&path);
            let after = fs::metadata(&path).and_then(|m| m.modified()).ok();
            if before.is_some() && before != after {
                scrubbed += 1;
            }
            if needs_chmod {
                let _ =
                    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(perms));
            }
        }
        if rewritten > 0 {
            eprintln!(
                "{} RUNPATH to $ORIGIN/../lib in {} binaries",
                color::bold("Rewrote"),
                rewritten
            );
        }
        if scrubbed > 0 {
            eprintln!(
                "{} /nix/store paths in {} binaries",
                color::bold("Scrubbed"),
                scrubbed
            );
        }
        report_unguaranteed_runpath(&unguaranteed, &self_extract);
    }

    // Patch PT_INTERP of every ELF with a bundled loader. This is what
    // makes /proc/self/exe point at the real target after kernel exec
    // (Python's stdlib detection, Electron's ASAR locator, Qt's plugin
    // loader all read /proc/self/exe). The runtime and `onelf run` chdir
    // into the AppDir before exec so the relative path resolves.
    if !opts.dry_run {
        match inject_bootstraps(&opts.directory, &lib_dest) {
            Ok(n) if n > 0 => eprintln!(
                "{} AT_EXECFN bootstrap into {} binaries",
                color::bold("Injected"),
                n
            ),
            Ok(_) => {}
            Err(e) => eprintln!(
                "{} bootstrap injection failed: {e}",
                color::bold_red("warning:"),
            ),
        }
    }

    Ok(())
}

fn find_elf_files(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false) {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if is_elf(&path) {
            result.push(path);
        }
    }
    result
}

fn is_elf(path: &Path) -> bool {
    fs::File::open(path)
        .and_then(|mut f| {
            let mut magic = [0u8; 4];
            io::Read::read_exact(&mut f, &mut magic)?;
            Ok(magic == *b"\x7fELF")
        })
        .unwrap_or(false)
}

/// Read the ELF class (1 = 32-bit, 2 = 64-bit) from a file.
fn read_elf_class(path: &Path) -> Option<u8> {
    let mut f = fs::File::open(path).ok()?;
    let mut header = [0u8; 5];
    io::Read::read_exact(&mut f, &mut header).ok()?;
    if header[0..4] == *b"\x7fELF" {
        Some(header[4])
    } else {
        None
    }
}

/// Read the ELF e_machine field (bytes 18-19, little-endian).
fn read_elf_machine(path: &Path) -> Option<u16> {
    let mut f = fs::File::open(path).ok()?;
    let mut header = [0u8; 20];
    io::Read::read_exact(&mut f, &mut header).ok()?;
    if header[0..4] != *b"\x7fELF" {
        return None;
    }
    Some(u16::from_le_bytes([header[18], header[19]]))
}

const EM_X86_64: u16 = 62;
const EM_386: u16 = 3;
const EM_AARCH64: u16 = 183;
const EM_ARM: u16 = 40;

/// Vulkan driver filenames relevant to x86/x86_64 desktop GPUs.
const VULKAN_DRIVERS_X86: &[&str] = &[
    "libvulkan_intel.so",
    "libvulkan_radeon.so",
    "libvulkan_nouveau.so",
    "libvulkan_lvp.so",
    "libvulkan_virtio.so",
];

/// Vulkan driver filenames relevant to ARM/AArch64 GPUs.
const VULKAN_DRIVERS_ARM: &[&str] = &[
    "libvulkan_panfrost.so",
    "libvulkan_asahi.so",
    "libvulkan_freedreno.so",
    "libvulkan_broadcom.so",
    "libvulkan_powervr_mesa.so",
    "libvulkan_lvp.so",
    "libvulkan_virtio.so",
];

/// DRI driver filenames relevant to x86/x86_64.
const DRI_DRIVERS_X86: &[&str] = &[
    "iris_dri.so",
    "i915_dri.so",
    "i965_dri.so",
    "radeonsi_dri.so",
    "r600_dri.so",
    "r300_dri.so",
    "nouveau_dri.so",
    "swrast_dri.so",
    "kms_swrast_dri.so",
    "vmwgfx_dri.so",
    "virtio_gpu_dri.so",
    "zink_dri.so",
];

/// DRI driver filenames relevant to ARM/AArch64.
const DRI_DRIVERS_ARM: &[&str] = &[
    "panfrost_dri.so",
    "asahi_dri.so",
    "freedreno_dri.so",
    "v3d_dri.so",
    "vc4_dri.so",
    "etnaviv_dri.so",
    "lima_dri.so",
    "tegra_dri.so",
    "swrast_dri.so",
    "kms_swrast_dri.so",
    "virtio_gpu_dri.so",
    "zink_dri.so",
];

/// Get the architecture-specific driver filter list.
/// Returns None for unknown architectures (no filtering).
fn driver_filter(
    machine: Option<u16>,
    x86_list: &'static [&'static str],
    arm_list: &'static [&'static str],
) -> Option<&'static [&'static str]> {
    match machine {
        Some(EM_X86_64) | Some(EM_386) => Some(x86_list),
        Some(EM_AARCH64) | Some(EM_ARM) => Some(arm_list),
        _ => None,
    }
}

#[derive(Default, Debug, Clone, Copy)]
struct FrameworkFlags {
    gl: bool,
    dri: bool,
    vulkan: bool,
    wayland: bool,
    gtk: bool,
}

/// Inspect DT_NEEDED across the input binaries (or the --target if set) and
/// infer which framework bundlers should run. Heuristics track common sonames:
/// the user can still explicitly pass the flags to force any of them on.
fn detect_frameworks(directory: &Path, target: Option<&Path>) -> FrameworkFlags {
    let files = match target {
        Some(t) => {
            let p = if t.is_absolute() {
                t.to_path_buf()
            } else {
                directory.join(t)
            };
            if p.is_file() {
                vec![p]
            } else {
                return FrameworkFlags::default();
            }
        }
        None => find_elf_files(directory),
    };
    let mut flags = FrameworkFlags::default();
    for path in &files {
        if let Ok(needed) = parse_needed(path) {
            for soname in needed {
                inspect_soname_for_frameworks(&soname, &mut flags);
            }
        }

        // Scan for NUL-terminated soname strings in the binary. C/C++
        // apps like Blender don't DT_NEED libwayland-cursor or libdecor;
        // they dlopen them at runtime. This catches those cases for
        // binaries with proper NUL-separated string tables. Rust binaries
        // with merged string sections won't trigger false positives here;
        // users should pass --scan-dlopen or explicit framework flags.
        if let Ok(bytes) = fs::read(path) {
            scan_framework_strings(&bytes, &mut flags);
        }
    }
    flags
}

/// Inspect a single soname (from DT_NEEDED or a dlopen string scan)
/// and turn on whichever framework flags it implies.
fn inspect_soname_for_frameworks(soname: &str, flags: &mut FrameworkFlags) {
    if soname.starts_with("libGL.so")
        || soname.starts_with("libEGL.so")
        || soname.starts_with("libGLESv")
        || soname.starts_with("libOpenGL.so")
    {
        flags.gl = true;
        flags.dri = true;
    }
    if soname.starts_with("libgbm.so") {
        flags.dri = true;
    }
    if soname.starts_with("libvulkan.so") {
        flags.vulkan = true;
    }
    if soname.starts_with("libwayland-client.so")
        || soname.starts_with("libwayland-egl.so")
        || soname.starts_with("libwayland-cursor.so")
        || soname.starts_with("libwayland-server.so")
        || soname.starts_with("libdecor-0.so")
    {
        flags.wayland = true;
    }
    if soname.starts_with("libgtk-3.so")
        || soname.starts_with("libgtk-4.so")
        || soname.starts_with("libgtk-")
    {
        flags.gtk = true;
    }
}

/// Walk the byte buffer looking for library names that would make us
/// enable a framework bundler. Only matches on well-known soname stems
/// to avoid false positives from arbitrary strings in the binary.
///
/// A match must be a *versioned* soname — `lib<name>.so.<digit>...` — to
/// flag a framework. We deliberately do not require a NUL boundary before
/// the match: Rust's string-merging optimization packs string literals
/// together without NUL separators, so genuine dlopen sonames in Rust
/// binaries (e.g. the wgpu/khronos-egl/wayland-backend strings in
/// `amdgpu_top`) appear mid-blob like `...eglWaitSynclibEGL.so.1libEGL.so...`.
/// Requiring the version suffix is precise enough to keep those while still
/// rejecting:
/// - Unversioned soname text in prose (e.g. `"Library libwayland-client.so
///   could not be loaded."` — the `.so` is followed by a space, not `.N`).
/// - The bare `.so` fallback strings dlopen loaders carry alongside the
///   versioned form; the versioned sibling next to them is what we flag.
fn scan_framework_strings(bytes: &[u8], flags: &mut FrameworkFlags) {
    // Library name stems (without the `.so` suffix). `libGL` is a prefix of
    // `libGLESv`/`libOpenGL`, but `versioned_soname_at` reconstructs the full
    // token and `inspect_soname_for_frameworks` classifies it correctly.
    const STEMS: &[&[u8]] = &[
        b"libGL",
        b"libEGL",
        b"libGLESv",
        b"libOpenGL",
        b"libgbm",
        b"libvulkan",
        b"libwayland-client",
        b"libwayland-egl",
        b"libwayland-cursor",
        b"libwayland-server",
        b"libdecor-0",
        b"libgtk-3",
        b"libgtk-4",
    ];
    let mut i = 0;
    while i < bytes.len() {
        // Cheap gate: every stem starts with 'l'.
        if bytes[i] != b'l' {
            i += 1;
            continue;
        }
        if STEMS.iter().any(|stem| bytes[i..].starts_with(stem))
            && let Some(soname) = versioned_soname_at(bytes, i)
        {
            inspect_soname_for_frameworks(&soname, flags);
        }
        i += 1;
    }
}

/// Validate that the bytes at `start` form a versioned soname and return it.
///
/// Consumes soname name characters, then requires a literal `.so` followed by
/// at least one `.<digit>` version component (`.so.1`, `.so.1.2`, ...). Returns
/// the `lib<name>.so.<version>` token on success, or `None` if the shape does
/// not match (unversioned `.so`, prose, or merged-string junk).
fn versioned_soname_at(bytes: &[u8], start: usize) -> Option<String> {
    let mut j = start;
    while j < bytes.len()
        && matches!(bytes[j], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-')
    {
        j += 1;
    }
    if !bytes[j..].starts_with(b".so") {
        return None;
    }
    j += 3;
    // Require `.` then a digit to start the version (rejects `.so` + space/letter).
    if j + 1 >= bytes.len() || bytes[j] != b'.' || !bytes[j + 1].is_ascii_digit() {
        return None;
    }
    while j < bytes.len() && (bytes[j] == b'.' || bytes[j].is_ascii_digit()) {
        j += 1;
    }
    std::str::from_utf8(&bytes[start..j])
        .ok()
        .map(str::to_string)
}

/// Sonames that applications commonly dlopen at runtime. Absence from DT_NEEDED
/// doesn't mean absence from the runtime graph; these are known offenders.
const DLOPEN_CANDIDATES: &[&str] = &[
    // OpenGL / GLVND
    "libGL.so.1",
    "libEGL.so.1",
    "libGLX.so.0",
    "libGLdispatch.so.0",
    "libOpenGL.so.0",
    "libGLESv1_CM.so.1",
    "libGLESv2.so.2",
    "libgbm.so.1",
    // Vulkan
    "libvulkan.so.1",
    // Wayland
    "libwayland-client.so.0",
    "libwayland-cursor.so.0",
    "libwayland-egl.so.1",
    "libdecor-0.so.0",
    // X11
    "libX11.so.6",
    "libxcb.so.1",
    "libxkbcommon.so.0",
    "libxkbcommon-x11.so.0",
    // Video acceleration
    "libva.so.2",
    "libva-drm.so.2",
    "libva-x11.so.2",
    "libva-wayland.so.2",
    // Audio
    "libpulse.so.0",
    "libasound.so.2",
    "libjack.so.0",
    // IPC / desktop
    "libdbus-1.so.3",
    // NVIDIA proprietary stack
    "libcuda.so.1",
    "libnvidia-ml.so.1",
    "libnvidia-encode.so.1",
    "libnvidia-fbc.so.1",
    // Fonts / text
    "libfontconfig.so.1",
    "libfreetype.so.6",
    "libharfbuzz.so.0",
];

/// Scan a binary's string table for soname-shaped values that match the
/// dlopen allow-list (built-in plus any user-supplied additions). Matches
/// are candidates for bundling even though they don't appear in DT_NEEDED.
fn scan_dlopen(path: &Path, extra: &[String]) -> io::Result<Vec<String>> {
    let data = fs::read(path)?;
    let mut found: Vec<String> = Vec::new();

    let mut start = None;
    for (i, &b) in data.iter().enumerate() {
        let printable = (0x20..=0x7e).contains(&b);
        if printable {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            if i - s >= 5 {
                if let Ok(text) = std::str::from_utf8(&data[s..i]) {
                    let match_builtin = DLOPEN_CANDIDATES.iter().any(|c| *c == text);
                    let match_extra = extra.iter().any(|c| c == text);
                    if (match_builtin || match_extra) && !found.iter().any(|x| x == text) {
                        found.push(text.to_string());
                    }
                }
            }
        }
    }
    Ok(found)
}

fn parse_needed(path: &Path) -> io::Result<Vec<String>> {
    let data = fs::read(path)?;
    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    // nixpkgs occasionally emits DT_NEEDED entries as absolute
    // `/nix/store/<hash>/lib/libfoo.so` paths rather than plain
    // sonames. Reduce those to their basename so our resolver can
    // find the lib on the host / search paths. `strip_absolute_needed`
    // rewrites the ELF's own DT_NEEDED string after bundling so the
    // runtime loader also picks up the bundled copy via RUNPATH.
    Ok(elf
        .libraries
        .iter()
        .map(|s| {
            if s.starts_with('/') {
                Path::new(s)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(s)
                    .to_string()
            } else {
                s.to_string()
            }
        })
        .collect())
}

/// Parse PT_INTERP from an ELF binary, returning the interpreter path.
///
/// goblin returns `p_filesz - 1` bytes verbatim, so a slot padded with
/// trailing NULs (common after our in-place PT_INTERP rewrite if the
/// phdr wasn't shrunk) would leak into callers as embedded NULs in
/// the returned string. Trim them so queue lookups and `file_name()`
/// behave correctly.
fn parse_interp(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    let elf = goblin::elf::Elf::parse(&data).ok()?;
    elf.interpreter
        .map(|s| s.trim_end_matches('\0').to_string())
}

/// Map an ELF interpreter basename to the libc filename that serves it.
/// On musl, `ld-musl-<arch>.so.1` and `libc.musl-<arch>.so.1` are both
/// names for the same file. Returns None if no mapping is known.
fn libc_alias_for(interp_name: &str) -> Option<String> {
    interp_name
        .strip_prefix("ld-musl-")
        .map(|rest| format!("libc.musl-{rest}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibcFamily {
    Musl,
    Glibc,
}

/// Detect a binary's libc family from its PT_INTERP basename.
fn libc_family_from_interp(interp: &str) -> Option<LibcFamily> {
    let name = Path::new(interp).file_name()?.to_str()?;
    if name.starts_with("ld-musl-") {
        Some(LibcFamily::Musl)
    } else if name.starts_with("ld-linux") {
        Some(LibcFamily::Glibc)
    } else {
        None
    }
}

/// Map a soname to the libc family it belongs to, when known.
fn libc_family_of_soname(soname: &str) -> Option<LibcFamily> {
    if soname == "libc.so.6" || soname.starts_with("ld-linux") {
        Some(LibcFamily::Glibc)
    } else if soname.starts_with("libc.musl-")
        || soname.starts_with("ld-musl-")
        || soname == "libc.so"
    {
        // libc.so is musl's canonical libc filename; libc.musl-*/ld-musl-* are aliases.
        Some(LibcFamily::Musl)
    } else {
        None
    }
}

/// Parse RPATH and RUNPATH entries from an ELF binary.
fn parse_rpaths(path: &Path) -> Vec<PathBuf> {
    let Ok(data) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(elf) = goblin::elf::Elf::parse(&data) else {
        return Vec::new();
    };
    elf.runpaths
        .iter()
        .chain(elf.rpaths.iter())
        .map(|s| PathBuf::from(s))
        .filter(|p| p.is_absolute() && p.is_dir())
        .collect()
}

/// Rewrite RPATH/RUNPATH to `$ORIGIN/../lib` so the bundled ELF finds its
/// transitive libraries via its own on-disk location, never via
/// `LD_LIBRARY_PATH`. That matters because `LD_LIBRARY_PATH` is a
/// per-process env variable that gets inherited into host binaries the
/// app may spawn (for example, `postgres` uses `popen(3)` which execs
/// `/bin/sh` - a host binary linked against the host's glibc). If we
/// left our bundle dir on `LD_LIBRARY_PATH`, the host shell would load
/// our newer `libc.so.6` against its own older `ld-linux.so.2` and
/// crash with a null deref in the loader. Using `$ORIGIN/../lib` keeps
/// the bundle's library search scoped to the bundled ELF itself.
///
/// First tries in-place patching of an existing DT_RPATH/DT_RUNPATH
/// slot. If the binary has no slot or it's too small for our string
/// (e.g. Bun, Go, Zig outputs), falls back to `patchelf --set-rpath`
/// when available.
///
/// The outcome matters for re-exec safety: an executable that ends up
/// without a baked-in `$ORIGIN` RUNPATH can only find its bundled libs
/// via `LD_LIBRARY_PATH`, which is wiped when the app re-execs itself in
/// a sandbox (`clearenv()` + `execve`). The caller surfaces those so the
/// package isn't silently fragile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunpathOutcome {
    /// `$ORIGIN` RUNPATH is baked into the ELF (in-place or via patchelf).
    Set,
    /// No RUNPATH needed: static binary, bare lib, or no DT_NEEDED.
    NotNeeded,
    /// Executable with deps but RUNPATH could not be guaranteed (no
    /// in-place slot and patchelf missing or failed). Relies on
    /// `LD_LIBRARY_PATH`; not sandbox-re-exec-safe.
    Unguaranteed,
    /// Executable with a self-extract trailer: patchelf would clobber
    /// the trailer, so RUNPATH can't be added. Known limitation.
    SelfExtract,
}

fn set_origin_runpath(path: &Path) -> io::Result<RunpathOutcome> {
    // Cover binaries at depth 1 (e.g. bin/foo), 2 (libexec/podman/x),
    // and 3 (share/pkg/helpers/y). Nonexistent entries are silently
    // ignored by the dynamic loader, so this is safe to apply
    // uniformly without knowing where each ELF sits.
    const NEW: &str = "$ORIGIN/../lib:$ORIGIN/../../lib:$ORIGIN/../../../lib";
    let new_bytes = NEW.as_bytes();
    let data = fs::read(path)?;

    // Self-extracting binaries (e.g. pre-1.3.12 Bun) have a trailer at
    // the end of the file. patchelf can grow the file when adding a
    // missing DT_RUNPATH, which would invalidate the trailer. The
    // in-place rewrite is safe (same file size), so we still attempt
    // that, but we skip the patchelf fallback for these binaries.
    let is_self_extract = has_self_extract_trailer(&data);

    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    // Only meaningful for binaries with dynamic dependencies. A bottom-
    // of-stack lib like libc.so.6 or the dynamic loader has no DT_NEEDED
    // entries and doesn't need DT_RUNPATH itself.
    let has_needed = !elf.libraries.is_empty();
    // PT_INTERP marks an executable; pure shared libraries lack it. We
    // only warn / fall back to patchelf for executables, since libs
    // typically resolve their deps via the executable's DT_RUNPATH.
    // glibc's libc.so.6 (and ld.so) carry PT_INTERP but are libraries,
    // distinguished by a DT_SONAME; exclude anything with a SONAME so
    // they aren't mis-flagged as un-RUNPATH'd app executables.
    let has_soname = elf.soname.is_some();
    let is_executable = !has_soname
        && elf
            .program_headers
            .iter()
            .any(|p| p.p_type == goblin::elf::program_header::PT_INTERP);

    // Find .dynstr section file offset
    let dynstr_offset = elf
        .section_headers
        .iter()
        .find(|sh| elf.shdr_strtab.get_at(sh.sh_name) == Some(".dynstr"))
        .map(|sh| sh.sh_offset as usize);

    let dynamic_present = elf.dynamic.is_some();
    let mut in_place_done = false;

    if let (Some(dynstr_offset), Some(dynamic)) = (dynstr_offset, &elf.dynamic) {
        let mut modified = data.clone();
        for dyn_entry in &dynamic.dyns {
            if dyn_entry.d_tag == goblin::elf::dynamic::DT_RPATH
                || dyn_entry.d_tag == goblin::elf::dynamic::DT_RUNPATH
            {
                let file_pos = dynstr_offset + dyn_entry.d_val as usize;
                if file_pos >= modified.len() {
                    continue;
                }
                let mut end = file_pos;
                while end < modified.len() && modified[end] != 0 {
                    end += 1;
                }
                while end < modified.len() && modified[end] == 0 {
                    end += 1;
                }
                let slot_size = end - file_pos;
                if new_bytes.len() + 1 > slot_size {
                    // Slot too small; will fall back to patchelf below.
                    continue;
                }
                modified[file_pos..file_pos + new_bytes.len()].copy_from_slice(new_bytes);
                for i in new_bytes.len()..slot_size {
                    modified[file_pos + i] = 0;
                }
                in_place_done = true;
            }
        }
        if in_place_done {
            fs::write(path, &modified)?;
            return Ok(RunpathOutcome::Set);
        }
    }

    drop(elf);

    if !dynamic_present || !has_needed {
        // Nothing depends on libs (static binary, libc.so itself, the
        // dynamic loader, etc.). DT_RUNPATH wouldn't help here.
        return Ok(RunpathOutcome::NotNeeded);
    }

    if !is_executable {
        // Shared libraries usually resolve their deps via the
        // executable's DT_RUNPATH (transitive search). Skip patchelf
        // and the noisy warning for bare libs.
        return Ok(RunpathOutcome::NotNeeded);
    }

    if is_self_extract {
        // Don't risk patchelf growing the file and clobbering the
        // self-extract trailer. The runtime still sets LD_LIBRARY_PATH
        // as a fallback for these binaries.
        return Ok(RunpathOutcome::SelfExtract);
    }

    // No usable in-place slot. Fall back to patchelf, which can
    // either resize an existing slot or add a fresh DT_RUNPATH by
    // growing the file's string table.
    if let Some(patchelf) = which_patchelf() {
        let status = std::process::Command::new(&patchelf)
            .arg("--force-rpath")
            .arg("--set-rpath")
            .arg(NEW)
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        match status {
            Ok(o) if o.status.success() => return Ok(RunpathOutcome::Set),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                eprintln!(
                    "  {} patchelf failed for {}: {}",
                    color::bold_red("warning:"),
                    path.display(),
                    stderr.trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "  {} could not run patchelf for {}: {e}",
                    color::bold_red("warning:"),
                    path.display(),
                );
            }
        }
    }
    // No patchelf available and no in-place slot. The runtime still sets
    // LD_LIBRARY_PATH as a fallback for the initial launch, but it won't
    // survive a sandboxed re-exec. The caller reports this.
    Ok(RunpathOutcome::Unguaranteed)
}

/// Apply `set_origin_runpath` and fold the outcome into the running
/// tallies. `set` counts binaries that got a baked-in RUNPATH;
/// `unguaranteed` / `self_extract` collect executables that did not, so
/// the caller can warn that they aren't sandbox-re-exec-safe.
fn tally_origin_runpath(
    path: &Path,
    set: &mut usize,
    unguaranteed: &mut Vec<PathBuf>,
    self_extract: &mut Vec<PathBuf>,
) {
    match set_origin_runpath(path) {
        Ok(RunpathOutcome::Set) => *set += 1,
        Ok(RunpathOutcome::Unguaranteed) => unguaranteed.push(path.to_path_buf()),
        Ok(RunpathOutcome::SelfExtract) => self_extract.push(path.to_path_buf()),
        Ok(RunpathOutcome::NotNeeded) | Err(_) => {}
    }
}

/// Warn that the listed executables could not get a baked-in `$ORIGIN`
/// RUNPATH and therefore won't survive a sandboxed re-exec. Printed once
/// per bundling pass; empty input prints nothing.
fn report_unguaranteed_runpath(unguaranteed: &[PathBuf], self_extract: &[PathBuf]) {
    if !unguaranteed.is_empty() {
        eprintln!(
            "{} {} executable(s) have no baked-in $ORIGIN RUNPATH and rely \
             on LD_LIBRARY_PATH:",
            color::bold_red("warning:"),
            unguaranteed.len()
        );
        for p in unguaranteed {
            eprintln!("  - {}", p.display());
        }
        eprintln!(
            "  These break if the app re-execs itself in a sandbox \
             (clearenv). Install `patchelf` (or set ONELF_PATCHELF) and \
             repack to make them re-exec-safe."
        );
    }
    if !self_extract.is_empty() {
        eprintln!(
            "{} {} self-extracting executable(s) can't take a baked-in \
             RUNPATH (would clobber the embedded payload):",
            color::bold_red("warning:"),
            self_extract.len()
        );
        for p in self_extract {
            eprintln!("  - {}", p.display());
        }
        eprintln!(
            "  These rely on the runtime's LD_LIBRARY_PATH and are not \
             sandbox-re-exec-safe."
        );
    }
}

/// Detect binaries that embed a self-extracting payload at the end of
/// the file. Bootstrap injection appends to the file, which would clobber
/// such payloads and prevent runtime detection.
///
/// Currently detects: pre-1.3.12 Bun (`bun build --compile`) binaries
/// which end with `\n---- Bun! ----\n` followed by an 8-byte length.
fn has_self_extract_trailer(data: &[u8]) -> bool {
    // Bun's trailer is 16 bytes; pre-1.3.12 also has an 8-byte length
    // word after it (so check at offsets -16 and -24). Modern Bun
    // (>=1.3.12) uses a `.bun` ELF section instead and is unaffected.
    const BUN_TRAILER: &[u8] = b"\n---- Bun! ----\n";
    if data.len() >= BUN_TRAILER.len() && data.ends_with(BUN_TRAILER) {
        return true;
    }
    if data.len() >= BUN_TRAILER.len() + 8
        && &data[data.len() - BUN_TRAILER.len() - 8..data.len() - 8] == BUN_TRAILER
    {
        return true;
    }
    false
}

/// Locate patchelf in PATH (or ONELF_PATCHELF override).
fn which_patchelf() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ONELF_PATCHELF") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = PathBuf::from(dir).join("patchelf");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Rewrite any absolute-path DT_NEEDED entry to just its basename. The
/// pack host's nixpkgs stack sometimes emits a full
/// `/nix/store/<hash>-name/lib/libfoo.so` as the DT_NEEDED string. The
/// dynamic loader treats those literally and ignores `RUNPATH` /
/// `LD_LIBRARY_PATH`, so a binary built with them will try to `open`
/// that exact path on the user's machine and fail. Stripping to the
/// basename puts the lookup back on the standard search path and
/// picks up our bundled copy via `$ORIGIN/../lib`.
///
/// Operates in place: writes the new basename over the old string and
/// NUL-pads the rest of the slot. The old slot is always longer than
/// the new basename, so this never needs to grow the string table.
fn strip_absolute_needed(path: &Path) -> io::Result<()> {
    let data = fs::read(path)?;
    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let dynstr_offset = elf
        .section_headers
        .iter()
        .find(|sh| elf.shdr_strtab.get_at(sh.sh_name) == Some(".dynstr"))
        .map(|sh| sh.sh_offset as usize);
    let Some(dynstr_offset) = dynstr_offset else {
        return Ok(());
    };
    let Some(dynamic) = &elf.dynamic else {
        return Ok(());
    };

    let mut modified = data;
    let mut changed = false;

    for dyn_entry in &dynamic.dyns {
        if dyn_entry.d_tag != goblin::elf::dynamic::DT_NEEDED {
            continue;
        }
        let file_pos = dynstr_offset + dyn_entry.d_val as usize;
        if file_pos >= modified.len() || modified[file_pos] != b'/' {
            continue;
        }
        // Read the current (absolute) path from the string table.
        let mut end = file_pos;
        while end < modified.len() && modified[end] != 0 {
            end += 1;
        }
        let original = &modified[file_pos..end];
        let slot_size = end - file_pos;

        let basename_start = match original.iter().rposition(|&b| b == b'/') {
            Some(p) => p + 1,
            None => 0,
        };
        let basename_len = original.len() - basename_start;
        if basename_len == 0 || basename_len >= slot_size {
            continue;
        }

        let basename: Vec<u8> = original[basename_start..].to_vec();
        modified[file_pos..file_pos + basename_len].copy_from_slice(&basename);
        for i in basename_len..slot_size {
            modified[file_pos + i] = 0;
        }
        changed = true;
    }

    if changed {
        fs::write(path, &modified)?;
    }
    Ok(())
}

fn is_excluded(soname: &str, excludes: &[&str]) -> bool {
    excludes.iter().any(|pat| soname.starts_with(pat))
}

/// True for sonames that denote the dynamic linker itself.
fn is_dynamic_loader(soname: &str) -> bool {
    soname.starts_with("ld-linux") || soname.starts_with("ld-musl-") || soname == "ld.so"
}

/// Rewrite absolute-path byte sequences baked into the dynamic loader.
///
/// glibc's `ld-linux` hardcodes its build-time `/etc/ld.so.cache`,
/// `/etc/ld-nix.so.preload`, `/nix/store/<hash>-glibc-X/lib/`, and a
/// few other absolute paths. Those exist on the packer's machine but
/// not on the user's; worse, if any do exist, they'll point at a
/// libc that disagrees with the one we bundled. The fix is to replace
/// each prefix with a path that is guaranteed not to resolve (starts
/// with `/XXX`), keeping byte length identical so ELF offsets stay
/// valid.
///
/// This is the same idea as sharun's `sed` pass, done in pure Rust
/// with a more targeted prefix list.
fn scrub_loader_paths(path: &Path) -> io::Result<()> {
    let mut data = fs::read(path)?;
    let mut changed = false;
    // Each pattern and replacement are equal length to avoid any ELF
    // structure shifts. Replacements are paths that simply don't exist
    // on any sane system.
    let replacements: &[(&[u8], &[u8])] = &[
        (b"/etc/", b"/XXX/"),
        (b"/usr/", b"/XXX/"),
        (b"/nix/", b"/XXX/"),
        // /lib/ and /lib64/ appear as glibc's hardcoded fallback
        // library search paths. Our bundled libs live in `lib/` (no
        // leading slash), so scrubbing absolute /lib doesn't hurt.
        (b"/lib/", b"/XXX/"),
        (b"/lib64/", b"/XXX///"),
    ];

    for (needle, replace) in replacements {
        debug_assert_eq!(needle.len(), replace.len());
        let len = needle.len();
        let mut i = 0;
        while i + len <= data.len() {
            if &data[i..i + len] == *needle {
                data[i..i + len].copy_from_slice(replace);
                changed = true;
                i += len;
            } else {
                i += 1;
            }
        }
    }

    if changed {
        fs::write(path, &data)?;
    }
    Ok(())
}

/// Rewrite specific `/nix/store/<hash>-<name>-<version>/...` strings
/// baked into a bundled ELF with sensible host equivalents. Called for
/// every bundled non-loader ELF, not just the loader.
///
/// nixpkgs typically compiles postgres with `--with-system-tzdata=<store>`
/// and embeds the full path to the `locale` binary it will shell out
/// to. Both paths exist only on the packer's machine. On the user's
/// machine postgres prints a parade of warnings about the missing
/// directory, then falls back to internal UTC-only behavior and
/// still-functional locale defaults. The bundle still works, but the
/// noise is confusing.
///
/// Replacements are equal-length to avoid any ELF structure shifts,
/// with the replacement null-padded to the original slot size. We
/// target the suffix (e.g. `/share/zoneinfo`, `/bin/locale`) and walk
/// back to the nearest NUL to find the start of the whole path
/// string.
fn scrub_nix_store_paths(path: &Path) -> io::Result<()> {
    let mut data = fs::read(path)?;
    let mut changed = false;

    // (suffix to find, replacement path, friendly name)
    let rewrites: &[(&[u8], &[u8])] = &[
        (b"/share/zoneinfo", b"/usr/share/zoneinfo"),
        (b"/bin/locale", b"/usr/bin/locale"),
    ];

    for (suffix, replacement) in rewrites {
        let mut i = 0;
        while i + suffix.len() <= data.len() {
            if &data[i..i + suffix.len()] != *suffix {
                i += 1;
                continue;
            }
            // Walk back to find the start of this C string.
            let mut start = i;
            while start > 0 && data[start - 1] != 0 {
                start -= 1;
            }
            // Only touch strings rooted in /nix/store/.
            if start + 11 > data.len() || &data[start..start + 11] != b"/nix/store/" {
                i = i + suffix.len();
                continue;
            }
            // Find end of string: walk forward to the NUL.
            let mut end = i + suffix.len();
            while end < data.len() && data[end] != 0 {
                end += 1;
            }
            let slot = end - start;
            if replacement.len() + 1 > slot {
                // Shouldn't happen for these specific replacements,
                // but guard just in case.
                i = end;
                continue;
            }
            data[start..start + replacement.len()].copy_from_slice(replacement);
            for b in &mut data[start + replacement.len()..end] {
                *b = 0;
            }
            changed = true;
            i = end;
        }
    }

    if changed {
        fs::write(path, &data)?;
    }
    Ok(())
}

/// Inject the AT_EXECFN bootstrap into a single ELF binary.
///
/// Repurposes PT_INTERP as PT_LOAD containing the bootstrap payload +
/// metadata. At runtime the bootstrap reads AT_EXECFN from the aux
/// vector, computes the interpreter path relative to the binary's own
/// location (not CWD), mmaps the interpreter, and jumps to its entry.
///
/// Returns Ok(true) if injected, Ok(false) if the binary has no
/// PT_INTERP (static, shared lib, or already injected).
fn inject_relative_interp(path: &Path, rel_interp: &str) -> io::Result<bool> {
    use crate::payload;
    use goblin::elf::program_header::PT_INTERP;

    let data = fs::read(path)?;

    // Skip self-extracting binaries that store metadata at file end.
    // Appending the bootstrap PT_LOAD here would clobber their trailer
    // and break payload detection (e.g. pre-1.3.12 Bun-compiled
    // binaries). For these, the runtime sets LD_LIBRARY_PATH and the
    // kernel exec resolves PT_INTERP normally.
    if has_self_extract_trailer(&data) {
        eprintln!(
            "  note: {} appears to be a self-extracting binary \
             (Bun-compiled or similar); skipping bootstrap injection",
            path.display(),
        );
        return Ok(false);
    }

    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    if elf.header.e_ident[4] != 2 || elf.header.e_ident[5] != 1 {
        return Ok(false); // 64-bit little-endian only
    }
    let is_x86_64 = elf.header.e_machine == goblin::elf::header::EM_X86_64;
    let is_aarch64 = elf.header.e_machine == goblin::elf::header::EM_AARCH64;
    if !is_x86_64 && !is_aarch64 {
        return Ok(false);
    }

    let phdr_idx = match elf
        .program_headers
        .iter()
        .position(|p| p.p_type == PT_INTERP)
    {
        Some(i) => i,
        None => return Ok(false),
    };

    let highest_vend: u64 = elf
        .program_headers
        .iter()
        .filter(|p| p.p_type == goblin::elf::program_header::PT_LOAD)
        .map(|p| p.p_vaddr + p.p_memsz)
        .max()
        .unwrap_or(0);

    let page_size: u64 = 4096;
    let new_vaddr = (highest_vend + page_size - 1) & !(page_size - 1);
    let orig_entry = elf.header.e_entry;
    let e_phoff = elf.header.e_phoff as usize;
    let e_phentsize = elf.header.e_phentsize as usize;
    drop(elf);

    let code = if is_x86_64 {
        payload::BOOTSTRAP_X86_64
    } else {
        payload::BOOTSTRAP_AARCH64
    };
    let rel_bytes = rel_interp.as_bytes();

    // Build: [code] [padding to 8-byte align] [orig_entry u64] [path_len u16] [path NUL]
    let mut blob = Vec::with_capacity(code.len() + 64);
    blob.extend_from_slice(code);
    while blob.len() % 8 != 0 {
        blob.push(0);
    }
    let metadata_offset = blob.len();
    let entry_delta = (orig_entry as i64) - (new_vaddr as i64);
    blob.extend_from_slice(&entry_delta.to_le_bytes());
    blob.extend_from_slice(&(rel_bytes.len() as u16).to_le_bytes());
    blob.extend_from_slice(rel_bytes);
    blob.push(0);

    // Patch the trampoline's metadata-pointer instruction.
    if is_x86_64 {
        let disp = (metadata_offset as i32) - (payload::X86_64_METADATA_LEA_RIP as i32);
        blob[payload::X86_64_METADATA_LEA_DISP_OFFSET
            ..payload::X86_64_METADATA_LEA_DISP_OFFSET + 4]
            .copy_from_slice(&disp.to_le_bytes());
    } else {
        payload::patch_aarch64_adr(&mut blob, metadata_offset);
    }

    let mut modified = data;
    // Pad to page alignment so p_offset % p_align == p_vaddr % p_align.
    // The kernel rejects PT_LOAD segments where this doesn't hold.
    let page = page_size as usize;
    while modified.len() % page != 0 {
        modified.push(0);
    }
    let file_offset = modified.len() as u64;
    let blob_len = blob.len() as u64;
    modified.extend_from_slice(&blob);

    // Overwrite PT_INTERP phdr -> PT_LOAD, then swap it to the end of
    // the phdr table. The bootstrap has the highest vaddr and the kernel
    // uses the FIRST PT_LOAD to compute the ASLR base. If our high-vaddr
    // segment is first, the base is too high and original segments at
    // lower vaddrs fall outside the reserved region.
    let phdr_off = e_phoff + phdr_idx * e_phentsize;
    modified[phdr_off..phdr_off + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    modified[phdr_off + 4..phdr_off + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R|PF_X
    modified[phdr_off + 8..phdr_off + 16].copy_from_slice(&file_offset.to_le_bytes());
    modified[phdr_off + 16..phdr_off + 24].copy_from_slice(&new_vaddr.to_le_bytes());
    modified[phdr_off + 24..phdr_off + 32].copy_from_slice(&new_vaddr.to_le_bytes());
    modified[phdr_off + 32..phdr_off + 40].copy_from_slice(&blob_len.to_le_bytes());
    modified[phdr_off + 40..phdr_off + 48].copy_from_slice(&blob_len.to_le_bytes());
    modified[phdr_off + 48..phdr_off + 56].copy_from_slice(&page_size.to_le_bytes());

    // Swap our phdr entry with the last one so original PT_LOADs come first.
    let e_phnum = u16::from_le_bytes(modified[56..58].try_into().unwrap()) as usize;
    let last_phdr_off = e_phoff + (e_phnum - 1) * e_phentsize;
    if phdr_off != last_phdr_off {
        let mut tmp = vec![0u8; e_phentsize];
        tmp.copy_from_slice(&modified[phdr_off..phdr_off + e_phentsize]);
        modified.copy_within(last_phdr_off..last_phdr_off + e_phentsize, phdr_off);
        modified[last_phdr_off..last_phdr_off + e_phentsize].copy_from_slice(&tmp);
    }

    // Rewrite e_entry
    modified[24..32].copy_from_slice(&new_vaddr.to_le_bytes());

    fs::write(path, &modified)?;
    Ok(true)
}

/// Outcome of trying to make an entrypoint load the onelf-env
/// constructor (re-exec-safe `.onelf/env` / `.onelf/preload`).
enum EnvNeededOutcome {
    /// `libonelf-env.so` is now a DT_NEEDED of the binary.
    Added,
    /// Already a DT_NEEDED (idempotent repack).
    AlreadyPresent,
    /// No onelf-env blob built for this arch; runtime-only env.
    NoBlobForArch,
    /// patchelf unavailable, so DT_NEEDED couldn't be added; the binary
    /// falls back to runtime-set env (not sandbox-re-exec-safe).
    NoPatchelf,
    /// Self-extract trailer or unsupported ELF: left untouched.
    Skipped,
}

/// Stage the arch-appropriate `libonelf-env.so` into `lib_dest` and add
/// it as a `DT_NEEDED` of `path`. Run on the pristine binary *before*
/// bootstrap injection so patchelf operates on a normal ELF (the
/// bootstrap later only repurposes PT_INTERP and appends at EOF, which
/// doesn't disturb the added DT_NEEDED).
fn add_onelf_env_needed(path: &Path, lib_dest: &Path) -> io::Result<EnvNeededOutcome> {
    let data = fs::read(path)?;
    if data.len() < 20 || &data[0..4] != b"\x7fELF" || data[4] != 2 {
        return Ok(EnvNeededOutcome::Skipped); // not a 64-bit ELF
    }
    // patchelf would grow a self-extract binary and clobber its trailer.
    if has_self_extract_trailer(&data) {
        return Ok(EnvNeededOutcome::Skipped);
    }
    let e_machine = u16::from_le_bytes([data[18], data[19]]);
    let Some(blob) = crate::payload::onelf_env_blob(e_machine) else {
        return Ok(EnvNeededOutcome::NoBlobForArch);
    };

    // Skip if this binary already lists the constructor (idempotent).
    if let Ok(elf) = goblin::elf::Elf::parse(&data) {
        if elf
            .libraries
            .iter()
            .any(|l| *l == crate::payload::ONELF_ENV_SONAME)
        {
            return Ok(EnvNeededOutcome::AlreadyPresent);
        }
    }

    // Stage the blob into lib/ (write once; idempotent across binaries).
    let dest = lib_dest.join(crate::payload::ONELF_ENV_SONAME);
    let need_write = match fs::read(&dest) {
        Ok(existing) => existing != blob,
        Err(_) => true,
    };
    if need_write {
        fs::create_dir_all(lib_dest)?;
        fs::write(&dest, blob)?;
        let _ = fs::set_permissions(&dest, std::os::unix::fs::PermissionsExt::from_mode(0o755));
    }

    let Some(patchelf) = which_patchelf() else {
        return Ok(EnvNeededOutcome::NoPatchelf);
    };
    let out = std::process::Command::new(&patchelf)
        .arg("--add-needed")
        .arg(crate::payload::ONELF_ENV_SONAME)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(EnvNeededOutcome::Added),
        Ok(o) => Err(io::Error::other(
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        )),
        Err(e) => Err(e),
    }
}

/// Walk every ELF under `app_dir` and inject the AT_EXECFN bootstrap
/// so the bundled interpreter is found relative to each binary's own
/// location. CWD-independent. Returns the count of injected files.
fn inject_bootstraps(app_dir: &Path, lib_dest: &Path) -> io::Result<usize> {
    let rel_lib = lib_dest
        .strip_prefix(app_dir)
        .unwrap_or(lib_dest)
        .to_path_buf();

    let mut injected = 0usize;
    let mut env_added = 0usize;
    let mut env_no_patchelf: Vec<PathBuf> = Vec::new();
    let mut env_no_blob = false;
    for path in find_elf_files(app_dir) {
        let Some(interp) = parse_interp(&path) else {
            continue;
        };
        let Some(basename) = Path::new(&interp).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let bundled = lib_dest.join(basename);
        if !bundled.exists() {
            continue;
        }
        // Skip everything in lib/ (shared libs, the ld, libc, etc).
        // Only inject into application binaries outside the lib dir.
        if path.starts_with(lib_dest) {
            continue;
        }

        // Compute relative path from binary's dir to the bundled loader.
        let rel_bin = match path.strip_prefix(app_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let depth = rel_bin
            .parent()
            .map(|p| p.components().count())
            .unwrap_or(0);
        let mut rel = PathBuf::new();
        for _ in 0..depth {
            rel.push("..");
        }
        rel.push(&rel_lib);
        rel.push(basename);
        let rel_interp = rel.to_string_lossy().into_owned();

        let perms = fs::metadata(&path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o755);
        let needs_chmod = perms & 0o200 == 0;
        if needs_chmod {
            let _ = fs::set_permissions(
                &path,
                std::os::unix::fs::PermissionsExt::from_mode(perms | 0o200),
            );
        }
        // Make .onelf/env + .onelf/preload re-exec-safe by injecting the
        // onelf-env constructor as a DT_NEEDED (resolved via the
        // $ORIGIN RUNPATH set earlier). Done before bootstrap injection
        // so patchelf sees a normal ELF.
        match add_onelf_env_needed(&path, lib_dest) {
            Ok(EnvNeededOutcome::Added) => env_added += 1,
            Ok(EnvNeededOutcome::AlreadyPresent) => {}
            Ok(EnvNeededOutcome::NoBlobForArch) => env_no_blob = true,
            Ok(EnvNeededOutcome::NoPatchelf) => env_no_patchelf.push(path.clone()),
            Ok(EnvNeededOutcome::Skipped) => {}
            Err(e) => {
                eprintln!(
                    "  {} could not add onelf-env to {}: {e}",
                    color::bold_red("warning:"),
                    path.display()
                );
            }
        }

        match inject_relative_interp(&path, &rel_interp) {
            Ok(true) => injected += 1,
            Ok(false) => {}
            Err(e) => {
                eprintln!(
                    "  {} could not inject bootstrap into {}: {e}",
                    color::bold_red("warning:"),
                    path.display()
                );
            }
        }
        if needs_chmod {
            let _ = fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(perms));
        }
    }

    if env_added > 0 {
        eprintln!(
            "{} onelf-env (re-exec-safe .onelf/env) into {} binaries",
            color::bold("Injected"),
            env_added
        );
    }
    if !env_no_patchelf.is_empty() {
        eprintln!(
            "{} patchelf unavailable; {} executable(s) won't re-apply \
             .onelf/env after a sandboxed re-exec:",
            color::bold_red("warning:"),
            env_no_patchelf.len()
        );
        for p in &env_no_patchelf {
            eprintln!("  - {}", p.display());
        }
        eprintln!(
            "  Install `patchelf` (or set ONELF_PATCHELF) and repack for \
             re-exec-safe env."
        );
    }
    if env_no_blob {
        eprintln!(
            "{} no onelf-env blob built for this target arch; \
             .onelf/env is runtime-only (not sandbox-re-exec-safe). \
             Build it via crates/onelf/src/payload/Makefile.",
            color::bold_red("warning:"),
        );
    }
    Ok(injected)
}

fn find_existing_libs(dir: &Path) -> HashSet<String> {
    let mut libs = HashSet::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false) {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy();
        if !name.contains(".so") {
            continue;
        }
        // A previous run may have copied NixOS's stub loader into the
        // bundle. Treat it as absent so it gets replaced with a real
        // loader on this pass; otherwise the stale stub would persist
        // forever.
        if is_nix_stub_ld(&path) {
            let _ = fs::remove_file(&path);
            continue;
        }
        libs.insert(name.into_owned());
    }
    libs
}

fn build_lib_cache() -> HashMap<String, Vec<PathBuf>> {
    let cache = parse_ldconfig_cache();
    if !cache.is_empty() {
        return cache;
    }

    // Fallback: on NixOS, ldconfig has no cache. Scan the system closure instead.
    if Path::new("/nix/store").is_dir() {
        return scan_nix_store_libs();
    }

    cache
}

fn parse_ldconfig_cache() -> HashMap<String, Vec<PathBuf>> {
    let mut cache: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let Ok(output) = Command::new("ldconfig").arg("-p").output() else {
        return cache;
    };
    // Lines like: "	libX11.so.6 (libc6,x86-64) => /usr/lib/libX11.so.6"
    for line in output.stdout.lines().map_while(Result::ok) {
        let line = line.trim();
        if let Some((left, right)) = line.split_once(" => ") {
            let soname = left.split_whitespace().next().unwrap_or("");
            if !soname.is_empty() {
                cache
                    .entry(soname.to_string())
                    .or_default()
                    .push(PathBuf::from(right.trim()));
            }
        }
    }
    cache
}

/// Scan lib/ directories from NixOS closures to build a soname map.
/// Scans the system closure, user profile, and home-manager profile.
fn scan_nix_store_libs() -> HashMap<String, Vec<PathBuf>> {
    let mut cache: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut store_paths: HashSet<String> = HashSet::new();

    // Collect store paths from multiple roots
    let roots: Vec<&str> = vec![
        "/run/current-system",
        "~/.nix-profile",
        "/etc/profiles/per-user",
    ];

    for root in &roots {
        let expanded = if root.starts_with('~') {
            if let Ok(home) = std::env::var("HOME") {
                root.replacen('~', &home, 1)
            } else {
                continue;
            }
        } else {
            root.to_string()
        };

        if !Path::new(&expanded).exists() {
            continue;
        }

        let Ok(output) = Command::new("nix-store").args(["-qR", &expanded]).output() else {
            continue;
        };

        if output.status.success() {
            for line in output.stdout.lines().map_while(Result::ok) {
                store_paths.insert(line.trim().to_string());
            }
        }
    }

    if store_paths.is_empty() {
        return cache;
    }

    let lib_dirs: Vec<PathBuf> = store_paths
        .iter()
        .map(|p| PathBuf::from(p).join("lib"))
        .filter(|p| p.is_dir())
        .collect();

    eprintln!(
        "{} scanning {} store paths...",
        color::dim("NixOS detected,"),
        lib_dirs.len()
    );

    for lib_dir in &lib_dirs {
        for entry in jwalk::WalkDir::new(lib_dir).max_depth(3).skip_hidden(false) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(name) = entry.path().file_name() {
                let name = name.to_string_lossy();
                if name.contains(".so") {
                    cache
                        .entry(name.into_owned())
                        .or_default()
                        .push(entry.path());
                }
            }
        }
    }

    cache
}

/// Extract the nix store path from a full path.
/// e.g. /nix/store/HASH-name/lib/foo.so -> /nix/store/HASH-name
fn nix_store_path(path: &Path) -> Option<PathBuf> {
    let s = path.to_string_lossy();
    let rest = s.strip_prefix("/nix/store/")?;
    let end = rest.find('/').unwrap_or(rest.len());
    Some(PathBuf::from(format!("/nix/store/{}", &rest[..end])))
}

/// When a lib is resolved from the nix store, scan its store path's closure
/// to discover transitive dependencies that may not be in the initial scan set.
/// Tracks already-expanded store paths to avoid redundant work.
fn expand_nix_cache(
    resolved: &Path,
    cache: &mut HashMap<String, Vec<PathBuf>>,
    expanded: &mut HashSet<PathBuf>,
) {
    let store_path = match nix_store_path(resolved) {
        Some(p) => p,
        None => return,
    };

    if !expanded.insert(store_path.clone()) {
        return; // already expanded this store path
    }

    let Ok(output) = Command::new("nix-store")
        .args(["-qR"])
        .arg(&store_path)
        .output()
    else {
        return;
    };

    if !output.status.success() {
        return;
    }

    for line in output.stdout.lines().map_while(Result::ok) {
        let lib_dir = PathBuf::from(line.trim()).join("lib");
        if !lib_dir.is_dir() {
            continue;
        }
        for entry in jwalk::WalkDir::new(&lib_dir)
            .max_depth(3)
            .skip_hidden(false)
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(name) = entry.path().file_name() {
                let name = name.to_string_lossy();
                if name.contains(".so") {
                    let paths = cache.entry(name.into_owned()).or_default();
                    let path = entry.path();
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
        }
    }
}

fn locate_lib(
    soname: &str,
    ldconfig_cache: &HashMap<String, Vec<PathBuf>>,
    search_paths: &[PathBuf],
    target_class: Option<u8>,
) -> Option<PathBuf> {
    let class_matches = |path: &Path| -> bool {
        match target_class {
            Some(tc) => read_elf_class(path) == Some(tc),
            None => true,
        }
    };
    // Reject NixOS's stub loader anywhere it surfaces. It exists on disk
    // but refuses to actually load foreign binaries, so bundling it would
    // produce a package that runs nowhere.
    let acceptable = |path: &Path| class_matches(path) && !is_nix_stub_ld(path);

    // 1. --search-path directories (user-provided: highest priority)
    for dir in search_paths {
        let candidate = dir.join(soname);
        if candidate.exists() && acceptable(&candidate) {
            return Some(candidate);
        }
    }

    // 2. ldconfig cache
    if let Some(paths) = ldconfig_cache.get(soname) {
        for path in paths {
            if path.exists() && acceptable(path) {
                return Some(path.clone());
            }
        }
    }

    // 3. Standard paths
    for dir in STANDARD_LIB_PATHS {
        let candidate = Path::new(dir).join(soname);
        if candidate.exists() && acceptable(&candidate) {
            return Some(candidate);
        }
    }

    // 4. LD_LIBRARY_PATH and NIX_LD_LIBRARY_PATH
    for var in ["LD_LIBRARY_PATH", "NIX_LD_LIBRARY_PATH"] {
        if let Ok(val) = std::env::var(var) {
            for dir in val.split(':') {
                if dir.is_empty() {
                    continue;
                }
                let candidate = Path::new(dir).join(soname);
                if candidate.exists() && acceptable(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }

    // 5. NixOS fallback: scan /nix/store/*/lib/ directly
    if Path::new("/nix/store").is_dir() {
        if let Ok(entries) = fs::read_dir("/nix/store") {
            for entry in entries.filter_map(Result::ok) {
                let lib_dir = entry.path().join("lib");
                // Check lib/<soname> directly
                let candidate = lib_dir.join(soname);
                if candidate.exists() && acceptable(&candidate) {
                    return Some(candidate);
                }
                // Also check one level of subdirs (e.g. lib/pulseaudio/)
                if let Ok(subdirs) = fs::read_dir(&lib_dir) {
                    for subdir in subdirs.filter_map(Result::ok) {
                        if subdir.file_type().map_or(false, |t| t.is_dir()) {
                            let candidate = subdir.path().join(soname);
                            if candidate.exists() && acceptable(&candidate) {
                                return Some(candidate);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Detect NixOS's stub-ld, a tiny loader that prints a message and exits.
/// The stub lives at `/lib*/ld-*` on NixOS when nix-ld isn't enabled.
/// We check two signals:
///
/// 1. The canonical path contains `stub-ld` (covers the fresh symlink case).
/// 2. The file content contains NixOS's signature error string (covers the
///    case where a previous bundle copied the stub into the AppDir itself,
///    so canonicalize no longer points at the nix store).
fn is_nix_stub_ld(path: &Path) -> bool {
    if let Ok(real) = fs::canonicalize(path) {
        if real.to_string_lossy().contains("stub-ld") {
            return true;
        }
    }
    // Real glibc ld-linux is >100 KB; the stub is ~35 KB. Cheap filter
    // before hashing through the file content.
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > 128 * 1024 {
        return false;
    }
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    bytes
        .windows(b"NixOS cannot run".len())
        .any(|w| w == b"NixOS cannot run")
}

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

const VK_SEARCH_PATHS: &[&str] = &["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"];

/// Bundle GPU drivers and vendor configs so OpenGL/Vulkan/EGL apps work portably.
fn bundle_gpu(
    directory: &Path,
    lib_dir: &Path,
    extra_search: &[PathBuf],
    dry_run: bool,
    strip: bool,
    include_gl: bool,
    include_dri: bool,
    include_vulkan: bool,
) -> io::Result<()> {
    eprintln!("{} GPU drivers...", color::bold("Bundling"));

    let elf_files = find_elf_files(directory);

    // Determine target ELF class and machine type from existing binaries
    let target_class = elf_files.iter().find_map(|f| read_elf_class(f));
    let target_machine = elf_files.iter().find_map(|f| read_elf_machine(f));

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
        collect_nix_store_paths()
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

    // 0. Remove conflicting GL libraries shipped by the application (e.g.
    //    old monolithic Mesa libGL.so in a subdirectory) so they don't shadow
    //    the glvnd versions we're about to copy.
    if include_gl {
        remove_conflicting_gl_libs(directory, &lib_dest, dry_run);
    }

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

    // 2. DRI drivers (only with --dri)
    let mut dri_count = 0;
    if include_dri {
        let dri_filter = driver_filter(target_machine, DRI_DRIVERS_X86, DRI_DRIVERS_ARM);
        let dri_dest = lib_dest.join("dri");
        let (count, bytes) = copy_so_dir(
            &dri_dirs,
            &dri_dest,
            target_class,
            dri_filter,
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
        let (count, bytes) = copy_so_dir(&gbm_dirs, &gbm_dest, target_class, None, dry_run, strip)?;
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
        let (count, bytes) =
            copy_vendor_json(&egl_dirs, &egl_dest, &lib_dest, target_class, None, dry_run)?;
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
            vk_filter,
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
    name_filter: Option<&[&str]>,
    dry_run: bool,
    strip: bool,
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
            if !name.contains(".so") {
                continue;
            }
            // Architecture-specific driver filter
            if let Some(allowed) = name_filter {
                if !allowed.iter().any(|a| name.starts_with(a)) {
                    continue;
                }
            }
            // Skip if we already have this filename from an earlier directory
            if !seen.insert(name.clone()) {
                continue;
            }
            // ELF class filter
            if let Some(tc) = target_class {
                if read_elf_class(&path) != Some(tc) {
                    continue;
                }
            }
            let resolved = fs::canonicalize(&path).unwrap_or(path.clone());
            let size = fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
            eprintln!(
                "  {} <- {} ({})",
                color::bold_green(&name),
                resolved.display(),
                color::dim(&format_size(size))
            );
            if !dry_run {
                fs::create_dir_all(dest)?;
                let dest_path = dest.join(&name);
                ensure_writable(&dest_path);
                fs::copy(&resolved, &dest_path)?;
                let _ = fs::set_permissions(&dest_path, PermissionsExt::from_mode(0o755));
                if strip {
                    strip_debug(&dest_path);
                }
            }
            copied += 1;
            total_bytes += size;
        }
    }
    Ok((copied, total_bytes))
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
fn bundle_wayland(
    directory: &Path,
    lib_dir: &Path,
    extra_search: &[PathBuf],
    dry_run: bool,
    strip: bool,
) -> io::Result<()> {
    eprintln!("{} Wayland libraries...", color::bold("Bundling"));

    let elf_files = find_elf_files(directory);
    let target_class = elf_files.iter().find_map(|f| read_elf_class(f));

    let nix_paths = if Path::new("/nix/store").is_dir() {
        collect_nix_store_paths()
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
        None,
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
fn bundle_gtk_data(directory: &Path, dry_run: bool) -> io::Result<()> {
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
        for sp in &collect_nix_store_paths() {
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

    // Collect all .gschema.xml files into a temp dir, then compile
    let tmp = directory.join(".onelf-schemas-tmp");
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
            if !name.ends_with(".gschema.xml") && !name.ends_with(".enums.xml") {
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
        let _ = fs::remove_dir_all(&tmp);
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
        let _ = fs::remove_dir_all(&tmp);
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

    // NixOS: search store for glib-*-dev/bin/glib-compile-schemas
    if Path::new("/nix/store").is_dir() {
        if let Ok(entries) = fs::read_dir("/nix/store") {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.contains("glib-") && name.ends_with("-dev") {
                    let candidate = entry.path().join("bin/glib-compile-schemas");
                    if candidate.is_file() {
                        return candidate;
                    }
                }
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
    driver_filter: Option<&[&str]>,
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
            if !seen.insert(name.clone()) {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let (rewritten, so_path) = rewrite_library_path(&content, &path);

            // If we found a library_path, validate ELF class and copy the .so
            if let Some(ref so_src) = so_path {
                let resolved = fs::canonicalize(so_src).unwrap_or(so_src.clone());
                // Architecture-specific driver filter
                if let Some(allowed) = driver_filter {
                    let so_fname = resolved.file_name().unwrap_or_default().to_string_lossy();
                    if !allowed.iter().any(|a| so_fname.starts_with(a)) {
                        continue;
                    }
                }
                if let Some(tc) = target_class {
                    if read_elf_class(&resolved) != Some(tc) {
                        continue;
                    }
                }
                let so_name = resolved
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
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
                    if !dest_so.exists() {
                        fs::copy(&resolved, &dest_so)?;
                        let _ = fs::set_permissions(&dest_so, PermissionsExt::from_mode(0o755));
                    }
                }
                total_bytes += so_size;
            }

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

    let filename = resolved
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    // Rewrite the content: replace the path with just the filename
    let mut rewritten = String::with_capacity(content.len());
    rewritten.push_str(&content[..value_start]);
    rewritten.push_str(&filename);
    rewritten.push_str(&content[value_start + close_quote..]);

    (rewritten, Some(resolved))
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
        }
        count += 1;
    }
    Ok(count)
}

/// Collect nix store paths from system and user closures.
fn collect_nix_store_paths() -> Vec<String> {
    let mut store_paths: HashSet<String> = HashSet::new();

    let roots: &[&str] = &["/run/current-system", "/etc/profiles/per-user"];

    // Also try ~/.nix-profile
    let home_profile = std::env::var("HOME")
        .ok()
        .map(|h| format!("{h}/.nix-profile"));

    for root in roots.iter().copied().chain(home_profile.as_deref()) {
        if !Path::new(root).exists() {
            continue;
        }
        let Ok(output) = Command::new("nix-store").args(["-qR", root]).output() else {
            continue;
        };
        if output.status.success() {
            for line in output.stdout.lines().map_while(Result::ok) {
                store_paths.insert(line.trim().to_string());
            }
        }
    }

    store_paths.into_iter().collect()
}

#[cfg(test)]
mod framework_scan_tests {
    use super::*;

    fn scan(bytes: &[u8]) -> FrameworkFlags {
        let mut flags = FrameworkFlags::default();
        scan_framework_strings(bytes, &mut flags);
        flags
    }

    #[test]
    fn detects_versioned_soname_merged_without_nul_separators() {
        // Rust string merging packs literals together (no NUL between them),
        // exactly as seen in amdgpu_top's dlopen strings.
        let blob = b"eglWaitSynclibEGL.so.1libEGL.so";
        let flags = scan(blob);
        assert!(
            flags.gl,
            "versioned libEGL.so.1 inside a merged blob should flag gl"
        );
    }

    #[test]
    fn detects_versioned_wayland_in_merged_blob() {
        let blob = b"some junklibwayland-client.so.0libwayland-client.so";
        let flags = scan(blob);
        assert!(flags.wayland);
    }

    #[test]
    fn ignores_unversioned_soname_in_prose() {
        // Error messages embed the bare `.so` form, which we must not flag.
        let blob = b"Library libwayland-client.so could not be loaded.";
        let flags = scan(blob);
        assert!(
            !flags.wayland,
            "unversioned soname in prose must not flag wayland"
        );
    }

    #[test]
    fn ignores_bare_soname_without_version() {
        let blob = b"\0libvulkan.so\0";
        let flags = scan(blob);
        assert!(
            !flags.vulkan,
            "bare libvulkan.so without a version must not flag"
        );
    }

    #[test]
    fn detects_nul_terminated_versioned_soname() {
        let blob = b"\0libvulkan.so.1\0";
        let flags = scan(blob);
        assert!(flags.vulkan);
    }

    #[test]
    fn classifies_glesv_stem_with_inner_version() {
        let blob = b"\0libGLESv2.so.2\0";
        let flags = scan(blob);
        assert!(flags.gl, "libGLESv2.so.2 should flag gl");
    }

    #[test]
    fn gtk_versioned_soname() {
        let blob = b"prefixlibgtk-3.so.0suffix";
        let flags = scan(blob);
        assert!(flags.gtk);
    }
}

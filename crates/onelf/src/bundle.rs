//! Shared library bundling for ONELF packages.
//!
//! Scans ELF binaries in a directory for shared library dependencies,
//! resolves them via ldconfig cache, standard paths, or NixOS store
//! scanning, and copies them into a lib directory for self-contained
//! packaging.
//!
//! # External tools and trust boundary
//!
//! Bundling shells out to a few host tools: `ldconfig` (read the library
//! cache), `patchelf` (rewrite RUNPATH when no in-place slot fits),
//! `strip` (drop debug info from copied libraries), `nix-store` (walk Nix
//! closures), and `glib-compile-schemas` (compile GSettings schemas). Every
//! invocation is built as an explicit argument vector via [`Command`] with
//! no shell, so file names and paths are never subject to shell word
//! splitting or metacharacter interpretation. These tools run over the
//! *developer's own* build inputs (the app tree being packaged and the
//! host's libraries), i.e. trusted input at package-build time, not over
//! attacker-controlled package contents at runtime. Absolute tool paths
//! used as fallbacks (`/sbin/ldconfig`, `/usr/sbin/ldconfig`) are fixed
//! constants, not derived from any input.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

mod ui;
pub(crate) use ui::{color, format_size};
mod gpu;
use gpu::{bundle_gpu, bundle_gtk_data, bundle_wayland};
mod resolve;
pub(crate) use resolve::*;
mod elf;
pub(crate) use elf::*;

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

/// True if `dest` already resolves to the same on-disk file as the
/// already-canonicalized `src`. Copying a file onto itself with `fs::copy`
/// truncates it, which can happen once an expanded `$ORIGIN` search dir is
/// the bundle's own lib dir on a second pass.
fn is_same_file(src: &Path, dest: &Path) -> bool {
    fs::canonicalize(dest).map(|c| c == src).unwrap_or(false)
}

/// Copy libraries matching any of `prefixes` (prefix match on filename) from
/// `search_dirs` into `dest`. Resolves symlinks, deduplicates by filename,
/// and filters by ELF class. Returns (files_copied, total_bytes).
/// Copy libraries from `search_dirs` into `dest`, selecting entries by
/// `name_pred` (on the file name). Resolves symlinks, filters by ELF class
/// and machine, dedups by soname only after every filter passes (so a
/// wrong-arch or dangling earlier copy never shadows a valid later one),
/// removes an alias symlink at the destination before copying, and never
/// copies a file onto itself. Returns `(files_copied, total_bytes)`.
fn copy_libs(
    search_dirs: &[PathBuf],
    dest: &Path,
    name_pred: impl Fn(&str) -> bool,
    target_class: Option<u8>,
    target_machine: Option<u16>,
    excludes: &[&str],
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
            if !name_pred(&name) {
                continue;
            }
            if is_excluded(&name, excludes) || seen.contains(&name) {
                continue;
            }
            let resolved = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !resolved.is_file() {
                continue;
            }
            if let Some(tc) = target_class {
                if read_elf_class(&resolved) != Some(tc) {
                    continue;
                }
            }
            if let Some(tm) = target_machine {
                if read_elf_machine(&resolved) != Some(tm) {
                    continue;
                }
            }
            seen.insert(name.clone());
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
                // Copying onto an existing alias symlink corrupts the alias
                // target; drop the symlink first (propagate failure so we
                // never fall through to fs::copy and clobber the target).
                if dest_path.is_symlink() {
                    fs::remove_file(&dest_path)?;
                }
                // Never copy a file onto itself (an $ORIGIN search dir that
                // resolved to the bundle's own lib dir): fs::copy truncates it.
                if !is_same_file(&resolved, &dest_path) {
                    ensure_writable(&dest_path);
                    fs::copy(&resolved, &dest_path)?;
                    let _ = fs::set_permissions(&dest_path, PermissionsExt::from_mode(0o755));
                    if strip {
                        strip_debug(&dest_path);
                    }
                    normalize_mtime(&dest_path);
                }
            }
            copied += 1;
            total_bytes += size;
        }
    }
    Ok((copied, total_bytes))
}

/// Copy libraries whose file name starts with any of `prefixes`.
fn copy_prefixed_libs(
    search_dirs: &[PathBuf],
    prefixes: &[&str],
    dest: &Path,
    target_class: Option<u8>,
    target_machine: Option<u16>,
    excludes: &[&str],
    dry_run: bool,
    strip: bool,
) -> io::Result<(usize, u64)> {
    copy_libs(
        search_dirs,
        dest,
        |name| prefixes.iter().any(|p| name.starts_with(p)),
        target_class,
        target_machine,
        excludes,
        dry_run,
        strip,
    )
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
    "/usr/lib32",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib/i386-linux-gnu",
    "/usr/lib/arm-linux-gnueabihf",
    "/lib",
    "/lib64",
    "/lib32",
    "/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/lib/i386-linux-gnu",
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
    // Stripping a Bun-compiled binary makes it lose its embedded module graph
    // and fall back to the bare `bun` CLI (the `.bun` section survives, but
    // strip perturbs what the runtime payload check relies on). Leave such
    // binaries untouched, same as the RUNPATH / DT_NEEDED / bootstrap steps.
    if fs::read(path)
        .map(|d| has_embedded_payload(&d))
        .unwrap_or(false)
    {
        return;
    }
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
    // Resolve the input binaries once (respecting --target) and derive the
    // architecture up front: this single walk feeds framework detection and
    // the framework bundlers below, and keys them off the app's real
    // class/machine rather than whatever ELF find_map first hits.
    let target_elfs = resolve_target_elfs(opts)?;
    let target_class = target_elfs.iter().find_map(|f| read_elf_class(f));
    let target_machine = target_elfs.iter().find_map(|f| read_elf_machine(f));

    let detected = detect_frameworks(&target_elfs);
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

    // Built here (before the framework bundlers) so `--exclude` applies to
    // the GL/GPU/Wayland copy helpers too, not only the primary walk below.
    let excludes: Vec<&str> = DEFAULT_EXCLUDES
        .iter()
        .copied()
        .chain(opts.exclude.iter().map(|s| s.as_str()))
        .collect();

    // Bundle GPU assets first so DRI driver .so files are present when the
    // dependency walk below runs, letting it resolve their transitive deps.
    if want_gl || want_dri || want_vulkan {
        bundle_gpu(
            &opts.directory,
            &opts.lib_dir,
            &opts.search_path,
            &excludes,
            target_class,
            target_machine,
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
            &excludes,
            target_class,
            target_machine,
            opts.dry_run,
            opts.strip,
        )?;
    }

    if want_gtk {
        bundle_gtk_data(&opts.directory, opts.dry_run)?;
    }

    // Re-scan after the GPU/Wayland copy so freshly-bundled libraries are
    // walked for their own transitive dependencies (they did not exist when
    // the target set was resolved above). An explicit --target keeps the
    // walk scoped to that one binary.
    let elf_files = if opts.target.is_some() {
        target_elfs
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

    // Filter libs already present in the directory tree. Stale stub loaders
    // are deleted only in the non-dry-run path so they get re-resolved.
    let (existing, stubs) = find_existing_libs(&opts.directory);
    if !opts.dry_run {
        for stub in &stubs {
            // Fail if a stub can't be removed: leaving it in place would ship
            // a loader that runs nowhere, so bundling must not continue.
            fs::remove_file(stub).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("removing stub loader {}: {e}", stub.display()),
                )
            })?;
        }
    }
    needed_by.retain(|soname, _| !existing.contains(soname));

    if needed_by.is_empty() {
        eprintln!("All dependencies satisfied, nothing to bundle.");
        // PT_INTERP + RUNPATH rewrites still need to run. A prior bundle
        // may have left stale paths (e.g. from an older onelf version)
        // that either don't resolve under the current CWD policy or
        // still rely on LD_LIBRARY_PATH.
        if !opts.dry_run {
            let lib_dest = opts.directory.join(&opts.lib_dir);
            let (rewritten, _scrubbed, unguaranteed, self_extract) = finalize_tree(&opts.directory);
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

    // target_class / target_machine were derived up front (before the
    // framework bundlers) from the --target-respecting set and are reused
    // here so the dependency walk resolves against the same architecture.

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
        let (bundled, stubs) = find_existing_libs(&lib_dest);
        if !opts.dry_run {
            for stub in &stubs {
                fs::remove_file(stub).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("removing stub loader {}: {e}", stub.display()),
                    )
                })?;
            }
        }
        for lib_name in bundled {
            if let Some(src) = locate_lib(
                &lib_name,
                &ldconfig_cache,
                &search_paths,
                target_class,
                target_machine,
            ) {
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

        match locate_lib(
            &soname,
            &ldconfig_cache,
            &search_paths,
            target_class,
            target_machine,
        ) {
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
                    // Copying onto an existing alias symlink corrupts the
                    // alias target; drop the symlink first.
                    if dest.is_symlink() {
                        let _ = fs::remove_file(&dest);
                    }
                    ensure_writable(&dest);
                    fs::copy(&resolved, &dest)?;
                    let _ = fs::set_permissions(
                        &dest,
                        std::os::unix::fs::PermissionsExt::from_mode(0o755),
                    );
                    normalize_mtime(&dest);
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
        let (rewritten, scrubbed, unguaranteed, self_extract) = finalize_tree(&opts.directory);
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

/// Pin a bundled file's mtime so repeated `bundle-libs` runs produce
/// metadata-identical trees (`fs::copy` otherwise stamps "now"). Honors
/// `SOURCE_DATE_EPOCH`, else the Unix epoch. Best-effort: errors ignored.
fn normalize_mtime(path: &Path) {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t));
    }
}

/// Resolve the set of target binaries to bundle: just `--target` when set
/// (validated to exist), otherwise every ELF found under the app directory.
fn resolve_target_elfs(opts: &BundleOptions) -> io::Result<Vec<PathBuf>> {
    if let Some(ref target) = opts.target {
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
        Ok(vec![path])
    } else {
        Ok(find_elf_files(&opts.directory))
    }
}

fn find_elf_files(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false).sort(true) {
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
fn detect_frameworks(files: &[PathBuf]) -> FrameworkFlags {
    let mut flags = FrameworkFlags::default();
    for path in files {
        // Read each ELF once and use the bytes for both DT_NEEDED parsing
        // and the dlopen string scan.
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        if let Ok(needed) = parse_needed_bytes(&bytes) {
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
        scan_framework_strings(&bytes, &mut flags);
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
    // `libgtk-` covers every GTK major (libgtk-3.so, libgtk-4.so, ...).
    if soname.starts_with("libgtk-") {
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
        b"libgtk-",
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

#[cfg(test)]
mod correctness_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("onelf-bundle-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Minimal ELF-shaped bytes: magic + EI_CLASS + e_machine, enough for
    /// `read_elf_class` / `read_elf_machine` to classify.
    fn fake_elf(class: u8, machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = class;
        v[18..20].copy_from_slice(&machine.to_le_bytes());
        v
    }

    #[test]
    fn nix_stub_matcher_requires_loader_shape() {
        let d = tmp("stub");
        let mut stub = b"\x7fELF".to_vec();
        stub.extend_from_slice(b" ... NixOS cannot run this binary ... ");
        let stub_path = d.join("ld-linux-x86-64.so.2");
        fs::write(&stub_path, &stub).unwrap();
        assert!(is_nix_stub_ld(&stub_path));

        // Benign data file containing the phrase but not loader-shaped.
        let benign = d.join("notes.txt");
        fs::write(&benign, b"the NixOS cannot run message appears here").unwrap();
        assert!(!is_nix_stub_ld(&benign));

        // `ld-*` name but no ELF magic.
        let noelf = d.join("ld-fake.so");
        fs::write(&noelf, b"NixOS cannot run").unwrap();
        assert!(!is_nix_stub_ld(&noelf));

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn find_existing_libs_reports_stubs_without_deleting() {
        let d = tmp("dryscan");
        let mut stub = b"\x7fELF".to_vec();
        stub.extend_from_slice(b" NixOS cannot run ");
        let stub_path = d.join("ld-linux-x86-64.so.2");
        fs::write(&stub_path, &stub).unwrap();
        fs::write(d.join("libfoo.so.1"), fake_elf(2, 62)).unwrap();

        let (libs, stubs) = find_existing_libs(&d);
        assert!(stub_path.exists(), "the scan must not delete the stub");
        assert!(stubs.iter().any(|p| p == &stub_path));
        assert!(libs.contains("libfoo.so.1"));
        assert!(!libs.contains("ld-linux-x86-64.so.2"));

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn runpath_entry_expands_origin() {
        let origin = Path::new("/app/bin");
        assert_eq!(
            expand_runpath_entry("$ORIGIN/../lib", origin),
            PathBuf::from("/app/bin/../lib")
        );
        assert_eq!(
            expand_runpath_entry("${ORIGIN}/lib", origin),
            PathBuf::from("/app/bin/lib")
        );
        assert_eq!(
            expand_runpath_entry("/usr/lib", origin),
            PathBuf::from("/usr/lib")
        );
    }

    #[test]
    fn runpath_splits_colons_and_resolves_origin() {
        let d = tmp("rpath");
        fs::create_dir_all(d.join("a/lib")).unwrap();
        fs::create_dir_all(d.join("a/lib64")).unwrap();
        fs::create_dir_all(d.join("a/bin")).unwrap();

        // Colon list: both existing dirs resolve.
        let list = format!(
            "{}:{}",
            d.join("a/lib").display(),
            d.join("a/lib64").display()
        );
        let dirs = resolve_runpath_dirs(std::iter::once(list.as_str()), Path::new("/"));
        assert!(dirs.contains(&d.join("a/lib")));
        assert!(dirs.contains(&d.join("a/lib64")));

        // `$ORIGIN` relative to the ELF's own dir resolves to a real dir.
        let origin = d.join("a/bin");
        let dirs = resolve_runpath_dirs(std::iter::once("$ORIGIN/../lib"), &origin);
        assert_eq!(dirs.len(), 1);
        assert_eq!(
            fs::canonicalize(&dirs[0]).unwrap(),
            fs::canonicalize(d.join("a/lib")).unwrap()
        );

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_dedupes_after_filters_and_honors_arch_and_excludes() {
        let d = tmp("copy");
        let dir1 = d.join("dir1");
        let dir2 = d.join("dir2");
        let dest = d.join("dest");
        fs::create_dir_all(&dir1).unwrap();
        fs::create_dir_all(&dir2).unwrap();

        // dir1 (searched first) has a DANGLING symlink for the soname.
        std::os::unix::fs::symlink(d.join("nonexistent"), dir1.join("libGL.so.1")).unwrap();
        // dir2 has the valid x86-64 library, plus a wrong-arch (aarch64) one.
        fs::write(dir2.join("libGL.so.1"), fake_elf(2, 62)).unwrap();
        fs::write(dir2.join("libwrong.so"), fake_elf(2, 183)).unwrap();

        let (copied, _) = copy_prefixed_libs(
            &[dir1.clone(), dir2.clone()],
            &["libGL", "libwrong"],
            &dest,
            Some(2),  // ELFCLASS64
            Some(62), // EM_X86_64
            &[],
            false,
            false,
        )
        .unwrap();
        assert!(
            dest.join("libGL.so.1").is_file(),
            "the valid libGL must not be shadowed by the earlier dangling one"
        );
        assert!(
            !dest.join("libwrong.so").exists(),
            "a wrong-arch library must be rejected"
        );
        assert_eq!(copied, 1);

        // With an exclude for the soname, nothing is copied.
        let dest2 = d.join("dest2");
        let (copied2, _) = copy_prefixed_libs(
            &[dir2.clone()],
            &["libGL"],
            &dest2,
            Some(2),
            Some(62),
            &["libGL"],
            false,
            false,
        )
        .unwrap();
        assert_eq!(copied2, 0, "an excluded soname must not be copied");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn bootstrap_alignment_is_16k_safe_on_aarch64() {
        assert_eq!(bootstrap_page_align(true), 0x10000);
        assert_eq!(bootstrap_page_align(false), 0x1000);

        // The injected segment's file offset and vaddr are aligned to the
        // same value, so `p_offset % p_align == p_vaddr % p_align` holds
        // under any aarch64 page size (4K / 16K / 64K).
        let align = bootstrap_page_align(true);
        for (vend, len) in [(0x1234u64, 0x5678u64), (0, 1), (0xffff, 0x10001)] {
            let new_vaddr = (vend + align - 1) & !(align - 1);
            let file_off = (len + align - 1) & !(align - 1);
            for page in [0x1000u64, 0x4000, 0x10000] {
                assert_eq!(new_vaddr % page, file_off % page);
            }
        }
    }

    #[test]
    fn copy_does_not_truncate_a_file_onto_itself() {
        let d = tmp("selfcopy");
        // The search dir IS the destination dir, as an expanded `$ORIGIN`
        // runpath resolves to the bundle's own lib dir on a second pass.
        fs::write(d.join("libself.so.1"), fake_elf(2, 62)).unwrap();
        let before = fs::read(d.join("libself.so.1")).unwrap();
        assert!(!before.is_empty());

        let (_copied, _) = copy_prefixed_libs(
            &[d.clone()],
            &["libself"],
            &d, // dest == search dir
            Some(2),
            Some(62),
            &[],
            false,
            false,
        )
        .unwrap();

        // The file must be intact, not truncated to zero by `fs::copy(x, x)`.
        let after = fs::read(d.join("libself.so.1")).unwrap();
        assert_eq!(before, after, "a self-copy must not truncate the file");

        let _ = fs::remove_dir_all(&d);
    }
}

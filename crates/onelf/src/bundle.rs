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
    "/lib",
    "/lib64",
    "/lib/x86_64-linux-gnu",
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
    pub strip: bool,
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

    // Bundle GPU assets first so DRI driver .so files are present when
    // find_elf_files runs, letting the main loop resolve their transitive deps.
    if opts.gl || opts.dri || opts.vulkan {
        bundle_gpu(
            &opts.directory,
            &opts.lib_dir,
            &opts.search_path,
            opts.dry_run,
            opts.strip,
            opts.gl,
            opts.dri,
            opts.vulkan,
        )?;
    }

    if opts.wayland {
        bundle_wayland(
            &opts.directory,
            &opts.lib_dir,
            &opts.search_path,
            opts.dry_run,
            opts.strip,
        )?;
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
        match parse_needed(path) {
            Ok(libs) => {
                let requirer = path
                    .strip_prefix(&opts.directory)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                for lib in libs {
                    needed_by.entry(lib).or_insert_with(|| requirer.clone());
                }
            }
            Err(e) => {
                eprintln!("warning: {}: {e}", path.display());
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

    // Filter excluded
    needed_by.retain(|soname, _| !is_excluded(soname, &excludes));

    // Filter libs already present in the directory tree
    let existing = find_existing_libs(&opts.directory);
    needed_by.retain(|soname, _| !existing.contains(soname));

    if needed_by.is_empty() {
        eprintln!("All dependencies satisfied, nothing to bundle.");
        return Ok(());
    }

    // Determine target ELF class (32-bit vs 64-bit) from the input binaries
    let target_class = elf_files.iter().find_map(|f| read_elf_class(f));

    let mut ldconfig_cache = build_lib_cache();
    let mut search_paths: Vec<PathBuf> = opts.search_path.clone();
    search_paths.extend(rpath_dirs);
    let lib_dest = opts.directory.join(&opts.lib_dir);

    let mut copied: Vec<(String, PathBuf, u64, String)> = Vec::new();
    let mut not_found: Vec<(String, String)> = Vec::new();
    let mut already_processed: HashSet<String> = HashSet::new();
    let mut expanded_nix: HashSet<PathBuf> = HashSet::new();
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

                // On NixOS: expand cache with this store path's closure
                // so transitive deps (e.g. libsndfile for libpulsecommon) are found
                expand_nix_cache(&resolved, &mut ldconfig_cache, &mut expanded_nix);

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
                    if let Err(e) = strip_rpath(&dest) {
                        eprintln!(
                            "  {} failed to strip rpath from {}: {e}",
                            color::bold_red("warning:"),
                            soname
                        );
                    }
                    if opts.strip {
                        strip_debug(&dest);
                    }
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
                    if let Ok(transitive) = parse_needed(&resolved) {
                        for dep in transitive {
                            if !already_processed.contains(&dep)
                                && !is_excluded(&dep, &excludes)
                                && !existing.contains(&dep)
                            {
                                needed_by
                                    .entry(dep.clone())
                                    .or_insert_with(|| soname.clone());
                                queue.push(dep);
                            }
                        }
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

    // Strip RPATHs from all ELF files in the directory for portability.
    // Hardcoded absolute paths (e.g. /nix/store/...) won't exist on the
    // target system; LD_LIBRARY_PATH (set by the runtime) is used instead.
    if !opts.dry_run {
        let mut stripped = 0usize;
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
            if strip_rpath(&path).is_ok() {
                stripped += 1;
            }
            if needs_chmod {
                let _ =
                    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(perms));
            }
        }
        if stripped > 0 {
            eprintln!(
                "{} RPATHs from {} binaries",
                color::bold("Stripped"),
                stripped
            );
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

fn parse_needed(path: &Path) -> io::Result<Vec<String>> {
    let data = fs::read(path)?;
    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(elf.libraries.iter().map(|s| s.to_string()).collect())
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

/// Strip RPATH/RUNPATH from an ELF binary by zeroing the string in .dynstr.
/// This makes the dynamic linker rely on LD_LIBRARY_PATH instead of hardcoded paths.
fn strip_rpath(path: &Path) -> io::Result<()> {
    let data = fs::read(path)?;
    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    // Find .dynstr section file offset
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
        if dyn_entry.d_tag == goblin::elf::dynamic::DT_RPATH
            || dyn_entry.d_tag == goblin::elf::dynamic::DT_RUNPATH
        {
            let file_pos = dynstr_offset + dyn_entry.d_val as usize;
            if file_pos < modified.len() && modified[file_pos] != 0 {
                // Zero out the entire rpath string
                let mut pos = file_pos;
                while pos < modified.len() && modified[pos] != 0 {
                    modified[pos] = 0;
                    pos += 1;
                }
                changed = true;
            }
        }
    }

    if changed {
        fs::write(path, &modified)?;
    }
    Ok(())
}

fn is_excluded(soname: &str, excludes: &[&str]) -> bool {
    excludes.iter().any(|pat| soname.starts_with(pat))
}

fn find_existing_libs(dir: &Path) -> HashSet<String> {
    let mut libs = HashSet::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false) {
        let Ok(entry) = entry else { continue };
        if let Some(name) = entry.path().file_name() {
            let name = name.to_string_lossy();
            if name.contains(".so") {
                libs.insert(name.into_owned());
            }
        }
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

    // 1. ldconfig cache
    if let Some(paths) = ldconfig_cache.get(soname) {
        for path in paths {
            if path.exists() && class_matches(path) {
                return Some(path.clone());
            }
        }
    }

    // 2. Standard paths
    for dir in STANDARD_LIB_PATHS {
        let candidate = Path::new(dir).join(soname);
        if candidate.exists() && class_matches(&candidate) {
            return Some(candidate);
        }
    }

    // 3. --search-path directories
    for dir in search_paths {
        let candidate = dir.join(soname);
        if candidate.exists() && class_matches(&candidate) {
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
                if candidate.exists() && class_matches(&candidate) {
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
                if candidate.exists() && class_matches(&candidate) {
                    return Some(candidate);
                }
                // Also check one level of subdirs (e.g. lib/pulseaudio/)
                if let Ok(subdirs) = fs::read_dir(&lib_dir) {
                    for subdir in subdirs.filter_map(Result::ok) {
                        if subdir.file_type().map_or(false, |t| t.is_dir()) {
                            let candidate = subdir.path().join(soname);
                            if candidate.exists() && class_matches(&candidate) {
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

// ---------------------------------------------------------------------------
// GPU asset bundling
// ---------------------------------------------------------------------------

const DRI_SEARCH_PATHS: &[&str] = &[
    "/usr/lib/dri",
    "/usr/lib64/dri",
    "/usr/lib/x86_64-linux-gnu/dri",
];

const GBM_SEARCH_PATHS: &[&str] = &[
    "/usr/lib/gbm",
    "/usr/lib64/gbm",
    "/usr/lib/x86_64-linux-gnu/gbm",
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

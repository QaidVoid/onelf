//! Shared library bundling for ONELF packages.
//!
//! Scans ELF binaries in a directory for shared library dependencies,
//! resolves them via ldconfig cache, standard paths, or NixOS store
//! scanning, and copies them into a lib directory for self-contained
//! packaging.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead};
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

const DEFAULT_EXCLUDES: &[&str] = &[
    "libc.so",
    "libm.so",
    "libdl.so",
    "librt.so",
    "libpthread.so",
    "libresolv.so",
    "libnss_",
    "libnsl.so",
    "libutil.so",
    "ld-linux",
    "libgcc_s.so",
    "libstdc++.so",
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
    pub search_path: Vec<PathBuf>,
    pub dry_run: bool,
    pub recursive: bool,
}

pub fn bundle_libs(opts: &BundleOptions) -> io::Result<()> {
    if !opts.directory.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{}: not a directory", opts.directory.display()),
        ));
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

    let ldconfig_cache = build_lib_cache();
    let extra_paths = &opts.search_path;
    let lib_dest = opts.directory.join(&opts.lib_dir);

    let mut copied: Vec<(String, PathBuf, u64, String)> = Vec::new();
    let mut not_found: Vec<(String, String)> = Vec::new();
    let mut already_processed: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = needed_by.keys().cloned().collect();
    queue.sort();

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

        match locate_lib(&soname, &ldconfig_cache, extra_paths) {
            Some(src) => {
                let resolved = fs::canonicalize(&src).unwrap_or(src.clone());
                let size = fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
                let dest = lib_dest.join(&soname);

                if opts.dry_run {
                    println!(
                        "  {} {} {} {}",
                        color::bold_green(&soname),
                        color::dim("<-"),
                        resolved.display(),
                        color::dim(&format!("(needed by {})", color::cyan(&requirer)))
                    );
                } else {
                    fs::create_dir_all(&lib_dest)?;
                    fs::copy(&resolved, &dest)?;
                }

                copied.push((soname.clone(), resolved.clone(), size, requirer));

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
            color::bold(&format!("{:.1} MB", total_size as f64 / 1_048_576.0))
        );
    } else if !copied.is_empty() {
        eprintln!(
            "\n{} {} libraries ({}) to {}",
            color::bold_green("Copied"),
            copied.len(),
            color::bold(&format!("{:.1} MB", total_size as f64 / 1_048_576.0)),
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

fn parse_needed(path: &Path) -> io::Result<Vec<String>> {
    let data = fs::read(path)?;
    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(elf.libraries.iter().map(|s| s.to_string()).collect())
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

fn build_lib_cache() -> HashMap<String, PathBuf> {
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

fn parse_ldconfig_cache() -> HashMap<String, PathBuf> {
    let mut cache = HashMap::new();
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
                    .or_insert_with(|| PathBuf::from(right.trim()));
            }
        }
    }
    cache
}

/// Scan lib/ directories from the NixOS system closure to build a soname map.
fn scan_nix_store_libs() -> HashMap<String, PathBuf> {
    let mut cache = HashMap::new();

    let Ok(output) = Command::new("nix-store")
        .args(["-qR", "/run/current-system"])
        .output()
    else {
        return cache;
    };

    if !output.status.success() {
        return cache;
    }

    let lib_dirs: Vec<PathBuf> = output
        .stdout
        .lines()
        .map_while(Result::ok)
        .map(|line| PathBuf::from(line.trim()).join("lib"))
        .filter(|p| p.is_dir())
        .collect();

    eprintln!(
        "{} scanning {} store paths...",
        color::dim("NixOS detected,"),
        lib_dirs.len()
    );

    for lib_dir in &lib_dirs {
        let Ok(entries) = fs::read_dir(lib_dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains(".so") {
                cache
                    .entry(name.into_owned())
                    .or_insert_with(|| entry.path());
            }
        }
    }

    cache
}

fn locate_lib(
    soname: &str,
    ldconfig_cache: &HashMap<String, PathBuf>,
    extra_paths: &[PathBuf],
) -> Option<PathBuf> {
    // 1. ldconfig cache
    if let Some(path) = ldconfig_cache.get(soname) {
        if path.exists() {
            return Some(path.clone());
        }
    }

    // 2. Standard paths
    for dir in STANDARD_LIB_PATHS {
        let candidate = Path::new(dir).join(soname);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // 3. --search-path directories
    for dir in extra_paths {
        let candidate = dir.join(soname);
        if candidate.exists() {
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
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

//! Host library resolution: ldconfig cache, Nix store closures,
//! search-path lookups, and existing-lib / stub-loader detection.

use super::*;

/// Scan `dir` for existing shared libraries. Returns the set of sonames
/// found and, separately, the paths of any NixOS stub loaders. The scan
/// never deletes: the caller removes stubs only in the non-dry-run copy
/// phase so `--dry-run` stays side-effect free.
pub(crate) fn find_existing_libs(dir: &Path) -> (HashSet<String>, Vec<PathBuf>) {
    let mut libs = HashSet::new();
    let mut stubs = Vec::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false).sort(true) {
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
        // bundle. Report it (rather than delete it here) so the caller can
        // replace it with a real loader in the copy phase; the soname is
        // treated as absent so it gets re-resolved.
        if is_nix_stub_ld(&path) {
            stubs.push(path.clone());
            continue;
        }
        libs.insert(name.into_owned());
    }
    (libs, stubs)
}

pub(crate) fn build_lib_cache() -> HashMap<String, Vec<PathBuf>> {
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

pub(crate) fn parse_ldconfig_cache() -> HashMap<String, Vec<PathBuf>> {
    let mut cache: HashMap<String, Vec<PathBuf>> = HashMap::new();
    // `ldconfig` lives in /sbin (and /usr/sbin) on Debian, which is off a
    // non-root user's PATH. Try the bare name first (honoring PATH), then
    // the sbin locations, so non-root Debian runs still read the cache.
    let output = ["ldconfig", "/sbin/ldconfig", "/usr/sbin/ldconfig"]
        .into_iter()
        .find_map(|prog| Command::new(prog).arg("-p").output().ok());
    let Some(output) = output else {
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
pub(crate) fn scan_nix_store_libs() -> HashMap<String, Vec<PathBuf>> {
    let mut cache: HashMap<String, Vec<PathBuf>> = HashMap::new();

    // The system / user / per-user Nix closure roots (shared with the
    // framework bundlers' search-path builder).
    let store_paths = nix_closure_roots();
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
        for entry in jwalk::WalkDir::new(lib_dir)
            .max_depth(3)
            .skip_hidden(false)
            .sort(true)
        {
            let Ok(entry) = entry else { continue };
            // Follow symlinks: nix-store lib dirs expose versioned sonames as
            // symlinks to the real object, and `file_type()` (readdir, no
            // follow) would skip them, hiding those libs from resolution.
            if !entry.path().is_file() {
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
pub(crate) fn nix_store_path(path: &Path) -> Option<PathBuf> {
    let s = path.to_string_lossy();
    let rest = s.strip_prefix("/nix/store/")?;
    let end = rest.find('/').unwrap_or(rest.len());
    Some(PathBuf::from(format!("/nix/store/{}", &rest[..end])))
}

/// When a lib is resolved from the nix store, scan its store path's closure
/// to discover transitive dependencies that may not be in the initial scan set.
/// Tracks already-expanded store paths to avoid redundant work.
pub(crate) fn expand_nix_cache(
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
            .sort(true)
        {
            let Ok(entry) = entry else { continue };
            // Follow symlinks: nix-store lib dirs expose versioned sonames as
            // symlinks to the real object, and `file_type()` (readdir, no
            // follow) would skip them, hiding those libs from resolution.
            if !entry.path().is_file() {
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

pub(crate) fn locate_lib(
    soname: &str,
    ldconfig_cache: &HashMap<String, Vec<PathBuf>>,
    search_paths: &[PathBuf],
    target_class: Option<u8>,
    target_machine: Option<u16>,
) -> Option<PathBuf> {
    let class_matches = |path: &Path| -> bool {
        match target_class {
            Some(tc) => read_elf_class(path) == Some(tc),
            None => true,
        }
    };
    let machine_matches = |path: &Path| -> bool {
        match target_machine {
            Some(tm) => read_elf_machine(path) == Some(tm),
            None => true,
        }
    };
    // Reject NixOS's stub loader anywhere it surfaces. It exists on disk
    // but refuses to actually load foreign binaries, so bundling it would
    // produce a package that runs nowhere. Also reject a wrong-architecture
    // library from a multiarch or cross sysroot.
    let acceptable =
        |path: &Path| class_matches(path) && machine_matches(path) && !is_nix_stub_ld(path);

    // 1. --search-path directories (user-provided: highest priority)
    for dir in search_paths {
        let candidate = dir.join(soname);
        if candidate.exists() && acceptable(&candidate) {
            return Some(candidate);
        }
    }

    // 2. ldconfig cache. Sort candidates so the first acceptable match is
    // stable when several store paths ship the same soname (the cache is
    // filled from unordered HashSet + walk order).
    if let Some(paths) = ldconfig_cache.get(soname) {
        let mut sorted: Vec<&PathBuf> = paths.iter().collect();
        sorted.sort();
        for path in sorted {
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

    // 5. NixOS fallback: scan /nix/store/*/lib/ directly. Sort the store
    // entries (and subdirs) so that when multiple store paths provide the
    // same soname, the chosen copy is deterministic rather than dependent
    // on read_dir order.
    if Path::new("/nix/store").is_dir()
        && let Ok(entries) = fs::read_dir("/nix/store")
    {
        let mut dirs: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let lib_dir = dir.join("lib");
            // Check lib/<soname> directly
            let candidate = lib_dir.join(soname);
            if candidate.exists() && acceptable(&candidate) {
                return Some(candidate);
            }
            // Also check one level of subdirs (e.g. lib/pulseaudio/)
            if let Ok(subdirs) = fs::read_dir(&lib_dir) {
                let mut subs: Vec<PathBuf> = subdirs
                    .filter_map(Result::ok)
                    .filter(|s| s.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|s| s.path())
                    .collect();
                subs.sort();
                for subdir in subs {
                    let candidate = subdir.join(soname);
                    if candidate.exists() && acceptable(&candidate) {
                        return Some(candidate);
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
pub(crate) fn is_nix_stub_ld(path: &Path) -> bool {
    // Only a dynamic-loader-shaped file can be a stub loader. Reject any
    // non-`ld-*` filename up front, before the canonicalize shortcut, so a
    // path that merely contains "stub-ld" (or the phrase in its content)
    // can never be misclassified and, at the call site, deleted.
    let is_loader_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("ld-"));
    if !is_loader_name {
        return false;
    }
    if let Ok(real) = fs::canonicalize(path)
        && real.to_string_lossy().contains("stub-ld")
    {
        return true;
    }
    // Real glibc ld-linux is >100 KB; the stub is ~35 KB. Cheap filter
    // before reading the file content.
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > 128 * 1024 {
        return false;
    }
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    if bytes.len() < 4 || bytes[..4] != *b"\x7fELF" {
        return false;
    }
    bytes
        .windows(b"NixOS cannot run".len())
        .any(|w| w == b"NixOS cannot run")
}

/// Collect nix store paths from system and user closures.
pub(crate) fn nix_closure_roots() -> Vec<String> {
    let mut store_paths: HashSet<String> = HashSet::new();

    let mut query_paths: Vec<PathBuf> = vec![PathBuf::from("/run/current-system")];
    if let Ok(home) = std::env::var("HOME") {
        query_paths.push(PathBuf::from(format!("{home}/.nix-profile")));
    }
    // `/etc/profiles/per-user` is a parent directory, not a store path;
    // query each per-user profile subdirectory individually.
    if let Ok(entries) = fs::read_dir("/etc/profiles/per-user") {
        for entry in entries.flatten() {
            query_paths.push(entry.path());
        }
    }

    for qp in &query_paths {
        if !qp.exists() {
            continue;
        }
        let Ok(output) = Command::new("nix-store").arg("-qR").arg(qp).output() else {
            continue;
        };
        if output.status.success() {
            for line in output.stdout.lines().map_while(Result::ok) {
                store_paths.insert(line.trim().to_string());
            }
        }
    }

    // Sort so callers that pick a first match over duplicate sonames are
    // deterministic regardless of HashSet iteration order.
    let mut result: Vec<String> = store_paths.into_iter().collect();
    result.sort();
    result
}

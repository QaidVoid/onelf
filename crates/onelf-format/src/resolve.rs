//! Per-library choice between the bundle and the host, made at launch.
//!
//! A bundle that reaches a host GPU driver has to load that driver's
//! dependencies too, and the host built them against its own glibc and
//! libstdc++. Whether they can share a process with the bundled copies
//! depends on which side is newer, and that differs per library, so the
//! answer cannot be a directory order. Each soname present on both sides
//! is compared by the versions it defines and the superset wins.
//!
//! The loader and libc are a unit: a libc only runs under the loader it
//! shipped with. They are compared first, and when the host's pair wins
//! the whole glibc family comes from the host so nothing bundled is loaded
//! beside a foreign loader.
//!
//! Winners from the host are materialized as symlinks in a link farm that
//! goes first on the library path. No host directory is ever on the path,
//! so a soname the bundle lacks and the resolver did not choose fails by
//! name instead of being satisfied by whatever the host has.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::drivers::{self, DRIVER_FAMILIES};
use crate::footer::HostLibsPolicy;
use crate::verdef::{self, Choice, VersionSet};

/// What a launch takes from the host.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Directory of symlinks to the chosen host libraries, when any were
    /// chosen. Goes first on the library path.
    pub farm: Option<PathBuf>,
    /// The host loader to run the entrypoint under, when the host's libc
    /// won. The bundled loader is used otherwise.
    pub host_interp: Option<PathBuf>,
    /// Sonames whose two copies could not be ordered. The bundled copy is
    /// used; these are reported so the publisher can see them.
    pub incomparable: Vec<String>,
}

/// One launch's inputs.
pub struct Request<'a> {
    /// The package root as the entrypoint will see it.
    pub pkg_root: &'a Path,
    /// Library directories relative to `pkg_root`.
    pub lib_dirs: &'a [&'a str],
    pub policy: HostLibsPolicy,
    /// Where the link farm and the recorded decision live. Created on
    /// demand; reused across launches of the same package on the same
    /// host.
    pub store: &'a Path,
    /// The loader cache image describing the host.
    pub ld_cache: &'a Path,
    /// Directories of Vulkan ICD and EGL vendor files describing the
    /// host's drivers. [`ICD_DIRS`] outside of tests.
    pub icd_dirs: &'a [&'a str],
}

const LIBC: &str = "libc.so.6";

/// The glibc family: everything that must come from the same build as
/// the loader. Matched as soname prefixes.
const LIBC_FAMILY: &[&str] = &[
    "libc.so",
    "libm.so",
    "libpthread.so",
    "libdl.so",
    "librt.so",
    "libresolv.so",
    "libutil.so",
    "libanl.so",
    "libnss_",
    "libBrokenLocale.so",
    "libmvec.so",
    "libc_malloc_debug.so",
    "libthread_db.so",
    "ld-linux",
];

/// Where the host records the graphics drivers it loads by absolute path.
/// Those never appear as `DT_NEEDED`, so the walk has to be seeded from
/// here as well as from the driver families.
pub const ICD_DIRS: &[&str] = &[
    "/run/opengl-driver/share/vulkan/icd.d",
    "/etc/vulkan/icd.d",
    "/usr/share/vulkan/icd.d",
    "/run/opengl-driver/share/glvnd/egl_vendor.d",
    "/etc/glvnd/egl_vendor.d",
    "/usr/share/glvnd/egl_vendor.d",
];

fn is_libc_family(soname: &str) -> bool {
    LIBC_FAMILY.iter().any(|p| soname.starts_with(p))
}

fn is_driver_family(soname: &str) -> bool {
    DRIVER_FAMILIES.iter().any(|p| soname.starts_with(p))
}

/// Decide what this launch takes from the host.
pub fn resolve(req: &Request) -> Resolution {
    if req.policy == HostLibsPolicy::Never {
        return Resolution::default();
    }
    let host = HostIndex::load(req.ld_cache, req.icd_dirs);
    if host.libs.is_empty() {
        return Resolution::default();
    }
    let fingerprint = fingerprint(req.ld_cache, req.icd_dirs, req.policy);
    if let Some(recorded) = load_recorded(req.store, &fingerprint) {
        return recorded;
    }

    let bundled = bundled_libs(req.pkg_root, req.lib_dirs);
    let mut winners: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut incomparable = Vec::new();
    let mut host_interp = None;

    if let (Some(bundled_libc), Some(host_libc)) = (bundled.get(LIBC), host.libs.get(LIBC)) {
        match verdef::compare(&versions(bundled_libc), &versions(host_libc)) {
            Choice::Host => {
                if let Some(interp) = host.loader_for(host_libc) {
                    for name in bundled.keys().filter(|n| is_libc_family(n)) {
                        let path = if name.starts_with("ld-linux") {
                            Some(interp.clone())
                        } else {
                            host.libs.get(name).cloned()
                        };
                        if let Some(path) = path {
                            winners.insert(name.clone(), path);
                        }
                    }
                    host_interp = Some(interp);
                }
            }
            Choice::Incomparable => incomparable.push(LIBC.to_string()),
            Choice::Bundle => {}
        }
    }

    // The driver closure is what the host is expected to supply. A member
    // the bundle also carries is compared; one it does not carry can only
    // come from the host. Under `always` every bundled soname is compared
    // as well.
    let mut candidates: Vec<String> = host.driver_closure().into_iter().collect();
    if req.policy == HostLibsPolicy::Always {
        candidates.extend(bundled.keys().cloned());
    }
    candidates.sort();
    candidates.dedup();
    for name in candidates {
        if is_libc_family(&name) {
            continue;
        }
        let Some(host_path) = host.libs.get(&name) else {
            continue;
        };
        let choice = match bundled.get(&name) {
            Some(bundled_path) => verdef::compare(&versions(bundled_path), &versions(host_path)),
            None => Choice::Host,
        };
        match choice {
            Choice::Host => {
                winners.insert(name, host_path.clone());
            }
            Choice::Incomparable => incomparable.push(name),
            Choice::Bundle => {}
        }
    }

    let farm = match materialize(req.store, &winners) {
        Ok(farm) => farm,
        Err(e) => {
            eprintln!(
                "onelf-rt: resolver: cannot write {}: {e}",
                req.store.display()
            );
            return Resolution::default();
        }
    };
    let resolution = Resolution {
        farm,
        host_interp,
        incomparable,
    };
    if let Err(e) = record(req.store, &fingerprint, &resolution, &winners) {
        eprintln!(
            "onelf-rt: resolver: cannot record {}: {e}",
            req.store.display()
        );
    }
    resolution
}

/// A library that cannot be read is treated as defining nothing, so the
/// bundled copy wins by default.
fn versions(path: &Path) -> VersionSet {
    verdef::read(path).unwrap_or_default()
}

/// The host's libraries by soname, from its loader cache and the drivers
/// its ICD files name by path.
struct HostIndex {
    libs: BTreeMap<String, PathBuf>,
    /// Drivers reached through ICD files rather than the cache. They seed
    /// the closure walk alongside the driver families.
    icd_libs: Vec<PathBuf>,
}

impl HostIndex {
    fn load(ld_cache: &Path, icd_dirs: &[&str]) -> Self {
        let mut libs = BTreeMap::new();
        for (soname, path) in drivers::cache_paths_in(ld_cache) {
            libs.entry(soname).or_insert_with(|| PathBuf::from(path));
        }
        let icd_libs = icd_library_paths(icd_dirs);
        for path in &icd_libs {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                libs.entry(name.to_string()).or_insert_with(|| path.clone());
            }
        }
        HostIndex { libs, icd_libs }
    }

    /// The loader the host's libc was built with: its own `PT_INTERP`,
    /// or failing that any loader the cache names.
    fn loader_for(&self, host_libc: &Path) -> Option<PathBuf> {
        if let Some(interp) = crate::elf::read_interp(host_libc) {
            let interp = PathBuf::from(interp);
            if interp.is_file() {
                return Some(interp);
            }
        }
        self.libs
            .iter()
            .find(|(name, path)| name.starts_with("ld-linux") && path.is_file())
            .map(|(_, path)| path.clone())
    }

    /// Every soname reachable from the driver families through
    /// `DT_NEEDED`, seeds included.
    fn driver_closure(&self) -> HashSet<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = self
            .libs
            .keys()
            .filter(|name| is_driver_family(name))
            .cloned()
            .collect();
        for path in &self.icd_libs {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                queue.push_back(name.to_string());
            }
        }
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(path) = self.libs.get(&name) else {
                continue;
            };
            for needed in verdef::read_needed(path).unwrap_or_default() {
                if !seen.contains(&needed) {
                    queue.push_back(needed);
                }
            }
        }
        seen
    }
}

/// `library_path` values from the host's Vulkan ICD and EGL vendor files
/// that name a file by path. A bare soname there is found through the
/// cache like any other and needs no entry.
fn icd_library_paths(icd_dirs: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in icd_dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(value) = json_library_path(&text) else {
                continue;
            };
            if !value.contains('/') {
                continue;
            }
            let lib = if value.starts_with('/') {
                PathBuf::from(value)
            } else {
                path.parent().unwrap_or(Path::new("/")).join(value)
            };
            if lib.is_file() {
                out.push(lib);
            }
        }
    }
    out
}

/// The string value of the first `"library_path"` key in `text`. The files
/// are small and regular enough that a scan is safer than a parser the
/// runtime would otherwise not need.
fn json_library_path(text: &str) -> Option<&str> {
    let at = text.find("\"library_path\"")?;
    let rest = &text[at + "\"library_path\"".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// The shared objects the bundle carries in its library directories, by
/// soname.
fn bundled_libs(pkg_root: &Path, lib_dirs: &[&str]) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    for dir in lib_dirs {
        let Ok(entries) = fs::read_dir(pkg_root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if !name.contains(".so") {
                continue;
            }
            let path = entry.path();
            if path.is_file() {
                out.entry(name).or_insert(path);
            }
        }
    }
    out
}

/// A description of everything on the host the decision depends on: the
/// cache image, the directories it names, and the driver description
/// files, each with its size and modification time. Compared whole, so
/// no hash is needed and nothing has to be added to the format crate's
/// dependencies.
fn fingerprint(ld_cache: &Path, icd_dirs: &[&str], policy: HostLibsPolicy) -> String {
    let mut out = format!("onelf-resolve-1 {}", policy.as_str());
    let mut stamp = |path: &Path| {
        out.push('\x1f');
        out.push_str(&path.to_string_lossy());
        match fs::metadata(path) {
            Ok(md) => {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                out.push_str(&format!("={}:{mtime}", md.len()));
            }
            Err(_) => out.push_str("=missing"),
        }
    };
    stamp(ld_cache);
    for dir in drivers::cache_dirs_in(ld_cache) {
        stamp(Path::new(&dir));
    }
    for dir in icd_dirs {
        stamp(Path::new(dir));
    }
    out
}

const DECISION_FILE: &str = "decision";
const FARM_DIR: &str = "farm";

/// Rebuild the link farm from `winners`. An empty set leaves no farm.
fn materialize(store: &Path, winners: &BTreeMap<String, PathBuf>) -> io::Result<Option<PathBuf>> {
    let farm = store.join(FARM_DIR);
    match fs::remove_dir_all(&farm) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if winners.is_empty() {
        return Ok(None);
    }
    fs::create_dir_all(&farm)?;
    for (name, target) in winners {
        std::os::unix::fs::symlink(target, farm.join(name))?;
    }
    Ok(Some(farm))
}

fn record(
    store: &Path,
    fingerprint: &str,
    resolution: &Resolution,
    winners: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    fs::create_dir_all(store)?;
    let mut text = String::new();
    text.push_str("onelf-resolve 1\n");
    text.push_str(&format!("fingerprint {fingerprint}\n"));
    if let Some(interp) = &resolution.host_interp {
        text.push_str(&format!("interp {}\n", interp.display()));
    }
    for (name, path) in winners {
        text.push_str(&format!("host {name}\t{}\n", path.display()));
    }
    for name in &resolution.incomparable {
        text.push_str(&format!("incomparable {name}\n"));
    }
    let tmp = store.join(format!(".{DECISION_FILE}.{}", std::process::id()));
    fs::File::create(&tmp)?.write_all(text.as_bytes())?;
    fs::rename(&tmp, store.join(DECISION_FILE))
}

/// The recorded decision, when it was made for this exact host state and
/// everything it names still exists.
fn load_recorded(store: &Path, fingerprint: &str) -> Option<Resolution> {
    let text = fs::read_to_string(store.join(DECISION_FILE)).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "onelf-resolve 1" {
        return None;
    }
    if lines.next()?.strip_prefix("fingerprint ")? != fingerprint {
        return None;
    }
    let farm = store.join(FARM_DIR);
    let mut resolution = Resolution::default();
    for line in lines {
        if let Some(interp) = line.strip_prefix("interp ") {
            let interp = PathBuf::from(interp);
            if !interp.is_file() {
                return None;
            }
            resolution.host_interp = Some(interp);
        } else if let Some(rest) = line.strip_prefix("host ") {
            let (name, path) = rest.split_once('\t')?;
            let link = farm.join(name);
            if !Path::new(path).is_file() || fs::read_link(&link).ok()? != Path::new(path) {
                return None;
            }
            resolution.farm = Some(farm.clone());
        } else {
            let name = line.strip_prefix("incomparable ")?;
            resolution.incomparable.push(name.to_string());
        }
    }
    Some(resolution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdef::fixture::{shared_object, shared_object_with_interp, temp_dir};

    /// A host with a library directory and a loader cache naming its
    /// contents, and a package root with one `lib/` directory.
    struct World {
        host_lib: PathBuf,
        cache: PathBuf,
        pkg: PathBuf,
        store: PathBuf,
    }

    impl World {
        fn new() -> Self {
            let root = temp_dir("resolve");
            let host_lib = root.join("host");
            let pkg = root.join("pkg");
            fs::create_dir_all(&host_lib).unwrap();
            fs::create_dir_all(pkg.join("lib")).unwrap();
            World {
                cache: root.join("ld.so.cache"),
                store: root.join("store"),
                host_lib,
                pkg,
            }
        }

        fn host(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.host_lib.join(name);
            fs::write(&path, bytes).unwrap();
            self.write_cache();
            path
        }

        fn bundle(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.pkg.join("lib").join(name);
            fs::write(&path, bytes).unwrap();
            path
        }

        /// Rewrite the cache image from the host directory's contents.
        fn write_cache(&self) {
            let mut names: Vec<String> = fs::read_dir(&self.host_lib)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let paths: Vec<String> = names
                .iter()
                .map(|n| self.host_lib.join(n).to_string_lossy().into_owned())
                .collect();
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            fs::write(&self.cache, new_cache(&refs)).unwrap();
        }

        fn resolve(&self, policy: HostLibsPolicy) -> Resolution {
            resolve(&Request {
                pkg_root: &self.pkg,
                lib_dirs: &["lib"],
                policy,
                store: &self.store,
                ld_cache: &self.cache,
                icd_dirs: &[],
            })
        }

        fn farm_link(&self, name: &str) -> Option<PathBuf> {
            fs::read_link(self.store.join(FARM_DIR).join(name)).ok()
        }
    }

    /// A new-format loader cache image naming `paths`.
    fn new_cache(paths: &[&str]) -> Vec<u8> {
        let mut out = Vec::from(&b"glibc-ld.so.cache1.1"[..]);
        out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0; 12]);
        let strings_at = 48 + paths.len() * 24;
        let mut strings: Vec<u8> = Vec::new();
        for p in paths {
            let off = (strings_at + strings.len()) as u32;
            out.extend_from_slice(&0i32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
            strings.extend_from_slice(p.as_bytes());
            strings.push(0);
        }
        out.extend_from_slice(&strings);
        out
    }

    fn versioned(names: &[&str]) -> Vec<u8> {
        let defs: Vec<(u16, &str)> = names.iter().map(|n| (0, *n)).collect();
        shared_object(&defs, &[])
    }

    fn driver_needing(needed: &[&str]) -> Vec<u8> {
        shared_object(&[], needed)
    }

    #[test]
    fn a_newer_host_dependency_of_a_driver_wins() {
        let w = World::new();
        let host_gl = w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));

        let r = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(r.farm, Some(w.store.join(FARM_DIR)));
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo));
        // The driver itself is not bundled, so it can only come from the
        // host, and it must be reachable without a host directory on the
        // search path.
        assert_eq!(w.farm_link("libGL.so.1"), Some(host_gl));
        assert!(r.host_interp.is_none());
        assert!(r.incomparable.is_empty());
    }

    #[test]
    fn a_driver_dependency_the_bundle_lacks_comes_from_the_host() {
        let w = World::new();
        w.host("libvulkan.so.1", &driver_needing(&["libdrm.so.2"]));
        let host_drm = w.host("libdrm.so.2", &versioned(&[]));
        w.host("libunrelated.so.1", &versioned(&[]));
        w.bundle("libapp.so.1", &versioned(&[]));

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libdrm.so.2"), Some(host_drm));
        assert!(w.farm_link("libunrelated.so.1").is_none());
        assert!(w.farm_link("libapp.so.1").is_none());
    }

    #[test]
    fn auto_ignores_libraries_outside_the_driver_closure() {
        let w = World::new();
        let host_bar = w.host("libbar.so.1", &versioned(&["BAR_1.0", "BAR_2.0"]));
        w.bundle("libbar.so.1", &versioned(&["BAR_1.0"]));

        assert_eq!(w.resolve(HostLibsPolicy::Auto), Resolution::default());
        assert!(w.farm_link("libbar.so.1").is_none());

        let r = w.resolve(HostLibsPolicy::Always);
        assert!(r.farm.is_some());
        assert_eq!(w.farm_link("libbar.so.1"), Some(host_bar));
    }

    #[test]
    fn a_newer_bundled_copy_stays() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        w.host("libfoo.so.1", &versioned(&["FOO_1.0"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libfoo.so.1").is_none());
        assert!(w.farm_link("libGL.so.1").is_some());
    }

    #[test]
    fn unversioned_and_unreadable_host_copies_stay_bundled() {
        let w = World::new();
        w.host(
            "libGL.so.1",
            &driver_needing(&["libfoo.so.1", "libbaz.so.1"]),
        );
        w.host("libfoo.so.1", &versioned(&[]));
        w.bundle("libfoo.so.1", &versioned(&[]));
        let broken = versioned(&["BAZ_9.0"]);
        w.host("libbaz.so.1", &broken[..broken.len() / 2]);
        w.bundle("libbaz.so.1", &versioned(&["BAZ_1.0"]));
        let r = w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libfoo.so.1").is_none());
        assert!(w.farm_link("libbaz.so.1").is_none());
        assert!(r.incomparable.is_empty());
    }

    #[test]
    fn incomparable_copies_stay_bundled_and_are_named() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        w.host("libfoo.so.1", &versioned(&["FOO_1.0", "VENDOR_1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        let r = w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libfoo.so.1").is_none());
        assert_eq!(r.incomparable, ["libfoo.so.1"]);
    }

    #[test]
    fn never_takes_nothing() {
        let w = World::new();
        w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        assert_eq!(w.resolve(HostLibsPolicy::Never), Resolution::default());
        assert!(!w.store.exists());
    }

    #[test]
    fn closure_reaches_transitive_dependencies_and_stops_at_missing_ones() {
        let w = World::new();
        w.host(
            "libvulkan.so.1",
            &driver_needing(&["libdrm.so.2", "libgone.so.0"]),
        );
        w.host("libdrm.so.2", &driver_needing(&["libz.so.1"]));
        w.host("libz.so.1", &versioned(&[]));
        w.host("libunrelated.so.1", &versioned(&[]));
        let closure = HostIndex::load(&w.cache, &[]).driver_closure();
        for name in ["libvulkan.so.1", "libdrm.so.2", "libz.so.1", "libgone.so.0"] {
            assert!(closure.contains(name), "{name} missing from {closure:?}");
        }
        assert!(!closure.contains("libunrelated.so.1"));
    }

    #[test]
    fn a_newer_host_glibc_brings_its_loader_and_family() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &versioned(&["GLIBC_2.2.5"]));
        let host_libc = w.host(
            "libc.so.6",
            &shared_object_with_interp(
                &[(0, "GLIBC_2.2.5"), (0, "GLIBC_2.34")],
                &[],
                Some(host_ld.to_str().unwrap()),
            ),
        );
        let host_libm = w.host("libm.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]));
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.bundle("libm.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.bundle("ld-linux-x86-64.so.2", &versioned(&["GLIBC_2.2.5"]));
        w.bundle("libother.so.1", &versioned(&["OTHER_1"]));

        let r = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(r.host_interp, Some(host_ld.clone()));
        assert_eq!(w.farm_link("libc.so.6"), Some(host_libc));
        assert_eq!(w.farm_link("libm.so.6"), Some(host_libm));
        assert_eq!(w.farm_link("ld-linux-x86-64.so.2"), Some(host_ld));
        assert!(w.farm_link("libother.so.1").is_none());
    }

    #[test]
    fn an_older_or_equal_host_glibc_keeps_the_bundled_pair() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &versioned(&[]));
        w.host(
            "libc.so.6",
            &shared_object_with_interp(&[(0, "GLIBC_2.2.5")], &[], Some(host_ld.to_str().unwrap())),
        );
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]));
        assert_eq!(w.resolve(HostLibsPolicy::Always), Resolution::default());

        w.bundle("libc.so.6", &versioned(&["GLIBC_2.2.5"]));
        assert_eq!(w.resolve(HostLibsPolicy::Always), Resolution::default());
    }

    #[test]
    fn a_host_without_glibc_takes_nothing_for_the_pair() {
        let w = World::new();
        w.host("libc.musl-x86_64.so.1", &versioned(&[]));
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.2.5"]));
        let r = w.resolve(HostLibsPolicy::Always);
        assert!(r.host_interp.is_none());
        assert!(r.farm.is_none());
    }

    #[test]
    fn the_decision_is_recorded_and_reused_until_the_host_changes() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        let first = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));

        // Rewriting the file in place changes neither the cache nor the
        // directory, so the recorded decision still stands.
        fs::write(&host_foo, versioned(&["FOO_1.0"])).unwrap();
        assert_eq!(w.resolve(HostLibsPolicy::Auto), first);

        // A new entry in the directory is a host change; the fresh
        // comparison now sees an equal host copy and keeps the bundle.
        fs::write(w.host_lib.join("libnew.so.1"), versioned(&[])).unwrap();
        bump_mtime(&w.host_lib);
        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libfoo.so.1").is_none());
    }

    #[test]
    fn a_vanished_winner_recomputes_instead_of_failing() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        w.resolve(HostLibsPolicy::Auto);

        let before = fs::metadata(&w.host_lib).unwrap().modified().unwrap();
        fs::remove_file(&host_foo).unwrap();
        fs::File::open(&w.host_lib)
            .unwrap()
            .set_modified(before)
            .unwrap();
        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libfoo.so.1").is_none());
        assert!(w.farm_link("libGL.so.1").is_some());
    }

    #[test]
    fn library_path_is_read_from_icd_json() {
        assert_eq!(
            json_library_path(
                "{\n  \"file_format_version\": \"1.0.0\",\n  \"ICD\": {\n    \"library_path\": \"/usr/lib64/libvulkan_radeon.so\",\n    \"api_version\": \"1.3\"\n  }\n}"
            ),
            Some("/usr/lib64/libvulkan_radeon.so")
        );
        assert_eq!(
            json_library_path("{\"ICD\":{\"library_path\":\"libEGL_nvidia.so.0\"}}"),
            Some("libEGL_nvidia.so.0")
        );
        assert_eq!(json_library_path("{}"), None);
    }

    /// Move a directory's mtime well past "now", so a change made within
    /// the same filesystem timestamp granularity still reads as a change.
    fn bump_mtime(dir: &Path) {
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::open(dir).unwrap().set_modified(later).unwrap();
    }
}

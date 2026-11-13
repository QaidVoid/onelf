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

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufRead, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use onelf_sysroot::{Database, PlatformLine, Policy, Trace};

use crate::recipe;

pub(crate) mod ui;
pub(crate) use ui::{color, format_size};
mod datadirs;
mod discover;
pub(crate) mod gpu;
mod infer;
use gpu::{bundle_gpu, bundle_gtk_data, bundle_wayland};
mod activate;
mod collect;
mod completeness;
mod plan;
mod redir;
mod resolve;
pub(crate) use resolve::*;
pub(crate) mod elf;
pub mod sysroot;
use activate::{Discovery, WorkItem, publish_activations};
pub use activate::{RuleActivation, take_activations};
pub(crate) use completeness::record as record_finding;
pub use completeness::{Finding, Outcome, take_reported as take_findings};
pub(crate) use elf::*;
use onelf_format::paths::PathPair;
use onelf_format::plugindirs::{self, PluginDir};
use onelf_format::rules::{
    BuildHookId, Descriptor, HelperGroup, Requirement, ResourceGroup, ResourceKind, RuntimeBase,
    RuntimePathGroup, SourceBase, SourceCandidate,
};
use plan::{Admission, DestinationPlan, Existing};
pub use sysroot::SysrootOptions;

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

/// A fresh directory under the system temp dir that only this user can
/// enter, for scratch a host tool writes into.
///
/// A predictable name there can be planted by any local user beforehand,
/// as a directory they own or a symlink to somewhere of the victim's.
/// `mkdir` creates the final component itself or fails, never following
/// what is already there, so a name that is taken is simply passed over,
/// and the directory it does create is this process's with mode 0700.
pub(crate) fn private_temp_dir(tag: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    for _ in 0..100 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "onelf-{tag}-{}-{nanos:08x}{n:x}",
            std::process::id()
        ));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free scratch directory name under {}", base.display()),
    ))
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

/// Whether `dir` may answer a repair for a soname the tree already
/// carries: a directory of the tree itself, an author-declared search
/// path, or, in a sysroot build, a directory of the sysroot's own
/// filesystem. An installation the host happens to have is none of
/// these, whatever form the build takes, so the sources behind the
/// declared ones never resolve one.
fn repair_admits(
    dir: &Path,
    tree_root: &Path,
    tree_dir: &Path,
    declared: &[PathBuf],
    sysroot_root: Option<&Path>,
) -> bool {
    if dir.starts_with(tree_root) || dir.starts_with(tree_dir) {
        return true;
    }
    if declared.iter().any(|path| path == dir) {
        return true;
    }
    sysroot_root.is_some_and(|root| dir.starts_with(root))
}

/// The host object a tree-resident library a host scan admitted stands
/// for: the file a framework copier copied it from when one did, or else
/// the file `lookup`, the host search the walk resolves an absent soname
/// with, finds. A candidate inside one of the `tree` roots is the tree
/// itself, and one linking another libc family than `target_libc` is
/// not the installation the bundle runs with, so neither answers.
/// Without a `target_libc` the candidate is not read at all.
fn host_provenance(
    copied_from: Option<&Path>,
    tree: &[&Path],
    target_libc: Option<LibcFamily>,
    lookup: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let found = copied_from.map(Path::to_path_buf).or_else(lookup)?;
    let real = fs::canonicalize(&found).unwrap_or(found);
    if tree.iter().any(|root| real.starts_with(root)) {
        return None;
    }
    let Some(want) = target_libc else {
        return Some(real);
    };
    let family = parse_needed(&real)
        .unwrap_or_default()
        .iter()
        .find_map(|d| libc_family_of_soname(d));
    match family {
        Some(have) if have != want => None,
        _ => Some(real),
    }
}

/// Admit a host scan's tree copy of a rule's library at `copy`, with
/// the host object it stands for as its provenance when one answers,
/// see [`host_twin`].
fn admit_tree_copy(
    soname: &str,
    copy: &Path,
    copied_from: &HashMap<String, PathBuf>,
    tree: &[&Path],
    search_paths: &[PathBuf],
    ldconfig_cache: &HashMap<String, Vec<PathBuf>>,
    (class, machine, libc): (Option<u8>, Option<u16>, Option<LibcFamily>),
) -> AdmittedLibrary {
    let library = AdmittedLibrary::new(soname, copy);
    let twin = host_twin(
        soname,
        copied_from,
        tree,
        &host_dirs(search_paths, tree),
        ldconfig_cache,
        (class, machine, libc),
        LookupScope::Host,
    );
    match twin {
        Some(twin) => library.with_provenance(twin),
        None => library,
    }
}

/// The search paths outside the `tree` roots, which a host twin is
/// looked up in.
fn host_dirs(search_paths: &[PathBuf], tree: &[&Path]) -> Vec<PathBuf> {
    search_paths
        .iter()
        .filter(|d| !tree.iter().any(|root| d.starts_with(root)))
        .cloned()
        .collect()
}

/// The host object a host scan's tree copy of `soname` stands for: the
/// file a framework copier took it from, or the compatible file the
/// soname resolves to through `host_dirs` and the machine sources
/// `scope` allows.
fn host_twin(
    soname: &str,
    copied_from: &HashMap<String, PathBuf>,
    tree: &[&Path],
    host_dirs: &[PathBuf],
    ldconfig_cache: &HashMap<String, Vec<PathBuf>>,
    (class, machine, libc): (Option<u8>, Option<u16>, Option<LibcFamily>),
    scope: LookupScope,
) -> Option<PathBuf> {
    host_provenance(
        copied_from.get(soname).map(PathBuf::as_path),
        tree,
        libc,
        || {
            locate_lib(
                soname,
                ldconfig_cache,
                host_dirs,
                class,
                machine,
                None,
                scope,
            )
        },
    )
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    /// The copier's recorded source wins over the host search, a
    /// candidate inside the tree is never a provenance, and one of
    /// another libc family than the bundle's is refused.
    #[test]
    fn provenance_prefers_the_copy_source_and_refuses_tree_and_libc_mismatch() {
        let dir =
            std::env::temp_dir().join(format!("onelf-twin-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let tree = dir.join("appdir");
        let host = dir.join("host/usr/lib");
        fs::create_dir_all(tree.join("lib")).unwrap();
        fs::create_dir_all(&host).unwrap();
        fs::write(tree.join("lib/libX11.so.6"), b"tree").unwrap();
        fs::write(host.join("libX11.so.6"), b"host").unwrap();
        fs::write(host.join("libxkbcommon.so.0"), b"copied").unwrap();
        let tree_root = fs::canonicalize(&tree).unwrap();
        let host = fs::canonicalize(&host).unwrap();
        let roots = [tree_root.as_path()];

        assert_eq!(
            host_provenance(Some(&host.join("libxkbcommon.so.0")), &roots, None, || {
                panic!("a recorded copy source needs no host search")
            }),
            Some(host.join("libxkbcommon.so.0"))
        );
        assert_eq!(
            host_provenance(None, &roots, None, || Some(host.join("libX11.so.6"))),
            Some(host.join("libX11.so.6"))
        );
        assert_eq!(
            host_provenance(None, &roots, None, || Some(
                tree_root.join("lib/libX11.so.6")
            )),
            None,
            "the tree's own copy is not its provenance"
        );
        assert_eq!(host_provenance(None, &roots, None, || None), None);

        let exe = std::env::current_exe().unwrap();
        if let Some(family) = parse_needed(&exe)
            .unwrap_or_default()
            .iter()
            .find_map(|d| libc_family_of_soname(d))
        {
            let other = match family {
                LibcFamily::Glibc => LibcFamily::Musl,
                LibcFamily::Musl => LibcFamily::Glibc,
            };
            assert_eq!(
                host_provenance(None, &roots, Some(other), || Some(exe.clone())),
                None,
                "a host object of another libc family is refused"
            );
            assert!(host_provenance(None, &roots, Some(family), || Some(exe.clone())).is_some());
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

/// One library a framework copier selected: the file name it lands
/// under, the resolved source, and its size.
pub(crate) struct LibCopy {
    pub(crate) name: String,
    pub(crate) resolved: PathBuf,
    pub(crate) size: u64,
}

/// Select libraries from `search_dirs` by `name_pred` (on the file
/// name). Resolves symlinks, filters by ELF class and machine, and dedups
/// by name only after every filter passes, so a wrong-arch or dangling
/// earlier copy never shadows a valid later one. Reads only.
pub(crate) fn select_libs(
    search_dirs: &[PathBuf],
    name_pred: impl Fn(&str) -> bool,
    target_class: Option<u8>,
    target_machine: Option<u16>,
    excludes: &[&str],
) -> Vec<LibCopy> {
    let mut selected = Vec::new();
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
            seen.insert(name.clone());
            let size = fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
            selected.push(LibCopy {
                name,
                resolved,
                size,
            });
        }
    }
    selected
}

/// Print the line a selected library gets.
pub(crate) fn report_lib(copy: &LibCopy) {
    eprintln!(
        "  {} <- {} ({})",
        color::bold_green(&copy.name),
        copy.resolved.display(),
        color::dim(&format_size(copy.size))
    );
}

/// Write one selected library into `dest`, removing an alias symlink at
/// the destination first and never copying a file onto itself.
pub(crate) fn write_lib(copy: &LibCopy, dest: &Path, strip: bool) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    let dest_path = dest.join(&copy.name);
    // Copying onto an existing alias symlink corrupts the alias target;
    // drop the symlink first (propagate failure so we never fall through
    // to fs::copy and clobber the target).
    if dest_path.is_symlink() {
        fs::remove_file(&dest_path)?;
    }
    // Never copy a file onto itself (an $ORIGIN search dir that resolved
    // to the bundle's own lib dir): fs::copy truncates it.
    if !is_same_file(&copy.resolved, &dest_path) {
        ensure_writable(&dest_path);
        fs::copy(&copy.resolved, &dest_path)?;
        let _ = fs::set_permissions(&dest_path, PermissionsExt::from_mode(0o755));
        if strip {
            strip_debug(&dest_path);
        }
        normalize_mtime(&dest_path);
    }
    Ok(())
}

/// Copy libraries from `search_dirs` into `dest`, selecting entries by
/// `name_pred` the way [`select_libs`] does. Returns
/// `(files_copied, total_bytes)`.
// Threads one bundling context; a parameter object belongs with the
// bundler restructure.
#[allow(clippy::too_many_arguments)]
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
    let selected = select_libs(
        search_dirs,
        name_pred,
        target_class,
        target_machine,
        excludes,
    );
    let mut total_bytes = 0u64;
    for copy in &selected {
        report_lib(copy);
        if !dry_run {
            write_lib(copy, dest, strip)?;
        }
        total_bytes += copy.size;
    }
    Ok((selected.len(), total_bytes))
}

/// Copy libraries whose file name starts with any of `prefixes`.
// Threads one bundling context; a parameter object belongs with the
// bundler restructure.
#[allow(clippy::too_many_arguments)]
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

/// The charset converters a bundled glibc brings along, by file name
/// prefix: the Unicode encodings, the Windows and ISO Latin code pages,
/// the common East Asian ones, and the `lib*` tables those share. glibc's
/// other two hundred are for charsets an application is very unlikely to
/// meet.
const GCONV_MODULES: &[&str] = &[
    "UTF", "UNICODE", "ANSI", "CP", "LATIN", "ISO8859", "SJIS", "EUC-JP", "EUC-KR", "EUC-CN", "lib",
];

/// Copy glibc's charset converters from beside `libc` into `gconv/`
/// under `lib_dest`, with the configuration that names them. Returns the
/// files copied, relative to `lib_dest`.
///
/// `iconv` loads these by `dlopen`, and they bind to `GLIBC_PRIVATE`, so
/// only the modules of the very glibc that was bundled can serve it: on a
/// host with a different glibc every conversion but the built-in UTF-8
/// ones fails without them. The runtime points `GCONV_PATH` here.
fn bundle_gconv(libc: &Path, lib_dest: &Path, strip: bool) -> io::Result<Vec<String>> {
    let Some(src) = libc.parent().map(|d| d.join("gconv")) else {
        return Ok(Vec::new());
    };
    if !src.is_dir() {
        return Ok(Vec::new());
    }
    let dest = lib_dest.join("gconv");
    let mut copied = Vec::new();
    let mut copy = |from: &Path, rel: &str| -> io::Result<()> {
        let to = dest.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        ensure_writable(&to);
        fs::copy(from, &to)?;
        normalize_mtime(&to);
        copied.push(format!("gconv/{rel}"));
        Ok(())
    };
    let modules_conf = src.join("gconv-modules");
    if modules_conf.is_file() {
        copy(&modules_conf, "gconv-modules")?;
    }
    if let Ok(entries) = fs::read_dir(src.join("gconv-modules.d")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.path().is_file() && name.ends_with(".conf") {
                copy(&entry.path(), &format!("gconv-modules.d/{name}"))?;
            }
        }
    }
    for entry in fs::read_dir(&src)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".so") || !GCONV_MODULES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        copy(&entry.path(), &name)?;
        if strip {
            strip_debug(&dest.join(&*name));
        }
    }
    copied.sort();
    Ok(copied)
}

/// Copy glibc's `C.UTF-8` locale from where `libc`'s distribution keeps
/// it into `locale/` under `lib_dest`. Returns the files copied, relative
/// to `lib_dest`.
///
/// A bundled glibc reads locales from the host's locale archive, and a
/// host without one (a musl host, a minimal container) leaves it unable
/// to set any UTF-8 locale. `C.UTF-8` is not built into glibc, so the
/// package carries it, and the runtime points `LOCPATH` here on such a
/// host. Arch keeps it beside libc, Debian one level up from its
/// multiarch directory, Fedora in `lib/` beside `lib64/`.
fn bundle_c_utf8(libc: &Path, lib_dest: &Path) -> io::Result<Vec<String>> {
    let Some(libc_dir) = libc.parent() else {
        return Ok(Vec::new());
    };
    let found = ["locale", "../locale", "../lib/locale"]
        .iter()
        .flat_map(|rel| ["C.utf8", "C.UTF-8"].map(|name| (libc_dir.join(rel).join(name), name)))
        .find(|(dir, _)| dir.join("LC_CTYPE").is_file());
    let Some((src, name)) = found else {
        return Ok(Vec::new());
    };
    let mut copied = Vec::new();
    let mut pending = vec![(src, PathBuf::from("locale").join(name))];
    while let Some((from, rel)) = pending.pop() {
        for entry in fs::read_dir(&from)?.flatten() {
            let rel = rel.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push((entry.path(), rel));
            } else if kind.is_file() {
                let to = lib_dest.join(&rel);
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                ensure_writable(&to);
                fs::copy(entry.path(), &to)?;
                normalize_mtime(&to);
                copied.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    copied.sort();
    Ok(copied)
}

/// The glibc libraries that objects built against an older glibc link by
/// name, carried beside a bundled `libc.so.6`.
///
/// Since glibc 2.34 all of them but `libresolv` are stubs whose code moved
/// into libc, so nothing in a bundle built against a current glibc asks
/// for them. An object from outside the bundle still can: a host plugin
/// that a user's configuration names, built against the host's older
/// glibc, fails to load without them.
const GLIBC_COMPAT_LIBS: &[&str] = &[
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
    "libutil.so.1",
    "libanl.so.1",
    "libresolv.so.2",
];

const DEFAULT_EXCLUDES: &[&str] = &[
    "libnss_",
    "libcuda.so",
    "libnvidia",
    "libamdhip64.so",
    "libze_loader.so",
    "linux-vdso.so",
];

pub(crate) use onelf_sysroot::LIB_PATHS as STANDARD_LIB_PATHS;

/// A symlink target reaching `to` (relative to the library directory)
/// from inside `from_dir` (also relative to it), where the link itself
/// sits: the bare name when both are in the same directory, `../` climbs
/// otherwise.
fn link_target(from_dir: &Path, to: &Path) -> String {
    if to.parent().is_some_and(|p| p == from_dir) {
        return to
            .file_name()
            .unwrap_or(to.as_os_str())
            .to_string_lossy()
            .into_owned();
    }
    let mut target = String::new();
    for _ in 0..from_dir.components().count() {
        target.push_str("../");
    }
    target.push_str(&to.to_string_lossy());
    target
}

/// Copy a data directory into the tree, pinning each file's mtime like
/// every other copy. Regular files and relative symlinks go; absolute
/// symlinks stay behind with a warning, since their target has no place
/// in the tree. Every path written is pushed to `written`. Returns the
/// bytes copied.
fn copy_data_dir(src: &Path, dst: &Path, written: &mut Vec<PathBuf>) -> io::Result<u64> {
    let mut total = 0u64;
    fs::create_dir_all(dst)?;
    let mut entries: Vec<std::fs::DirEntry> = fs::read_dir(src)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        if kind.is_dir() {
            total += copy_data_dir(&entry.path(), &dst.join(entry.file_name()), written)?;
        } else if kind.is_file() {
            let to = dst.join(entry.file_name());
            ensure_writable(&to);
            fs::copy(entry.path(), &to)?;
            written.push(to.clone());
            normalize_mtime(&to);
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        } else if kind.is_symlink() {
            let target = fs::read_link(entry.path())?;
            if target.is_relative() {
                let to = dst.join(entry.file_name());
                if to.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(&to);
                }
                std::os::unix::fs::symlink(target, &to)?;
                written.push(to);
            } else {
                eprintln!(
                    "  {} absolute symlink in {} left out: {} -> {}",
                    color::bold_red("warning:"),
                    src.display(),
                    entry.path().display(),
                    target.display()
                );
            }
        }
    }
    Ok(total)
}

/// Copy one carried path, a directory or a single file, returning the
/// bytes copied. A single file is written from the bytes the plan
/// verified against its stamp.
fn copy_data(
    src: &Path,
    dst: &Path,
    plan: &DestinationPlan,
    written: &mut Vec<PathBuf>,
) -> io::Result<u64> {
    if src.is_dir() {
        return copy_data_dir(src, dst, written);
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = plan.read_verified(src)?;
    ensure_writable(dst);
    fs::write(dst, &bytes)?;
    // The copy keeps the source's permission bits, so a helper script
    // beside a plugin stays executable and a read-only one stays read-
    // only.
    let mode = fs::metadata(src)?.permissions().mode();
    fs::set_permissions(dst, fs::Permissions::from_mode(mode & 0o7777))?;
    written.push(dst.to_path_buf());
    normalize_mtime(dst);
    Ok(bytes.len() as u64)
}

/// Copy the admitted members of a sysroot directory carry, each from
/// the bytes the plan verified, a relative link as a link, and nothing
/// the build's universe left out. Returns the bytes copied.
fn copy_members(
    members: &[(PathBuf, PathBuf)],
    directory: &Path,
    plan: &DestinationPlan,
    written: &mut Vec<PathBuf>,
) -> io::Result<u64> {
    let mut total = 0u64;
    for (from, rel) in members {
        let to = directory.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let meta = fs::symlink_metadata(from)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(from)?;
            if target.is_relative() {
                if to.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(&to);
                }
                std::os::unix::fs::symlink(target, &to)?;
                written.push(to);
            }
            continue;
        }
        total += copy_data(from, &to, plan, written)?;
    }
    Ok(total)
}

/// A carried `share/alsa` must not let the host's distro snippets name
/// the output. `alsa.conf`'s top-level `@hooks` load the host's
/// `/etc/alsa/conf.d` by absolute path after everything the carried
/// config named, so a host that ships a pipewire default wins over the
/// pulse default the package carried, and the plugin it names usually
/// cannot load. The carried snippet directory is inserted into the hook
/// list right after the host's, so the carried defaults load last among
/// them. `/etc/asound.conf` and `~/.asoundrc` keep their places after
/// that: an explicit user configuration still wins. Returns whether the
/// file was rewritten.
fn patch_alsa_conf_hooks(conf: &Path) -> io::Result<bool> {
    const CARRIED: &str = "{ @func datadir } \"/alsa.conf.d\"";
    let text = fs::read_to_string(conf)?;
    if text.contains(CARRIED) {
        return Ok(false);
    }
    let lines: Vec<&str> = text.lines().collect();
    let Some((start, end)) = hooks_block(&lines) else {
        return Ok(false);
    };
    let mut insert_at = None;
    for (at, line) in lines.iter().enumerate().skip(start).take(end - start) {
        let line = line.trim();
        if line.contains("conf.d") && !line.contains("@func") {
            insert_at = Some(at + 1);
        }
    }
    let Some(insert_at) = insert_at else {
        eprintln!(
            "  {} {} has a hooks block but no conf.d entry; the carried snippets are never loaded by it",
            color::bold_red("warning:"),
            conf.display()
        );
        return Ok(false);
    };
    let mut out = String::with_capacity(text.len() + CARRIED.len() + 64);
    for (at, line) in lines.iter().enumerate() {
        if at == insert_at {
            out.push_str("\t\t\t{ @func concat strings [ ");
            out.push_str(CARRIED);
            out.push_str(" ] }\n");
        }
        out.push_str(line);
        out.push('\n');
    }
    ensure_writable(conf);
    fs::write(conf, out)?;
    normalize_mtime(conf);
    Ok(true)
}

/// The half-open line range of the top-level `@hooks` block, `[` on the
/// first line through the line closing it. `cards.@hooks` and deeper
/// blocks do not match: the leading name must be exactly the hook.
fn hooks_block(lines: &[&str]) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut start = None;
    for (at, line) in lines.iter().enumerate() {
        if depth == 0 {
            let trimmed = line.trim_start();
            if !(trimmed.starts_with("@hooks") && trimmed.contains('[')) {
                continue;
            }
            start = Some(at);
            depth = line
                .matches('[')
                .count()
                .saturating_sub(line.matches(']').count());
        } else {
            depth += line.matches('[').count();
            depth = depth.saturating_sub(line.matches(']').count());
        }
        if start.is_some() && depth == 0 {
            return Some((start?, at + 1));
        }
    }
    None
}

/// Carry one activated rule's declared data group into the tree. The
/// group's ordered alternatives are tried in declaration order against
/// the roots of the libraries that triggered the rule, an installation
/// prefix or the directory the resolved library sits in, and the first
/// existing alternative is the one taken; a later alternative is
/// examined only where the earlier ones do not exist. The declared
/// destination is relative to the tree root, so the carried layout is
/// where the runtime's mappings look. The roots are the triggering
/// libraries' own, so data another installation happens to keep in a
/// directory the walk admitted never answers for a framework whose own
/// source lacks it.
///
/// A destination the tree already holds satisfies the group: a carried
/// directory is the tree's own, its configuration anchored in place,
/// and a carried file is judged by content rather than rewritten. When
/// no alternative exists on any admitted root, a tree-held destination
/// still satisfies the group, a required group otherwise fails the
/// assembly naming the rule and the examined locations, and an optional
/// one is reported and the build continues. Nothing is admitted to the
/// plan here that the plan has not judged, and what no admitted root
/// holds is never taken from elsewhere: in a sysroot build the
/// alternatives are the admitted files of the selected closure alone.
/// What a declared set holds instead is not carried raw: it is prepared
/// into each supplying pin's own subtree, where the runtime mapping
/// bound to that pin resolves it, and only when that pin is selected.
#[allow(clippy::too_many_arguments)]
fn carry_data_group(
    directory: &Path,
    descriptor: &Descriptor,
    declared: &ResourceGroup,
    admitted: &[AdmittedLibrary],
    sysroot: Option<&sysroot::SysrootUniverse>,
    plan: &mut DestinationPlan,
    actions: &mut Vec<WalkAction>,
    preparation: &mut SetPreparation,
    required: &mut HashSet<PathBuf>,
) -> io::Result<()> {
    let rule = descriptor.id.as_str();
    if declared.requirement == Requirement::Required {
        required.insert(PathBuf::from(declared.destination.as_str()));
    }
    let hooks: Vec<BuildHookId> = descriptor.build.hooks.clone();
    let origin = format!("rule {rule} group {} data carry", declared.id.as_str());
    let owners: Vec<AdmittedLibrary> = admitted
        .iter()
        .filter(|lib| {
            descriptor
                .triggers
                .iter()
                .any(|t| t.matches_soname(lib.soname()))
        })
        .cloned()
        .collect();
    let outcome = discover_group(
        &GroupQuery {
            rule,
            group: declared.id.as_str(),
            kind: declared.kind,
            destination: &declared.destination,
            candidates: &declared.candidates,
        },
        &AdmittedUniverse {
            libraries: &owners,
            sysroot,
        },
    )?;
    if let Some(hit) = outcome.found.first() {
        return carry_hit(directory, rule, hit, hooks, sysroot, plan, actions, &origin).map(|_| ());
    }
    if directory.join(declared.destination.as_str()).exists() {
        if !hooks.is_empty() {
            actions.push(WalkAction::RunStagedHooks { hooks });
        }
        return Ok(());
    }
    let supplied = preparation.supply_group(
        plan,
        &SupplyQuery {
            rule,
            group: declared.id.as_str(),
            kind: declared.kind,
            destination: declared.destination.as_str(),
            candidates: &declared.candidates,
            executable: false,
            hooks: &hooks,
            runtime: &descriptor.runtime.paths,
        },
    )?;
    if supplied {
        return Ok(());
    }
    let miss = data_miss(rule, declared, &outcome);
    completeness::record(miss.clone());
    match declared.requirement {
        Requirement::Required if !preparation.dry_run => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("rule {rule} {}", miss.reason),
        )),
        _ => Ok(()),
    }
}

/// Carry the data an alternative was found at into the tree at its
/// destination, then run `hooks` over it. A destination the tree already
/// holds is the tree's own and is kept. A sysroot directory carries only
/// what the build admitted under it, so a file the policy or a trace
/// pruned never comes back through a carry whose destination differs.
#[allow(clippy::too_many_arguments)]
fn carry_hit(
    directory: &Path,
    rule: &str,
    hit: &CandidateHit,
    hooks: Vec<BuildHookId>,
    sysroot: Option<&sysroot::SysrootUniverse>,
    plan: &mut DestinationPlan,
    actions: &mut Vec<WalkAction>,
    origin: &str,
) -> io::Result<Carry> {
    let dest = directory.join(hit.destination.as_path());
    if dest.exists() {
        if !hooks.is_empty() {
            actions.push(WalkAction::RunStagedHooks { hooks });
        }
        if hit.kind == ResourceKind::File {
            plan.admit_carried(hit.destination.as_path(), &hit.resolved, origin)?;
        }
        return Ok(Carry::Held);
    }
    if let (Some(universe), ResourceKind::Directory) = (sysroot, hit.kind) {
        let members = admitted_members(universe, hit, plan, origin)?;
        if members.is_empty() {
            return Ok(Carry::Empty);
        }
        actions.push(WalkAction::CarryData {
            rule: rule.to_string(),
            src: hit.resolved.clone(),
            rel: hit.destination.clone(),
            hooks,
            members: Some(members),
        });
        return Ok(Carry::Planned);
    }
    match plan.admit_carried(hit.destination.as_path(), &hit.resolved, origin)? {
        Admission::Write => {
            actions.push(WalkAction::CarryData {
                rule: rule.to_string(),
                src: hit.resolved.clone(),
                rel: hit.destination.clone(),
                hooks,
                members: None,
            });
            Ok(Carry::Planned)
        }
        Admission::Skip => Ok(Carry::Held),
        Admission::Rejected => Ok(Carry::Rejected),
    }
}

/// What carrying one found location planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carry {
    /// A copy into the tree.
    Planned,
    /// Nothing, since the tree already holds the destination.
    Held,
    /// Nothing, since the destination clashes with another write, which
    /// the plan records as a finding.
    Rejected,
    /// Nothing, since the build admitted nothing below the location.
    Empty,
}

/// The walk's queues and plan, which collecting a plugin group feeds.
struct WalkState<'a> {
    already_processed: &'a mut HashSet<String>,
    search_paths: &'a mut Vec<PathBuf>,
    repair_extra: &'a mut HashSet<PathBuf>,
    group_dests: &'a mut HashMap<String, PathBuf>,
    group_sources: &'a mut HashMap<String, PathBuf>,
    needed_by: &'a mut HashMap<String, String>,
    discovery: &'a mut Discovery,
    plan: &'a mut DestinationPlan,
    actions: &'a mut Vec<WalkAction>,
    scan: &'a mut Vec<(String, PathBuf)>,
}

/// Leave the recipe's excluded members out of a collection, recording
/// each with its reason.
fn exclude_recipe_members(collection: &mut collect::GroupCollection, recipe_exclude: &[String]) {
    let (plugins, excluded): (Vec<_>, Vec<_>) = std::mem::take(&mut collection.plugins)
        .into_iter()
        .partition(|p| !recipe_excludes(&p.soname, recipe_exclude));
    collection.plugins = plugins;
    collection.skipped.extend(excluded.into_iter().map(|p| {
        (
            p.destination.display().to_string(),
            "left out by [bundle] exclude".to_string(),
        )
    }));
}

/// Plan a collected plugin group's members into the tree: each member
/// that sits at its own place in the layout, a second one under a name
/// another group collected first, is admitted at its destination,
/// never re-resolved by name, with its copy planned. The members a
/// group collects first are returned as name and destination pairs, for
/// [`queue_collection`] to enqueue; nothing in the walk's queues is
/// touched, so a caller that undoes the plan here leaves the queues
/// untouched too.
fn plan_collection(
    collection: &mut collect::GroupCollection,
    origin: &str,
    directory: &Path,
    queue: &mut WalkState,
) -> io::Result<Vec<(String, PathBuf)>> {
    let mut firsts: Vec<(String, PathBuf)> = Vec::new();
    for plugin in &collection.plugins {
        // A name this collection claimed stays claimed, in a local list
        // until the commit, so a second member under one name is
        // planned at its own place here too.
        let first = match firsts.iter().find(|(soname, _)| soname == &plugin.soname) {
            Some((_, dest)) => *dest == plugin.destination,
            None => match queue.group_dests.get(&plugin.soname) {
                Some(dest) => *dest == plugin.destination,
                None => {
                    firsts.push((plugin.soname.clone(), plugin.destination.clone()));
                    true
                }
            },
        };
        if !first {
            let origin = format!("{origin} plugin {}", plugin.destination.display());
            plan_member_copy(directory, plugin, &origin, queue.plan, queue.actions)?;
        }
    }
    Ok(firsts)
}

/// Hand a planned plugin group to the walk's queues: the found
/// directories become search paths a repair may answer from, each
/// member enters the object queue at its destination, and a second
/// member under one name at its own place in the layout has its own
/// needs entering the walk. The members [`plan_collection`] collected
/// first are inserted into the group destinations here, after the plan
/// has admitted the rest, so an undone plan never claims a soname. A
/// second member the copy carries without owning its name registers
/// its own file in the walk state's scan list for the inference
/// rounds, since its compiled-in directories name resources no member
/// of the walk answers for; a sysroot build needs none of that,
/// because its populated tree is scanned already.
fn queue_collection(
    collection: &collect::GroupCollection,
    origin: &str,
    excludes: &[&str],
    reachable: &HashSet<String>,
    firsts: &[(String, PathBuf)],
    queue: &mut WalkState,
    sysroot: Option<&sysroot::SysrootUniverse>,
) -> io::Result<()> {
    for dir in &collection.search_dirs {
        if !queue.search_paths.contains(dir) {
            queue.search_paths.push(dir.clone());
        }
        queue.repair_extra.insert(dir.clone());
    }
    for plugin in &collection.plugins {
        if let Some(dir) = plugin.resolved.parent() {
            let dir = dir.to_path_buf();
            if !queue.search_paths.contains(&dir) {
                queue.search_paths.push(dir.clone());
            }
            queue.repair_extra.insert(dir);
        }
    }
    for (soname, destination) in firsts {
        queue
            .group_dests
            .entry(soname.clone())
            .or_insert_with(|| destination.clone());
    }
    for plugin in &collection.plugins {
        let first = queue
            .group_dests
            .get(&plugin.soname)
            .is_some_and(|dest| *dest == plugin.destination);
        if !first {
            let origin = format!("{origin} plugin {}", plugin.destination.display());
            if sysroot.is_none()
                && let Some(name) = plugin.resolved.file_name().and_then(|n| n.to_str())
            {
                queue.scan.push((name.to_string(), plugin.resolved.clone()));
            }
            for dep in parse_needed(&plugin.resolved).unwrap_or_default() {
                if queue.already_processed.contains(&dep)
                    || is_excluded(&dep, excludes)
                    || reachable.contains(&dep)
                    || libc_family_of_soname(&dep).is_some()
                {
                    continue;
                }
                queue
                    .needed_by
                    .entry(dep.clone())
                    .or_insert_with(|| origin.clone());
                queue.discovery.enqueue(&dep)?;
            }
            continue;
        }
        queue
            .group_sources
            .insert(plugin.soname.clone(), plugin.resolved.clone());
        queue
            .needed_by
            .entry(plugin.soname.clone())
            .or_insert_with(|| origin.to_string());
        // An object that drained before its group popped chose
        // its destination flat; the group's declared destination
        // supersedes it, so the object runs again with the
        // repair that move implies.
        queue.already_processed.remove(&plugin.soname);
        queue.discovery.enqueue(&plugin.soname)?;
    }
    Ok(())
}

/// What carrying one inferred directory planned, before the walk's
/// queues are touched and the carry is committed.
enum InferredPlan {
    /// The whole directory, as a data carry.
    Data(Carry),
    /// The trace-limited directory, as a data carry of the members the
    /// plan admitted: the planned pairs and bytes, and the member
    /// counts the trim judged.
    TrimmedData {
        files: Vec<(PathBuf, PathBuf)>,
        size: u64,
        kept: usize,
        total: usize,
    },
    /// The collection an inferred plugin directory's members went
    /// through, boxed to keep the variant small, the members a group
    /// collected first, the members that are not objects to carry as
    /// data, the planned byte size, and the member counts the trim
    /// judged, when the trace limited the directory.
    Plugins {
        collection: Box<collect::GroupCollection>,
        firsts: Vec<(String, PathBuf)>,
        files: Vec<(PathBuf, PathBuf)>,
        size: u64,
        trimmed: Option<(usize, usize)>,
    },
}

/// The `[bundle] trim-inferred` policy: what a trace observed, and the
/// keep globs that survive it regardless. A sysroot build reads the
/// recipe's `[sysroot] trace` and `[sysroot] keep`; a host scan takes
/// what its traced run opened, and has no keep patterns. A host scan
/// also carries the tree root, so an open of a copy the tree itself
/// holds can stand in for a member no spelling names.
struct Trim {
    opened: BTreeSet<String>,
    keep: Option<Policy>,
    tree: Option<String>,
}

impl Trim {
    /// Load the sysroot trace tiers: the paths a test run opened, one
    /// per line, and the globs the trim may not cut.
    fn load(trace: Option<&Path>, keep: Option<&Path>) -> io::Result<Self> {
        let opened = trace
            .map(Trace::load)
            .transpose()?
            .map(|t| t.opened_paths().clone())
            .unwrap_or_default();
        let keep = keep.map(Policy::load).transpose()?;
        Ok(Trim {
            opened,
            keep,
            tree: None,
        })
    }

    /// From the absolute paths a traced host run opened, and the tree
    /// root the run's copies live under.
    fn from_paths(paths: Vec<PathBuf>, tree: Option<&Path>) -> Self {
        let slash_trimmed = |p: &Path| {
            let rel = p.to_string_lossy();
            let rel = rel.trim_start_matches('/');
            (!rel.is_empty()).then(|| rel.to_string())
        };
        Trim {
            opened: paths.iter().filter_map(|p| slash_trimmed(p)).collect(),
            keep: None,
            tree: tree.and_then(slash_trimmed),
        }
    }

    /// Whether the trace observed `path`, or a keep glob names it.
    fn keeps(&self, path: &Path) -> bool {
        let rel = path.to_string_lossy();
        let rel = rel.trim_start_matches('/');
        if self.opened.contains(rel) {
            return true;
        }
        self.keep.as_ref().is_some_and(|keep| keep.matches(rel))
    }

    /// Whether the run opened a same-named copy of the member inside
    /// the tree: the tree's own library directories lead the run's
    /// search path, so on a rebuild a copy the last build carried
    /// stands in for the original, and that open names a path no
    /// member spelling spells. Matching by name alone can keep an
    /// unrelated member of the same name, which trims on the
    /// conservative side.
    fn keeps_tree_shadow(&self, rel: &Path) -> bool {
        let Some(tree) = &self.tree else {
            return false;
        };
        let Some(name) = rel.file_name() else {
            return false;
        };
        self.opened.iter().any(|opened| {
            Path::new(opened).strip_prefix(Path::new(tree)).is_ok()
                && Path::new(opened).file_name() == Some(name)
        })
    }
}

/// Every path spelling the trace could have named `dir`'s member `rel`
/// (relative to the directory) by: the canonical prefix pair, the
/// resolved source, every compiled-in path that names the directory,
/// since an app opens the spelling it compiled in, and, for a sysroot
/// build, the member's root-relative spellings, since a sysroot trace
/// records paths below the root while the canonical pair spells host
/// paths under it. A host scan needs none of these for the tree's own
/// copies: `Trim::keeps_tree_shadow` answers for every open of a
/// same-named copy below the tree root.
fn member_spellings(
    dir: &infer::InferredDir,
    rel: &Path,
    sysroot: Option<&sysroot::SysrootUniverse>,
) -> Vec<PathBuf> {
    let mut out = vec![dir.prefix.join(&dir.rel).join(rel), dir.source.join(rel)];
    out.extend(dir.compiled.iter().map(|s| Path::new(s).join(rel)));
    if let Some(universe) = sysroot
        && let Ok(dir_rel) = dir.source.strip_prefix(universe.root())
    {
        out.push(dir_rel.join(rel));
        if let Ok(resolved) = onelf_sysroot::path::in_root(universe.root(), dir_rel)
            && let Ok(resolved_rel) = resolved.strip_prefix(universe.root())
        {
            out.push(resolved_rel.join(rel));
        }
    }
    out
}

/// Whether the trim leaves `dir`'s member `rel` out: no spelling of it
/// was observed, no keep glob names it, and the run did not open a
/// same-named copy of it inside the tree. Without a trim every member
/// survives, so a recipe without the key reads exactly as before.
fn trimmed_out(
    dir: &infer::InferredDir,
    rel: &Path,
    sysroot: Option<&sysroot::SysrootUniverse>,
    trim: Option<&Trim>,
) -> bool {
    let Some(trim) = trim else {
        return false;
    };
    let kept = member_spellings(dir, rel, sysroot)
        .iter()
        .any(|spelling| trim.keeps(spelling));
    !(kept || trim.keeps_tree_shadow(rel))
}

/// Plan one inferred plugin directory's carry into the tree, with the
/// walk's queues untouched: the recipe's exclusions leave members out,
/// each member the collection took that sits at its own place in the
/// layout is admitted at its destination, the way a second member under
/// a collected name is, and every member that is not an ELF object is
/// admitted as data. A member is judged by the entry the sysroot's tree
/// holds for it, so a link there stays a link and is never followed
/// into a copy of what it names; an absolute one has no place in the
/// tree and is left out, as a host carry leaves it, and so is any entry
/// that is not a regular file, such as a directory the package lists.
/// On `Ok` the collection, the members a group collected first, the
/// data members to carry and the planned byte size; the caller hands
/// them to the walk's queues only after the plan has held.
#[allow(clippy::too_many_arguments)]
fn plan_plugin_carry(
    dir: &infer::InferredDir,
    members: &[(PathBuf, PathBuf)],
    mut collection: collect::GroupCollection,
    trimmed_total: Option<(usize, usize)>,
    origin: &str,
    opts: &BundleOptions,
    sysroot: Option<&sysroot::SysrootUniverse>,
    state: &mut WalkState,
) -> io::Result<InferredPlan> {
    exclude_recipe_members(&mut collection, &opts.exclude);
    let firsts = plan_collection(&mut collection, origin, &opts.directory, state)?;
    let mut size: u64 = collection
        .plugins
        .iter()
        .filter_map(|p| fs::metadata(&p.resolved).ok())
        .map(|meta| meta.len())
        .sum();
    let (files, files_size) = plan_member_data(dir, members, origin, sysroot, state)?;
    size += files_size;
    Ok(InferredPlan::Plugins {
        collection: Box::new(collection),
        firsts,
        files,
        size,
        trimmed: trimmed_total,
    })
}

/// Plan an inferred directory's non-object members as data, at their
/// places beneath its destination: a regular file admitted as a carry,
/// a relative link as a link, an absolute one left out, as a host carry
/// leaves it, and so is any entry that is not a regular file, such as a
/// directory the package lists. A member is judged by the entry the
/// sysroot's tree holds for it, so a link there stays a link and is
/// never followed into a copy of what it names; a host scan's members
/// are already the files they resolved to. On `Ok` the
/// (source, destination) pairs and the planned bytes.
fn plan_member_data(
    dir: &infer::InferredDir,
    members: &[(PathBuf, PathBuf)],
    origin: &str,
    sysroot: Option<&sysroot::SysrootUniverse>,
    state: &mut WalkState,
) -> io::Result<(Vec<(PathBuf, PathBuf)>, u64)> {
    let hit = dir.hit();
    // The member as the sysroot's tree holds it: every component but
    // the last resolved, so a link at the entry stays the link.
    let member_of = |rel: &Path| -> Option<PathBuf> {
        let universe = sysroot?;
        let dir_rel = hit.resolved.strip_prefix(universe.root()).ok()?;
        onelf_sysroot::path::entry_in_root(universe.root(), &dir_rel.join(rel)).ok()
    };
    let mut size = 0u64;
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (rel, file) in members {
        if read_elf_class(file).is_some() {
            continue;
        }
        let to = dir.destination.join(rel);
        let member = member_of(rel).unwrap_or_else(|| file.clone());
        let meta = fs::symlink_metadata(&member)?;
        let admission = if meta.file_type().is_symlink() {
            let target = fs::read_link(&member)?;
            if !target.is_relative() {
                continue;
            }
            state.plan.admit_strict_link(
                &to,
                &target.to_string_lossy(),
                origin,
                Existing::SkipOnAny,
            )?
        } else if meta.is_file() {
            state.plan.admit_carried(&to, file, origin)?
        } else {
            continue;
        };
        if admission == Admission::Write {
            if !meta.file_type().is_symlink() {
                size += fs::metadata(file).map(|meta| meta.len()).unwrap_or(0);
            }
            files.push((member, to));
        }
    }
    Ok((files, size))
}

/// The ledger spelling a member's populate-written tree copy is
/// recorded under: a sysroot build disposes by the populate's own
/// spelling of the member's sysroot path; under a prefix below the
/// sysroot's `usr` the carry destination can spell another directory's
/// attested key, so it is never consulted for one. A host scan knows
/// no sysroot and names the carry destination.
fn disposal_spelling(
    sysroot: Option<&sysroot::SysrootUniverse>,
    dir: &infer::InferredDir,
    rel: &Path,
) -> String {
    if let Some(universe) = sysroot
        && let Ok(dir_rel) = dir.source.strip_prefix(universe.root())
    {
        return sysroot::appdir_path(&dir_rel.join(rel).to_string_lossy()).to_string();
    }
    dir.destination.join(rel).to_string_lossy().into_owned()
}

/// The tree copies the sysroot populate wrote of the members the trim
/// leaves out are disposed the way the populate disposes its own stale
/// copies: an entry whose recorded hash still matches the bytes, or one
/// pending the build's own rewriting, is removed with its empty
/// directories, and the ledger drops it. A copy the ledger cannot
/// attest may hold a publisher's edit, and stays; the next build's
/// publication stops on it, where the author can resolve it. Each
/// member arrives with the ledger spelling its copy is recorded
/// under: a sysroot build disposes by the populate's own spelling of
/// the member's sysroot path, a host scan by the carry destination.
/// The member as its directory holds it: every component but the last
/// resolved, so a link at the entry stays the link. A host scan has no
/// sysroot tree and names the entry by the directory itself.
fn member_entry(
    sysroot: Option<&sysroot::SysrootUniverse>,
    resolved_dir: &Path,
    rel: &Path,
) -> Option<PathBuf> {
    match sysroot {
        Some(universe) => {
            let dir_rel = resolved_dir.strip_prefix(universe.root()).ok()?;
            onelf_sysroot::path::entry_in_root(universe.root(), &dir_rel.join(rel)).ok()
        }
        None => Some(resolved_dir.join(rel)),
    }
}

/// The lexical spelling of `path` with `.` dropped and `..` popped
/// from what it can. The same normalization on both spellings keeps a
/// prefix strip aligned across dotted anchors.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            component => out.push(component.as_os_str()),
        }
    }
    out
}

/// The trimmed member a kept relative link names, when nothing else
/// answers for the link: a collection object at the link's destination
/// carries the name itself, so the link rides with it; without one,
/// the plan would carry the link dangling, or drop it as an ELF and
/// carry nothing at all.
fn dangling_target(
    sysroot: Option<&sysroot::SysrootUniverse>,
    resolved_dir: &Path,
    rel: &Path,
    answered: bool,
) -> Option<PathBuf> {
    let entry = member_entry(sysroot, resolved_dir, rel)?;
    if !fs::symlink_metadata(&entry).ok()?.is_symlink() {
        return None;
    }
    if answered {
        return None;
    }
    let target = fs::read_link(&entry).ok()?;
    if !target.is_relative() {
        return None;
    }
    let named = entry.parent()?.join(target);
    // Lexical resolution only, mirroring the sysroot resolver's
    // component handling: a target through a subdirectory must spell
    // the member rel the enumeration judged. Both spellings normalize
    // the same way, so the prefix strip stays aligned, and a climb
    // past the directory names no member. The sysroot anchor is tried
    // as the directory spells itself and fully resolved, since a
    // compatibility link like `bin -> usr/bin` makes the entry's
    // parent spell the resolved form while the directory keeps the
    // compiled one.
    let named = normalize_lexical(&named);
    let mut anchors = vec![normalize_lexical(resolved_dir)];
    if let Some(universe) = sysroot
        && let Ok(dir_rel) = resolved_dir.strip_prefix(universe.root())
        && let Ok(resolved) = onelf_sysroot::path::in_root(universe.root(), dir_rel)
    {
        let resolved = normalize_lexical(&resolved);
        if !anchors.contains(&resolved) {
            anchors.push(resolved);
        }
    }
    anchors
        .iter()
        .find_map(|anchor| named.strip_prefix(anchor).ok().map(Path::to_path_buf))
}

/// A kept relative link needs the member it names: the trace used the
/// link, and the plan would otherwise carry it dangling once the
/// target member's tree copy is disposed. A link name a collection
/// object answers for rides with that object. Rescued links rescue
/// their own targets, until the trim's judgment is closed under
/// naming. Each pass rescues at least one member, so the passes are
/// bounded by the trimmed set, and a member whose target was not
/// trimmed when a pass judged it never rescues later: the trimmed set
/// only shrinks. A kept link whose target escapes the directory rides
/// unrescued; if the trim omits the target's home directory, the tree
/// keeps the link dangling and no finding names it.
fn keep_link_targets(
    sysroot: Option<&sysroot::SysrootUniverse>,
    dir: &infer::InferredDir,
    objects: Option<&[collect::CollectedPlugin]>,
    kept: &mut Vec<(PathBuf, PathBuf)>,
    trimmed: &mut Vec<(PathBuf, PathBuf)>,
) {
    let resolved_dir = &dir.hit().resolved;
    loop {
        let mut rescued: Option<usize> = None;
        for (rel, _) in kept.iter() {
            let answered = objects.is_some_and(|plugins| {
                plugins
                    .iter()
                    .any(|p| p.destination == dir.destination.join(rel))
            });
            let Some(target_rel) = dangling_target(sysroot, resolved_dir, rel, answered) else {
                continue;
            };
            if let Some(at) = trimmed.iter().position(|(r, _)| r == &target_rel) {
                rescued = Some(at);
                break;
            }
        }
        let Some(at) = rescued else {
            break;
        };
        kept.push(trimmed.swap_remove(at));
    }
}

#[cfg(test)]
mod trim_link_tests {
    use super::*;

    /// A kept relative link whose target climbs out of its own
    /// subdirectory names the member it lands on, a climb past the
    /// directory names no member at all, and a link a collection
    /// object answers for is never rescued. The dotted-anchor
    /// spellings a recipe's relative sysroot path produces are pinned
    /// end to end by the pipeline fixture and lexically below.
    #[test]
    fn a_link_target_through_dotdot_names_the_member_it_lands_on() {
        let dir =
            std::env::temp_dir().join(format!("onelf-dangling-target-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.conf"), b"conf\n").unwrap();
        fs::write(dir.join("outside.conf"), b"out\n").unwrap();
        std::os::unix::fs::symlink("../a.conf", dir.join("sub/link.conf")).unwrap();
        std::os::unix::fs::symlink("../../outside.conf", dir.join("sub/escape.conf")).unwrap();

        let named = dangling_target(None, &dir, Path::new("sub/link.conf"), false);
        assert_eq!(named.as_deref(), Some(Path::new("a.conf")));
        let escaped = dangling_target(None, &dir, Path::new("sub/escape.conf"), false);
        assert_eq!(escaped, None);
        let answered = dangling_target(None, &dir, Path::new("sub/link.conf"), true);
        assert_eq!(answered, None);

        let _ = fs::remove_dir_all(&dir);
    }

    /// A rootfs compatibility link (`bin -> usr/bin`, `lib ->
    /// usr/lib`) makes the entry's parent spell the resolved form
    /// while the directory keeps the compiled one; the anchor is
    /// tried fully resolved, so the target still names the member
    /// rel.
    #[test]
    fn a_target_beside_a_compatibility_link_anchor_still_names_the_member() {
        use sysroot::fixture::{package, populate_universe};

        let root = std::env::temp_dir().join(format!("onelf-compat-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sysroot = root.join("sysroot");
        fs::create_dir_all(sysroot.join("usr/lib/examplefw/sub")).unwrap();
        fs::write(sysroot.join("usr/lib/examplefw/a.conf"), b"conf\n").unwrap();
        std::os::unix::fs::symlink("usr/lib", sysroot.join("lib")).unwrap();
        std::os::unix::fs::symlink("../a.conf", sysroot.join("usr/lib/examplefw/sub/link.conf"))
            .unwrap();
        let packages = [package(
            "examplefw",
            &[
                "usr/lib/libexamplefw.so.1",
                "usr/lib/examplefw/a.conf",
                "usr/lib/examplefw/sub/link.conf",
            ],
        )];
        let universe = populate_universe(&sysroot, &packages, "examplefw", &[], None, None);
        // The directory spells the compiled form, through the
        // compatibility link; the entry spells the resolved one.
        let dir = sysroot.join("lib/examplefw");

        let named = dangling_target(Some(&universe), &dir, Path::new("sub/link.conf"), false);
        assert_eq!(named.as_deref(), Some(Path::new("a.conf")));

        // A sysroot trace records root-relative paths: the member's
        // root-relative spelling names it even though the directory
        // spells itself through the compatibility link.
        let dir = infer::InferredDir {
            source: sysroot.join("lib/examplefw"),
            prefix: sysroot.join("lib"),
            rel: PathBuf::from("examplefw"),
            destination: PathBuf::from("lib/examplefw"),
            kind: infer::InferredKind::Data,
            size: 0,
            compiled: BTreeSet::from(["/lib/examplefw".to_string()]),
            objects: BTreeSet::from(["libexamplefw.so.1".to_string()]),
        };
        let trace = Trim::from_paths(vec![PathBuf::from("/usr/lib/examplefw/a.conf")], None);
        assert!(
            !trimmed_out(&dir, Path::new("a.conf"), Some(&universe), Some(&trace)),
            "the trace's root-relative spelling names the member"
        );
        let none = Trim::from_paths(Vec::new(), None);
        assert!(trimmed_out(
            &dir,
            Path::new("a.conf"),
            Some(&universe),
            Some(&none)
        ));

        let _ = fs::remove_dir_all(&root);
    }

    /// A leading `.` survives as a `CurDir` a `..` may pop on an
    /// empty spelling, so the dotted anchor both sides share
    /// normalizes to one prefix; a target through a subdirectory
    /// lands on the member rel, and a climb past the directory lands
    /// outside the anchor.
    #[test]
    fn lexical_normalization_keeps_dotted_anchors_comparable() {
        let anchor = normalize_lexical(Path::new("./../sysroot/usr/share/examplefw"));
        assert_eq!(anchor, PathBuf::from("sysroot/usr/share/examplefw"));
        let named = normalize_lexical(Path::new("./../sysroot/usr/share/examplefw/sub/../a.conf"));
        assert_eq!(named.strip_prefix(&anchor), Ok(Path::new("a.conf")));
        let escaped = normalize_lexical(Path::new("./../sysroot/usr/share/outside.conf"));
        assert!(escaped.strip_prefix(&anchor).is_err());
    }
}

fn dispose_trimmed_tree_copies(opts: &BundleOptions, trimmed: &[String]) -> io::Result<()> {
    if trimmed.is_empty() {
        return Ok(());
    }
    let entries: std::collections::HashMap<String, sysroot::GeneratedEntry> =
        sysroot::read_generated(&opts.directory)?
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();
    let mut removed: Vec<String> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for rel in trimmed {
        // The member arrives with the one ledger spelling its copy is
        // recorded under; a spelling the ledger does not attest
        // disposes nothing.
        let Some(entry) = entries.get(rel) else {
            continue;
        };
        let at = opts.directory.join(rel);
        let actual = sysroot::hash_entry(&at)?;
        match sysroot::decide_generated_disposal(entry, actual) {
            sysroot::Disposition::Dispose | sysroot::Disposition::Excuse => {
                let _ = fs::remove_file(&at);
                if let Some(parent) = at.parent() {
                    dirs.push(parent.to_path_buf());
                }
                removed.push(rel.clone());
            }
            _ => {}
        }
    }
    if removed.is_empty() {
        return Ok(());
    }
    // Deepest first, so a directory is tried after its children; the
    // total order also puts equal paths side by side for the dedup.
    dirs.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| a.cmp(b))
    });
    dirs.dedup();
    for dir in dirs {
        let mut dir = dir.as_path();
        while dir != opts.directory && fs::remove_dir(dir).is_ok() {
            let Some(parent) = dir.parent() else {
                break;
            };
            dir = parent;
        }
    }
    sysroot::remove_generated(&opts.directory, &removed)
}

/// Collect one inferred directory, as the descriptors' groups are: a
/// data directory is carried whole into the generated ledger, and a
/// plugin directory's members join the walk, each object the collection
/// took resolved or copied at its place in the layout, with every other
/// member carried as data beside them. Under the trim, a data directory
/// keeps the members a trace observed and the keep patterns name, and a
/// plugin directory keeps the members it observed besides its objects'
/// own dependencies; a directory nothing survived in is omitted. Inferred
/// directories are optional, so one the plan refuses, in whole or in
/// part, is undone and omitted with the refusal's reason, without
/// failing the build. The walk's queues are fed only once the plan has
/// held, so an undone directory is not carried through the back door
/// either.
///
/// `carried` holds the destination, source and outcome of every carried
/// directory. One inside a carried destination whose source lies at the
/// same place inside that copy's source is already held by it when the
/// copy is carried, or when the tree holds the child too; a child the
/// tree does not hold is carried on its own. An outer one whose
/// destination contains a carried one is omitted naming it, since its
/// copy is planned already. Any other overlap with a carried destination
/// comes from another source and is omitted naming both, so no two
/// sources write into one place.
#[allow(clippy::too_many_arguments)]
fn collect_inferred(
    dir: &infer::InferredDir,
    carried: &mut Vec<(PathBuf, PathBuf, Carry)>,
    opts: &BundleOptions,
    sysroot: Option<&sysroot::SysrootUniverse>,
    trim: Option<&Trim>,
    (target_class, target_machine): (Option<u8>, Option<u16>),
    excludes: &[&str],
    reachable: &HashSet<String>,
    state: &mut WalkState,
) -> io::Result<()> {
    for (dest, source, carry) in carried.iter() {
        let inner = dir.destination.strip_prefix(dest).ok();
        let outer = dest.strip_prefix(&dir.destination).ok();
        match (inner, outer) {
            (Some(rel), _) if dir.source != source.join(rel) => {
                infer::omit(
                    dir,
                    &format!("{} overlaps it at {}", source.display(), dest.display()),
                );
                return Ok(());
            }
            (Some(_), _) => {
                // Under an applied trim the parent's carry is
                // trace-limited, and its trim judged the child's
                // members too: the subtree rides only with the
                // members the trace opened. The child says so instead
                // of counting the subtree complete.
                if *carry == Carry::Planned && trim.is_some() {
                    infer::report(dir, dir.size, infer::Carried::WithinTrimmed(dest));
                    return Ok(());
                }
                if *carry == Carry::Planned {
                    infer::report(dir, dir.size, infer::Carried::Within(dest));
                    return Ok(());
                }
                // A held parent stands for the child only while the
                // tree holds the child too. A file there, or nothing,
                // is answered by the carry below.
                if opts.directory.join(&dir.destination).is_dir() {
                    infer::report(dir, dir.size, infer::Carried::Held);
                    return Ok(());
                }
            }
            (None, Some(_)) => {
                infer::omit(
                    dir,
                    &format!("{} inside it is carried already", dest.display()),
                );
                return Ok(());
            }
            (None, None) => {}
        }
    }
    let objects = dir.objects.iter().cloned().collect::<Vec<_>>().join(", ");
    let origin = format!("inferred from {objects}");
    // A plugin directory's members are read and classified before
    // anything is planned, so a directory that cannot be read, or one
    // past the collection bound, is omitted with the plan and the
    // queues untouched.
    let mut trimmed_total: Option<(usize, usize)> = None;
    let plugin = if dir.kind == infer::InferredKind::Plugins {
        let mut members = match collect::members(&dir.hit(), sysroot) {
            Ok(members) => members,
            Err(error) => {
                infer::omit(dir, &format!("it cannot be read: {error}"));
                return Ok(());
            }
        };
        let mut collection = match collect::collect_objects(
            "inference",
            &dir.destination.display().to_string(),
            &dir.hit(),
            &members,
            target_class,
            target_machine,
        ) {
            Ok(collection) => collection,
            Err(error) => {
                infer::omit(dir, &error.to_string());
                return Ok(());
            }
        };
        // The trim keeps only what the trace observed: a member it did
        // not open, and that no keep pattern names, leaves the
        // collection with its own reason, and a directory nothing
        // survived in is omitted. A member the trim cannot name under
        // the directory, a link landing outside it, keeps: the
        // conservative side. A kept relative link keeps the member it
        // names. The tree copies the sysroot populate wrote of the
        // members left out are disposed by the ledger's hash.
        if let Some(trim) = trim {
            let total = members.len();
            let mut kept = Vec::new();
            for p in std::mem::take(&mut collection.plugins) {
                // The member is judged by the name the group collected
                // it under: a link collected as libfoo.so is what the
                // trace opened, whatever file it resolves to.
                let rel = p
                    .destination
                    .strip_prefix(&dir.destination)
                    .or_else(|_| p.resolved.strip_prefix(&dir.source))
                    .or_else(|_| p.resolved.strip_prefix(&dir.prefix))
                    .ok()
                    .map(|r| r.to_path_buf());
                let out = rel
                    .as_ref()
                    .filter(|rel| trimmed_out(dir, rel, sysroot, Some(trim)))
                    .map(|r| r.display().to_string());
                if let Some(shown) = out {
                    collection
                        .skipped
                        .push((shown, "the trace did not open it".to_string()));
                } else {
                    kept.push(p);
                }
            }
            collection.plugins = kept;
            let mut trimmed_members: Vec<(PathBuf, PathBuf)> = Vec::new();
            for (rel, file) in std::mem::take(&mut members) {
                if trimmed_out(dir, &rel, sysroot, Some(trim)) {
                    trimmed_members.push((rel, file));
                } else {
                    members.push((rel, file));
                }
            }
            keep_link_targets(
                sysroot,
                dir,
                Some(&collection.plugins),
                &mut members,
                &mut trimmed_members,
            );
            let trimmed_rels: Vec<String> = trimmed_members
                .iter()
                .map(|(rel, _)| disposal_spelling(sysroot, dir, rel))
                .collect();
            trimmed_total = Some((members.len(), total));
            dispose_trimmed_tree_copies(opts, &trimmed_rels)?;
            if collection.plugins.is_empty() && members.is_empty() {
                infer::omit(dir, "the trace opened nothing in it");
                return Ok(());
            }
        }
        Some((members, collection))
    } else {
        None
    };
    // The trace-limited data directory: its members are enumerated and
    // filtered before the plan, so a directory nothing survived in is
    // omitted with the plan and the queues untouched. A destination the
    // tree already holds is the tree's own, and the trim does not cut
    // it; the members the trim leaves out have their populate-written
    // tree copies disposed by the ledger's hash. The disposal commits
    // with the trim's judgment, not the carry plan: a directory the
    // plan later refuses keeps its kept members' populate copies, the
    // way every omitted directory of a populated tree does, and its
    // trimmed members' copies are gone either way; the next build
    // re-populates whatever it needs.
    let trimmed_data = if plugin.is_none() {
        match trim {
            Some(trim) => match collect::members(&dir.hit(), sysroot) {
                Ok(members) => {
                    let total = members.len();
                    let mut kept = Vec::new();
                    let mut trimmed: Vec<(PathBuf, PathBuf)> = Vec::new();
                    for (rel, file) in members {
                        if trimmed_out(dir, &rel, sysroot, Some(trim)) {
                            trimmed.push((rel, file));
                        } else {
                            kept.push((rel, file));
                        }
                    }
                    keep_link_targets(sysroot, dir, None, &mut kept, &mut trimmed);
                    let trimmed_rels: Vec<String> = trimmed
                        .iter()
                        .map(|(rel, _)| disposal_spelling(sysroot, dir, rel))
                        .collect();
                    dispose_trimmed_tree_copies(opts, &trimmed_rels)?;
                    if kept.is_empty() {
                        infer::omit(dir, "the trace opened nothing in it");
                        return Ok(());
                    }
                    Some((kept, total))
                }
                Err(error) => {
                    infer::omit(dir, &format!("it cannot be read: {error}"));
                    return Ok(());
                }
            },
            None => None,
        }
    } else {
        None
    };
    // The carry is all or nothing. A directory the plan admits only in
    // part, or refuses outright, is undone, and the omission names the
    // first refusal; a finding left behind would stop the build over a
    // directory that was optional all along. The walk's queues are fed
    // only once the plan has held, so an undone directory is not
    // carried through the back door either.
    let checkpoint = state.plan.checkpoint();
    let planned_actions = state.actions.len();
    let planned = match plugin {
        Some((members, collection)) => plan_plugin_carry(
            dir,
            &members,
            collection,
            trimmed_total,
            &origin,
            opts,
            sysroot,
            state,
        ),
        None => match trimmed_data {
            Some((members, total)) => {
                let kept = members.len();
                // The finding reports the bytes the kept members hold,
                // judged at the entry each name is: a link at the entry
                // counts nothing even where it resolves to a file, the
                // way the whole directory's size is classified. The
                // plan may skip writing members the tree already
                // holds, but they are still what the directory keeps.
                let hit = dir.hit();
                let size: u64 = members
                    .iter()
                    .filter_map(|(rel, _)| {
                        let entry = member_entry(sysroot, &hit.resolved, rel)?;
                        let meta = fs::symlink_metadata(entry).ok()?;
                        meta.is_file().then_some(meta.len())
                    })
                    .sum();
                plan_member_data(dir, &members, &origin, sysroot, state).map(|(files, _)| {
                    InferredPlan::TrimmedData {
                        files,
                        size,
                        kept,
                        total,
                    }
                })
            }
            _ => carry_hit(
                &opts.directory,
                &objects,
                &dir.hit(),
                Vec::new(),
                sysroot,
                state.plan,
                state.actions,
                &origin,
            )
            .map(InferredPlan::Data),
        },
    };
    let planned = match planned {
        Ok(planned) if !state.plan.found_since(&checkpoint) => planned,
        otherwise => {
            let rolled = state.plan.restore(checkpoint);
            state.actions.truncate(planned_actions);
            let why = match otherwise {
                Err(error) => format!("it cannot be carried: {error}"),
                Ok(_) => rolled
                    .first()
                    .map(|finding| format!("it {}", finding.clause()))
                    .unwrap_or_else(|| "it collides with another write".to_string()),
            };
            infer::omit(dir, &why);
            return Ok(());
        }
    };
    match planned {
        InferredPlan::Data(carry) => match carry {
            Carry::Planned => {
                infer::report(dir, dir.size, infer::Carried::Data);
                carried.push((dir.destination.clone(), dir.source.clone(), carry));
            }
            Carry::Held => {
                if opts.directory.join(&dir.destination).is_dir() {
                    infer::report(dir, dir.size, infer::Carried::Held);
                    carried.push((dir.destination.clone(), dir.source.clone(), carry));
                } else {
                    infer::omit(dir, "its destination is already in the tree");
                }
            }
            Carry::Rejected => {
                infer::omit(dir, "its destination clashes with another write");
            }
            Carry::Empty => {
                infer::omit(dir, "the build admitted nothing below it");
            }
        },
        InferredPlan::TrimmedData {
            files,
            size,
            kept,
            total,
        } => {
            let wrote = !files.is_empty();
            if wrote {
                state.actions.push(WalkAction::CarryData {
                    rule: objects,
                    src: dir.source.clone(),
                    rel: dir.destination.clone(),
                    hooks: Vec::new(),
                    members: Some(files),
                });
            }
            infer::report(dir, size, infer::Carried::Trimmed { kept, total });
            // The destination stands for itself only when something was
            // written under it, like the plugins arm: a trim whose kept
            // members are all the tree's own copies does not answer for
            // nested directories.
            if wrote {
                carried.push((dir.destination.clone(), dir.source.clone(), Carry::Planned));
            }
        }
        InferredPlan::Plugins {
            collection,
            firsts,
            files,
            size,
            trimmed,
        } => {
            // The plan held: the members join the walk, and the data
            // members are carried beside them.
            queue_collection(
                &collection,
                &origin,
                excludes,
                reachable,
                &firsts,
                state,
                sysroot,
            )?;
            let carried_files = files.len();
            if !files.is_empty() {
                state.actions.push(WalkAction::CarryData {
                    rule: objects,
                    src: dir.source.clone(),
                    rel: dir.destination.clone(),
                    hooks: Vec::new(),
                    members: Some(files),
                });
            }
            infer::report(
                dir,
                size,
                infer::Carried::Plugins(&collection, carried_files, trimmed),
            );
            if !collection.plugins.is_empty() || carried_files > 0 {
                carried.push((dir.destination.clone(), dir.source.clone(), Carry::Planned));
            }
        }
    }
    Ok(())
}

/// Plan the copy of one collected group member to its own destination,
/// from the file the group found: an existing destination is the tree's
/// own and is kept, the way the walk keeps a library it already holds.
fn plan_member_copy(
    directory: &Path,
    plugin: &collect::CollectedPlugin,
    origin: &str,
    plan: &mut DestinationPlan,
    actions: &mut Vec<WalkAction>,
) -> io::Result<()> {
    let rel = &plugin.destination;
    if fs::symlink_metadata(directory.join(rel)).is_ok() {
        return Ok(());
    }
    let bytes = fs::read(&plugin.resolved)?;
    plan.note_source(&plugin.resolved, &bytes)?;
    if plan.admit_file(rel, plan::file_hash(&bytes), origin, Existing::SkipOnAny)?
        == Admission::Write
    {
        actions.push(WalkAction::CopyLib {
            soname: plugin.soname.clone(),
            resolved: plugin.resolved.clone(),
            rel: rel.clone(),
            size: bytes.len() as u64,
            requirer: origin.to_string(),
            stale_flat: false,
            libc_data: false,
        });
    }
    Ok(())
}

/// The admitted files under a sysroot directory hit, each admitted to
/// the plan at its destination beneath the hit's, as the source and the
/// tree-relative destination the carry writes. A relative link is
/// planned as a link; an absolute one has no place in the tree and is
/// left out, as a host carry leaves it.
fn admitted_members(
    universe: &sysroot::SysrootUniverse,
    hit: &CandidateHit,
    plan: &mut DestinationPlan,
    origin: &str,
) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let Ok(dir_rel) = hit.resolved.strip_prefix(universe.root()) else {
        return Ok(Vec::new());
    };
    let dir_rel = dir_rel.to_string_lossy().into_owned();
    let mut members = Vec::new();
    for file in universe.admitted_under(&dir_rel) {
        let Ok(under) = Path::new(&file).strip_prefix(&dir_rel) else {
            continue;
        };
        let from = universe.root().join(&file);
        let rel = hit.destination.join(under);
        let meta = fs::symlink_metadata(&from)?;
        let admission = if meta.file_type().is_symlink() {
            let target = fs::read_link(&from)?;
            if !target.is_relative() {
                continue;
            }
            plan.admit_link(&rel, &target.to_string_lossy(), origin, Existing::SkipOnAny)?
        } else if meta.is_file() {
            plan.admit_carried(&rel, &from, origin)?
        } else {
            continue;
        };
        if admission == Admission::Write {
            members.push((from, rel));
        }
    }
    Ok(members)
}

/// The required-data failure: which rule's data is missing, at what
/// destination, and what was examined where, bounded like every
/// finding.
/// The finding for a data group the selected source lacks: a required
/// failure naming what the group requires and where it looked, or an
/// optional omission the build continues without.
fn data_miss(rule: &str, declared: &ResourceGroup, outcome: &GroupOutcome) -> Finding {
    let examined = examined_locations(outcome);
    let reason = match declared.requirement {
        Requirement::Required => {
            let mut text = format!(
                "group {} requires {} data at {}, but none of its declared locations \
                 exist in the selected source",
                declared.id.as_str(),
                match declared.kind {
                    ResourceKind::File => "file",
                    ResourceKind::Directory => "directory",
                },
                declared.destination.as_str()
            );
            for location in examined {
                text.push_str("; ");
                text.push_str(&location);
            }
            text
        }
        Requirement::Optional => format!(
            "group {}: optional data unavailable ({}); the build continues without it",
            declared.id.as_str(),
            if examined.is_empty() {
                "no declared location".to_string()
            } else {
                examined.join(", ")
            }
        ),
    };
    Finding::new(
        declared.destination.as_str(),
        match declared.requirement {
            Requirement::Required => Outcome::RequiredFailure,
            Requirement::Optional => Outcome::OptionalOmission,
        },
        reason,
    )
    .of_rule(rule)
}

/// Where the group looked and why each location held nothing, with the
/// same location under several alternatives named once, bounded like
/// every finding.
fn examined_locations(outcome: &GroupOutcome) -> Vec<String> {
    let mut shown: Vec<String> = Vec::new();
    for miss in outcome.misses.iter().take(12) {
        let text = format!("{}: {}", miss.root.display(), miss_text(&miss.reason));
        if !shown.contains(&text) {
            shown.push(text);
        }
    }
    shown.truncate(3);
    shown
}

/// Why one examined alternative is not the carry, in the shared wording.
pub(crate) fn miss_text(reason: &MissReason) -> String {
    match reason {
        MissReason::Absent => "absent".to_string(),
        MissReason::WrongKind {
            actual: ResourceKind::File,
        } => "a file where a directory is declared".to_string(),
        MissReason::WrongKind {
            actual: ResourceKind::Directory,
        } => "a directory where a file is declared".to_string(),
        MissReason::OutsideRoot => "outside the admitted root".to_string(),
        MissReason::NotAdmitted(refusal) => match refusal {
            sysroot::Refusal::Absent => "not in the selected source".to_string(),
            sysroot::Refusal::HostPackage { owner } => {
                format!("left to the host package {owner}")
            }
            sysroot::Refusal::UnselectedPackage { owner } => {
                format!("owned by the unselected package {owner}")
            }
            sysroot::Refusal::Pruned { owner } => format!("pruned from the package {owner}"),
        },
    }
}

/// Carry one activated rule's declared helper executable into the tree
/// through the common object queue. The declared alternatives are
/// discovered against the triggering libraries' own roots, the same
/// boundary the data groups read, and the executable the first
/// alternative yields is enqueued as an object: activation, the
/// transitive dependency closure, the locate and compatibility checks,
/// plan admission, the run path rewrite and hash aliasing are the
/// walk's ordinary ones, and the final tree pass finalizes the helper
/// beside every other copied object. The declared destination is
/// relative to the tree root, and the rule's runtime mapping resolves
/// the helper there, so the alias keeps working wherever the tree is
/// moved.
///
/// A destination the tree already holds satisfies the group. What a
/// declared set holds is prepared into each supplying pin's subtree
/// instead, so the helper never executes from a raw set root. A
/// required helper neither the tree nor a declared set supplies fails
/// the assembly naming the rule, the helper and the examined locations;
/// an optional one is reported and the build continues.
#[allow(clippy::too_many_arguments)]
fn collect_helper_group(
    directory: &Path,
    descriptor: &Descriptor,
    declared: &HelperGroup,
    admitted: &[AdmittedLibrary],
    sysroot: Option<&sysroot::SysrootUniverse>,
    search_paths: &mut Vec<PathBuf>,
    repair_extra: &mut HashSet<PathBuf>,
    destinations: &mut HashMap<String, PathBuf>,
    helpers: &mut HashSet<String>,
    needed_by: &mut HashMap<String, String>,
    processed: &mut HashSet<String>,
    discovery: &mut Discovery,
    preparation: &mut SetPreparation,
    plan: &mut DestinationPlan,
    required: &mut HashSet<PathBuf>,
) -> io::Result<()> {
    let rule = descriptor.id.as_str();
    if declared.requirement == Requirement::Required {
        required.insert(PathBuf::from(declared.destination.as_str()));
    }
    let owners: Vec<AdmittedLibrary> = admitted
        .iter()
        .filter(|lib| {
            descriptor
                .triggers
                .iter()
                .any(|t| t.matches_soname(lib.soname()))
        })
        .map(AdmittedLibrary::without_provenance)
        .collect();
    let outcome = discover_group(
        &GroupQuery {
            rule,
            group: declared.id.as_str(),
            kind: declared.kind,
            destination: &declared.destination,
            candidates: &declared.candidates,
        },
        &AdmittedUniverse {
            libraries: &owners,
            sysroot,
        },
    )?;
    let Some(hit) = outcome.found.first() else {
        if directory.join(declared.destination.as_str()).exists() {
            return Ok(());
        }
        let supplied = preparation.supply_group(
            plan,
            &SupplyQuery {
                rule,
                group: declared.id.as_str(),
                kind: declared.kind,
                destination: declared.destination.as_str(),
                candidates: &declared.candidates,
                executable: true,
                hooks: &descriptor.build.hooks,
                runtime: &descriptor.runtime.paths,
            },
        )?;
        if supplied {
            return Ok(());
        }
        let miss = helper_miss(rule, declared, &outcome);
        completeness::record(miss.clone());
        return match declared.requirement {
            Requirement::Required if !preparation.dry_run => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("rule {rule} {}", miss.reason),
            )),
            _ => Ok(()),
        };
    };
    let Some(source_name) = hit.resolved.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    if let Some(dir) = hit.resolved.parent() {
        let dir = dir.to_path_buf();
        if !search_paths.contains(&dir) {
            search_paths.push(dir.clone());
        }
        repair_extra.insert(dir);
    }
    destinations
        .entry(source_name.to_string())
        .or_insert_with(|| hit.destination.clone());
    helpers.insert(source_name.to_string());
    needed_by
        .entry(source_name.to_string())
        .or_insert_with(|| format!("rule {rule} helper {}", declared.id.as_str()));
    // An object that drained before its group popped chose its flat
    // destination; the helper's declared destination supersedes it.
    processed.remove(source_name);
    discovery.enqueue(source_name)?;
    eprintln!(
        "  {} helper {source_name} for {rule}/{} from {}",
        color::bold_green("Collected"),
        declared.id.as_str(),
        hit.resolved.display()
    );
    Ok(())
}

/// The required-helper failure: which rule's helper is missing, at what
/// destination, and what was examined where, bounded like every finding.
/// The finding for a helper the selected source lacks: a required
/// failure naming the executable and where it looked, or an optional
/// omission the build continues without.
fn helper_miss(rule: &str, declared: &HelperGroup, outcome: &GroupOutcome) -> Finding {
    let examined = examined_locations(outcome);
    let reason = match declared.requirement {
        Requirement::Required => {
            let mut text = format!(
                "helper {} requires the executable at {}, but none of its declared \
                 locations exist in the selected source",
                declared.id.as_str(),
                declared.destination.as_str()
            );
            for location in examined {
                text.push_str("; ");
                text.push_str(&location);
            }
            text
        }
        Requirement::Optional => format!(
            "helper {}: optional executable unavailable ({}); the build continues without it",
            declared.id.as_str(),
            if examined.is_empty() {
                "no declared location".to_string()
            } else {
                examined.join(", ")
            }
        ),
    };
    Finding::new(
        declared.destination.as_str(),
        match declared.requirement {
            Requirement::Required => Outcome::RequiredFailure,
            Requirement::Optional => Outcome::OptionalOmission,
        },
        reason,
    )
    .of_rule(rule)
}

/// Where an application keeps the resources a selected set pin's
/// preparation writes, relative to the AppDir root: the shared contract
/// a launch resolves a selected pin's bound mapping against.
///
/// The packer reserves `.onelf/` for the metadata it injects itself, so
/// the prepared subtrees live beside it at the top level and are packed
/// as the application's own content.
pub use onelf_format::runtime_metadata::PREPARED_DIR;

/// The subtree one selected pin's prepared resources live in,
/// relative to the AppDir: `PREPARED_DIR/<set>/<pin>`, the pin spelled
/// as the lowercase hex of its BLAKE3 digest.
///
/// The pin is the destination key, so different builds of one set name
/// never write one destination: two versions of a helper coexist at
/// their own paths, and a launch consumes the subtree of whichever pin
/// its selection named.
pub fn prepared_subtree(set: &str, pin: &str) -> io::Result<PathBuf> {
    sysroot::check_set_name(set)?;
    let digest = pin.parse::<blake3::Hash>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "set pin is not a blake3 digest",
        )
    })?;
    Ok(Path::new(PREPARED_DIR)
        .join(set)
        .join(digest.to_hex().as_ref()))
}

/// One selected pin whose set-supplied helper and configuration
/// resources the assembly prepared.
///
/// Every file was decompressed out of the pinned build through the
/// container's verified path and written once into the pin's own
/// subtree, so the pin's bytes are the only thing the subtree can hold.
/// The preparation binds the subtree to that one pin: a launch whose
/// selection does not name the pin never activates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPin {
    /// The set declaration name.
    pub set: String,
    /// The pinned build the bytes were read from.
    pub pin: String,
    /// The pin's subtree, relative to the AppDir.
    pub root: PathBuf,
    /// The files this build prepared, relative to the AppDir, sorted.
    pub files: Vec<String>,
    /// The rules whose resources the pin's subtree holds, sorted. Each
    /// lowers a runtime record bound to this pin.
    pub rules: Vec<String>,
}

/// The largest single resource preparation reads out of a set build.
/// A set's bulk libraries and plugin trees are not prepared at all, so
/// anything past this bound is a layout the preparation does not
/// expect, and the build says so instead of copying unbounded bytes.
const PREPARED_FILE_LIMIT: u64 = 16 * 1024 * 1024;

/// The largest total bytes one pin's subtree may hold. Small
/// configuration resources and helpers are the declared scope; a set
/// whose activated groups would prepare more than this belongs in the
/// set itself.
const PREPARED_TOTAL_LIMIT: u64 = 64 * 1024 * 1024;

/// The most entries one prepared directory may expand to, matching the
/// metadata record bound.
const PREPARED_ENTRY_LIMIT: usize = 4096;

/// The paths one declared candidate may sit at inside a set build. A
/// set's `usr/` is flattened into the build root, so an installation
/// prefix is the artifact root and a library directory is its `lib`;
/// the unflattened spellings stay in the search for builds that kept
/// their prefix. The order is fixed, so the same build answers the same
/// way every time.
fn artifact_candidate_paths(candidate: &SourceCandidate) -> Vec<String> {
    let path = candidate.path.as_str();
    let mut paths = Vec::with_capacity(4);
    let mut push = |p: String| {
        if !paths.contains(&p) {
            paths.push(p);
        }
    };
    match candidate.base {
        SourceBase::InstallationPrefix => {
            push(path.to_string());
            push(format!("usr/{path}"));
        }
        SourceBase::LibraryDir => {
            push(format!("lib/{path}"));
            push(format!("usr/lib/{path}"));
            push(path.to_string());
            push(format!("usr/{path}"));
        }
    }
    paths
}

/// Whether a set-supplied helper can execute without an interpreter the
/// set's host chose.
///
/// An ELF helper needs no interpreter. A script names one on its first
/// line, and the launch would then run whatever that path finds on the
/// machine it lands on: an interpreter from the packer's tree or the
/// set's build host, never one the application shipped. `/bin/sh` is
/// the one name every launch environment provides, so it is the only
/// one accepted; anything else is a declared helper that cannot run
/// safely, and the failure says what to ship instead.
fn check_prepared_helper(rel: &Path, bytes: &[u8]) -> io::Result<()> {
    if !bytes.starts_with(b"#!") {
        return Ok(());
    }
    let line_end = bytes
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(bytes.len());
    let line = String::from_utf8_lossy(&bytes[2..line_end]);
    let interpreter = line.split_whitespace().next().unwrap_or("");
    if interpreter == "/bin/sh" {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: the set supplies this helper as a script naming the interpreter \
             `{interpreter}`, which a launch must never run from a set host. Ship the \
             helper as an ELF executable, or as a /bin/sh script that execs the \
             application's own interpreter through the package launcher",
            rel.display()
        ),
    ))
}

/// The resources one pin's subtree accumulates while group collection
/// consults the declared sets: verified bytes keyed by their
/// subtree-relative destination, whether each is an executable, the
/// owning rules' declared hooks, the running total the size bound
/// checks, and the AppDir-relative paths the build wrote.
struct PinPreparation {
    root: PathBuf,
    entries: Vec<(PathBuf, Vec<u8>, bool)>,
    hooks: Vec<BuildHookId>,
    bytes: u64,
    files: Vec<String>,
    rules: BTreeSet<String>,
}

/// One activated group a declared set may supply: what the declaration
/// asks for, where it lands inside a pin's subtree, whether it is an
/// executable, and which build hooks the rule binds to it.
struct SupplyQuery<'a> {
    rule: &'a str,
    group: &'a str,
    kind: ResourceKind,
    destination: &'a str,
    candidates: &'a [SourceCandidate],
    executable: bool,
    hooks: &'a [BuildHookId],
    /// The rule's runtime path groups, which decide whether a launch
    /// would find the prepared destination at all.
    runtime: &'a [RuntimePathGroup],
}

/// Whether a runtime path group of the rule names `destination` inside a
/// prepared subtree: a package-root candidate at the destination itself,
/// or a library-root candidate below `lib_dir`. A launch evaluates a
/// selected pin's bound record against the subtree with exactly those
/// bases, so a resource no candidate names would be prepared and never
/// mapped.
fn runtime_maps(runtime: &[RuntimePathGroup], destination: &str, lib_dir: &Path) -> bool {
    let below_lib = Path::new(destination).strip_prefix(lib_dir).ok();
    runtime
        .iter()
        .flat_map(|group| &group.candidates)
        .any(|candidate| match candidate.base {
            RuntimeBase::PackageRoot => candidate.path.as_str() == destination,
            RuntimeBase::LibraryRoot => {
                below_lib.is_some_and(|rest| rest == Path::new(candidate.path.as_str()))
            }
        })
}

/// The set-supplied resources a checked assembly prepares while its
/// group collection runs: every set the build declares, whether pinned
/// without a sysroot or named by a database-backed sysroot's recipe
/// section, whose verified payload holds a resource an activated group
/// declared, keyed by the pin the bytes were read from.
///
/// Nothing here executes a helper, an interpreter, or an editing tool.
/// Resources are read verified out of the pinned build, staged into the
/// pin's subtree, and transformed only through the registered staged
/// hooks, whose procedures are fixed functions over the staged bytes.
struct SetPreparation<'a> {
    sets: &'a [sysroot::SharedSet],
    sysroot_sets: &'a [sysroot::SharedSet],
    lib_dir: &'a Path,
    dry_run: bool,
    reads: HashMap<String, sysroot::VerifiedSet>,
    pins: BTreeMap<(String, String), PinPreparation>,
    warned: HashSet<String>,
}

impl<'a> SetPreparation<'a> {
    /// The preparation over `sets` and the sysroot's `sysroot_sets`. A
    /// dry run consults no build: a dry run fetches nothing, and a set
    /// whose build is not already in the store has nothing to read on
    /// those terms.
    fn new(
        sets: &'a [sysroot::SharedSet],
        sysroot_sets: &'a [sysroot::SharedSet],
        lib_dir: &'a Path,
        dry_run: bool,
    ) -> Self {
        Self {
            sets,
            sysroot_sets,
            lib_dir,
            dry_run,
            reads: HashMap::new(),
            pins: BTreeMap::new(),
            warned: HashSet::new(),
        }
    }

    /// Whether the assembly consults the sets at all: a set-less build
    /// prepares nothing, and a dry run fetches nothing, so its report
    /// never rests on bytes it did not read.
    fn active(&self) -> bool {
        !(self.sets.is_empty() && self.sysroot_sets.is_empty()) && !self.dry_run
    }

    /// Prepare one activated group's resource out of every declared set
    /// whose verified payload holds it, into each supplying pin's own
    /// subtree. The first declared alternative the build holds wins,
    /// the way the tree carry takes the first alternative the selected
    /// source holds. Returns whether any pin ended up supplying the
    /// group, which satisfies a required declaration whose tree carry
    /// missed.
    ///
    /// The build lowers one runtime record per rule and pin bound to the
    /// pin's subtree, and a launch selecting that pin evaluates the
    /// rule's runtime paths against the subtree before the raw set root.
    /// A resource is therefore prepared, and counts as supplied, only
    /// when one of the rule's runtime candidates names its destination;
    /// a resource no mapping would reach is left in the set, and a
    /// required one fails as a miss. The requirement holds for the
    /// entrypoints selecting the supplying pin, which are the only ones
    /// preparation reads sets for.
    fn supply_group(
        &mut self,
        plan: &mut DestinationPlan,
        query: &SupplyQuery,
    ) -> io::Result<bool> {
        if !self.active() || !runtime_maps(query.runtime, query.destination, self.lib_dir) {
            return Ok(false);
        }
        let mut supplied = false;
        let lists = [self.sets, self.sysroot_sets];
        for set in lists.into_iter().flatten() {
            let read = match open_set_read(&mut self.reads, &mut self.warned, set) {
                Ok(read) => read,
                Err(_) => continue,
            };
            let Some(found) = take_candidate(read, set, query)? else {
                continue;
            };
            let root = prepared_subtree(&set.name, &set.blake3)?;
            let key = (set.name.clone(), set.blake3.to_ascii_lowercase());
            let bytes_in_pin: u64 = self.pins.get(&key).map(|p| p.bytes).unwrap_or(0);
            let origin = format!(
                "set {} (blake3 {}) rule {} group {} prepared",
                set.name,
                short_pin(&key.1),
                query.rule,
                query.group
            );
            let mut staged: Vec<(PathBuf, Vec<u8>, bool)> = Vec::new();
            let mut staged_files: Vec<String> = Vec::new();
            let mut staged_bytes = 0u64;
            for (rel, bytes) in found {
                if bytes.len() as u64 > PREPARED_FILE_LIMIT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{}: larger than the {PREPARED_FILE_LIMIT} byte preparation \
                             limit; keep this resource in the set instead of preparing it",
                            root.join(&rel).display()
                        ),
                    ));
                }
                if bytes_in_pin + staged_bytes + bytes.len() as u64 > PREPARED_TOTAL_LIMIT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "set {} (blake3 {}): prepared resources would pass the \
                             {PREPARED_TOTAL_LIMIT} byte per-pin limit; keep the bulk in \
                             the set",
                            set.name,
                            short_pin(&key.1)
                        ),
                    ));
                }
                let app_rel = root.join(&rel);
                match plan.admit_file(
                    &app_rel,
                    plan::file_hash(&bytes),
                    &origin,
                    Existing::SkipOnAny,
                )? {
                    Admission::Write => {
                        staged_bytes += bytes.len() as u64;
                        staged_files.push(app_rel.to_string_lossy().into_owned());
                        staged.push((rel, bytes, query.executable));
                    }
                    Admission::Skip | Admission::Rejected => {}
                }
            }
            supplied = true;
            let pin = self
                .pins
                .entry(key.clone())
                .or_insert_with(|| PinPreparation {
                    root,
                    entries: Vec::new(),
                    hooks: Vec::new(),
                    bytes: bytes_in_pin,
                    files: Vec::new(),
                    rules: BTreeSet::new(),
                });
            pin.rules.insert(query.rule.to_string());
            pin.bytes += staged_bytes;
            pin.entries.extend(staged);
            pin.files.extend(staged_files);
            pin.files.sort();
            pin.files.dedup();
            for hook in query.hooks {
                if !pin.hooks.contains(hook) {
                    pin.hooks.push(*hook);
                }
            }
        }
        Ok(supplied)
    }

    /// Queue one write per pin that accumulated entries, each carrying
    /// the verified bytes and the rules' declared hooks, and return every
    /// pin that supplied a group, including one whose subtree already
    /// held identical bytes and needs no write. Deterministic: the pin map
    /// is ordered by set and pin, and each pin's entries are sorted by
    /// destination.
    fn queue_actions(&mut self, actions: &mut Vec<WalkAction>) -> Vec<PreparedPin> {
        let mut prepared = Vec::new();
        for ((set, pin), done) in self.pins.iter_mut() {
            if !done.entries.is_empty() {
                let mut entries = std::mem::take(&mut done.entries);
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                actions.push(WalkAction::PreparePin {
                    set: set.clone(),
                    pin: pin.clone(),
                    root: done.root.clone(),
                    entries,
                    hooks: done.hooks.clone(),
                });
            }
            prepared.push(PreparedPin {
                set: set.clone(),
                pin: pin.clone(),
                root: done.root.clone(),
                files: std::mem::take(&mut done.files),
                rules: done.rules.iter().cloned().collect(),
            });
        }
        prepared
    }
}

thread_local! {
    static LAST_PREPARED: RefCell<Vec<PreparedPin>> = const { RefCell::new(Vec::new()) };
    static LAST_PAIRS: RefCell<Vec<PathPair>> = const { RefCell::new(Vec::new()) };
}

/// Publish the pins one bundle run prepared resources into.
fn publish_prepared(prepared: Vec<PreparedPin>) {
    LAST_PREPARED.with(|cell| *cell.borrow_mut() = prepared);
}

/// Take the prepared pins of the last bundle run on this thread, in
/// canonical order by set and pin. A run that prepared nothing, or
/// failed before preparation, leaves none.
pub fn take_prepared() -> Vec<PreparedPin> {
    LAST_PREPARED.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

/// Publish the pairs one bundle run chose for the package's paths
/// record.
fn publish_pairs(pairs: Vec<PathPair>) {
    LAST_PAIRS.with(|cell| *cell.borrow_mut() = pairs);
}

/// Take the pairs of the last bundle run on this thread, sorted by
/// absolute path. A run that chose no overlay, or failed after the
/// decision, leaves none.
pub fn take_pairs() -> Vec<PathPair> {
    LAST_PAIRS.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

/// The verified reader of one set's pinned build, opened once and reused
/// across groups. A build that cannot be had on pinned terms is reported
/// once and claims nothing, the way the provider audit treats a build it
/// cannot check.
fn open_set_read<'r>(
    reads: &'r mut HashMap<String, sysroot::VerifiedSet>,
    warned: &mut HashSet<String>,
    set: &sysroot::SharedSet,
) -> Result<&'r mut sysroot::VerifiedSet, String> {
    if reads.contains_key(&set.name) {
        return Ok(reads.get_mut(&set.name).unwrap());
    }
    match sysroot::VerifiedSet::open(set) {
        Ok(read) => {
            reads.insert(set.name.clone(), read);
            Ok(reads.get_mut(&set.name).unwrap())
        }
        Err(why) => {
            if warned.insert(set.name.clone()) {
                eprintln!(
                    "  {} set {} could not be prepared from ({why}); its resources stay \
                     in the set",
                    color::bold_red("warning:"),
                    color::cyan(&set.name)
                );
            }
            Err(why)
        }
    }
}

/// The verified bytes of one build path, as an integrity failure naming
/// the set and the path when the pinned build does not reproduce them.
fn read_verified(read: &mut sysroot::VerifiedSet, path: &str) -> io::Result<Option<Vec<u8>>> {
    read.read(path).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("set {}: {e}", read.set()),
        )
    })
}

/// One prepared entry: its destination inside the pin's subtree and the
/// verified bytes it holds.
type PreparedEntry = (PathBuf, Vec<u8>);

/// The first declared alternative one set's verified payload holds: an
/// exact file for a file declaration, and every file under the prefix,
/// layout preserved, for a directory declaration. The destination inside
/// the pin's subtree is the declared one, so the runtime mapping that
/// names the declared path resolves it there.
fn take_candidate(
    read: &mut sysroot::VerifiedSet,
    set: &sysroot::SharedSet,
    query: &SupplyQuery,
) -> io::Result<Option<Vec<PreparedEntry>>> {
    for candidate in query.candidates {
        for base in artifact_candidate_paths(candidate) {
            match query.kind {
                ResourceKind::File => {
                    if let Some(bytes) = read_verified(read, &base)? {
                        let rel = prepared_rel(query, Path::new(""))?;
                        if query.executable {
                            check_prepared_helper(&rel, &bytes)?;
                        }
                        return Ok(Some(vec![(rel, bytes)]));
                    }
                }
                ResourceKind::Directory => {
                    let prefix = format!("{base}/");
                    let mut under: Vec<String> = read
                        .file_paths()
                        .into_iter()
                        .filter(|path| path.starts_with(&prefix))
                        .collect();
                    if under.is_empty() {
                        continue;
                    }
                    under.sort();
                    if under.len() > PREPARED_ENTRY_LIMIT {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "set {} rule {} group {}: more than \
                                 {PREPARED_ENTRY_LIMIT} entries under {}; keep the \
                                 tree in the set",
                                set.name,
                                query.rule,
                                query.group,
                                read.build().display()
                            ),
                        ));
                    }
                    let mut found = Vec::with_capacity(under.len());
                    for path in under {
                        let bytes = read_verified(read, &path)?.ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("{}: vanished from the verified listing", path),
                            )
                        })?;
                        let rel = prepared_rel(query, Path::new(&path[prefix.len()..]))?;
                        found.push((rel, bytes));
                    }
                    return Ok(Some(found));
                }
            }
        }
    }
    Ok(None)
}

/// The destination one prepared entry takes inside the pin's subtree:
/// the declared destination, with a directory's layout preserved under
/// it. A path that would leave the subtree is the traversal the
/// preparation refuses.
fn prepared_rel(query: &SupplyQuery, under: &Path) -> io::Result<PathBuf> {
    let mut rel = PathBuf::from(query.destination);
    if !under.as_os_str().is_empty() {
        rel.push(under);
    }
    if !rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "rule {} group {}: {} would leave the prepared subtree",
                query.rule,
                query.group,
                rel.display()
            ),
        ));
    }
    Ok(rel)
}

/// One write the planned walk performs once the whole plan has resolved
/// without findings. The executor replays these in planning order, so
/// the output and the mutations match the single-pass walk this
/// replaces.
#[derive(Debug)]
enum WalkAction {
    /// Copy a resolved library or plugin into its destination, relative
    /// to the tree root.
    CopyLib {
        soname: String,
        resolved: PathBuf,
        rel: PathBuf,
        size: u64,
        requirer: String,
        stale_flat: bool,
        libc_data: bool,
    },
    /// Link one soname's destination onto identical content already
    /// bundled under another name.
    LinkAlias {
        soname: String,
        resolved: PathBuf,
        rel: PathBuf,
        existing: PathBuf,
        requirer: String,
        size: u64,
        stale_flat: bool,
    },
    /// Copy one carried data path beside the tree root, from the
    /// alternative the group took, then run the rule's declared hooks.
    CarryData {
        rule: String,
        src: PathBuf,
        rel: PathBuf,
        hooks: Vec<BuildHookId>,
        /// The admitted files of a sysroot directory carry, each as its
        /// source and its destination relative to the tree root. `None`
        /// carries the whole source path, as a host scan does.
        members: Option<Vec<(PathBuf, PathBuf)>>,
    },
    /// Run a rule's declared hooks over a destination the tree already
    /// holds.
    RunStagedHooks { hooks: Vec<BuildHookId> },
    /// Write one pin's prepared resources from verified set bytes into
    /// the pin's own subtree, then run the owning rules' declared hooks
    /// over the subtree. Each entry is the subtree-relative destination,
    /// the verified bytes, and whether the file is an executable.
    PreparePin {
        set: String,
        pin: String,
        root: PathBuf,
        entries: Vec<(PathBuf, Vec<u8>, bool)>,
        hooks: Vec<BuildHookId>,
    },
    /// Retarget a plugin's alias links and drop the flat copy the
    /// subdirectory copy replaces.
    MigrateFlat { flat: PathBuf, new_rel: PathBuf },
}

/// What executing the planned actions produced, for the summary and
/// the generated record: every path written, relative to the tree root,
/// in the order the writes ran, so a walk that stops part way still
/// records what it left behind.
#[derive(Default)]
struct WalkOutcome {
    copied: Vec<(String, PathBuf, u64, String)>,
    written: Vec<String>,
}

impl WalkOutcome {
    fn wrote(&mut self, rel: &Path) {
        self.written.push(rel.to_string_lossy().into_owned());
    }
}

/// Execute the planned walk actions in planning order, each with its
/// report line, failing the assembly the moment a source no longer
/// matches what planning read from it. A dry run reports without
/// writing, as before. Every copy and link destination is relative to
/// the tree root: the library directory for the flat sonames and the
/// plugin trees, the declared location for a carried helper.
fn execute_walk(
    directory: &Path,
    dry_run: bool,
    strip: bool,
    lib_dest: &Path,
    actions: &[WalkAction],
    plan: &DestinationPlan,
    outcome: &mut WalkOutcome,
) -> io::Result<()> {
    let lib_dir = lib_dest
        .strip_prefix(directory)
        .unwrap_or(lib_dest)
        .to_path_buf();
    for action in actions {
        match action {
            WalkAction::CopyLib {
                soname,
                resolved,
                rel,
                size,
                requirer,
                stale_flat,
                libc_data,
            } => {
                // The bytes written are the bytes verified against the
                // planning stamp, read once, so a source changing after
                // the check cannot reach the tree.
                let bytes = if dry_run {
                    plan.verify_source(resolved)?;
                    Vec::new()
                } else {
                    plan.read_verified(resolved)?
                };
                let dest = directory.join(rel);
                eprintln!(
                    "  {} <- {} (needed by {}, {})",
                    color::bold_green(&rel.to_string_lossy()),
                    resolved.display(),
                    color::cyan(requirer),
                    color::dim(&format_size(*size))
                );
                if !dry_run {
                    if let Some(dir) = dest.parent() {
                        fs::create_dir_all(dir)?;
                    }
                    // Copying onto an existing alias symlink corrupts the
                    // alias target; drop the symlink first.
                    if dest.is_symlink() {
                        let _ = fs::remove_file(&dest);
                    }
                    ensure_writable(&dest);
                    fs::write(&dest, &bytes)?;
                    outcome.wrote(rel);
                    let _ = fs::set_permissions(
                        &dest,
                        std::os::unix::fs::PermissionsExt::from_mode(0o755),
                    );
                    normalize_mtime(&dest);
                    // The dynamic loader itself ships with baked-in absolute
                    // paths (ld.so.cache location, preload hook, fallback
                    // library dirs). On the packer's system those resolve to
                    // real files (e.g. /nix/store/.../glibc/etc/ld.so.cache);
                    // on someone else's system they're dead paths at best and
                    // wrong-content paths at worst. Scrub before shipping.
                    // musl's libc is its loader too, and whichever of its
                    // names is copied first is the file the other links to.
                    let is_loader = is_dynamic_loader(soname)
                        || libc_family_of_soname(soname) == Some(LibcFamily::Musl);
                    if is_loader
                        && let Err(e) = scrub_loader_paths(
                            &dest,
                            libc_family_of_soname(soname).unwrap_or(LibcFamily::Glibc),
                        )
                    {
                        eprintln!(
                            "  {} failed to scrub loader paths in {}: {e}",
                            color::bold_red("warning:"),
                            soname
                        );
                    }
                    if strip {
                        strip_debug(&dest);
                    }
                }
                if *stale_flat {
                    migrate_flat_plugin(&lib_dest.join(soname), rel, &lib_dir, lib_dest, dry_run);
                }
                outcome
                    .copied
                    .push((soname.clone(), rel.clone(), *size, requirer.clone()));
                if *libc_data && !dry_run {
                    match bundle_gconv(resolved, lib_dest, strip) {
                        Ok(files) if !files.is_empty() => {
                            eprintln!(
                                "  {} {} gconv module(s) beside libc",
                                color::bold_green("Copied"),
                                files.len()
                            );
                            for file in &files {
                                outcome.wrote(&lib_dir.join(file));
                            }
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!(
                            "  {} could not copy gconv modules: {e}",
                            color::bold_red("warning:")
                        ),
                    }
                    match bundle_c_utf8(resolved, lib_dest) {
                        Ok(files) if !files.is_empty() => {
                            eprintln!("  {} the C.UTF-8 locale", color::bold_green("Copied"));
                            for file in &files {
                                outcome.wrote(&lib_dir.join(file));
                            }
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!(
                            "  {} could not copy the C.UTF-8 locale: {e}",
                            color::bold_red("warning:")
                        ),
                    }
                }
            }
            WalkAction::LinkAlias {
                soname,
                resolved,
                rel,
                existing,
                requirer,
                size,
                stale_flat,
            } => {
                plan.verify_source(resolved)?;
                eprintln!(
                    "  {} {} -> {} (alias for {}, {})",
                    color::bold_green("Linked"),
                    soname,
                    existing.display(),
                    color::cyan(requirer),
                    color::dim(&format_size(*size))
                );
                if !dry_run {
                    let dest = directory.join(rel);
                    if let Some(dir) = dest.parent() {
                        fs::create_dir_all(dir)?;
                    }
                    if dest.exists() || dest.is_symlink() {
                        let _ = fs::remove_file(&dest);
                    }
                    let target = link_target(rel.parent().unwrap_or(Path::new("")), existing);
                    match std::os::unix::fs::symlink(&target, &dest) {
                        Ok(()) => outcome.wrote(rel),
                        Err(e) => eprintln!(
                            "  {} failed to symlink {} -> {}: {e}",
                            color::bold_red("warning:"),
                            soname,
                            target
                        ),
                    }
                }
                if *stale_flat {
                    migrate_flat_plugin(&lib_dest.join(soname), rel, &lib_dir, lib_dest, dry_run);
                }
            }
            WalkAction::CarryData {
                rule,
                src,
                rel,
                hooks,
                members,
            } => {
                if let Some(members) = members {
                    for (from, _) in members {
                        plan.verify_source(from)?;
                    }
                } else if src.is_dir() {
                    plan.verify_tree(src, rel)?;
                } else {
                    plan.verify_source(src)?;
                }
                let dest = directory.join(rel);
                if dry_run {
                    eprintln!(
                        "  {} {} (data of {})",
                        color::bold("Would copy"),
                        rel.display(),
                        color::cyan(rule)
                    );
                    continue;
                }
                let mut carried: Vec<PathBuf> = Vec::new();
                let copied = match members {
                    Some(members) => copy_members(members, directory, plan, &mut carried),
                    None => copy_data(src, &dest, plan, &mut carried).and_then(|bytes| {
                        // The copy itself must hold what planning hashed,
                        // so a source that changed while it was read is
                        // caught over the bytes the tree now holds.
                        if src.is_dir() {
                            plan.verify_tree(&dest, rel)?;
                        }
                        Ok(bytes)
                    }),
                };
                match copied {
                    Ok(bytes) => {
                        for path in &carried {
                            outcome.wrote(path.strip_prefix(directory).unwrap_or(path));
                        }
                        eprintln!(
                            "  {} {} <- {} (data of {}, {})",
                            color::bold_green("Copied"),
                            rel.display(),
                            src.display(),
                            color::cyan(rule),
                            color::dim(&format_size(bytes))
                        );
                        run_staged_hooks(hooks, directory, dry_run);
                    }
                    Err(e) => {
                        // A half-copied path reads as carried to the lint
                        // while missing half its files; take back what
                        // this carry wrote.
                        for path in carried.iter().rev() {
                            let _ = fs::remove_file(path);
                        }
                        if e.kind() == io::ErrorKind::InvalidData {
                            return Err(e);
                        }
                        eprintln!(
                            "  {} could not carry {}: {e}",
                            color::bold_red("warning:"),
                            rel.display()
                        );
                    }
                }
            }
            WalkAction::RunStagedHooks { hooks } => {
                run_staged_hooks(hooks, directory, dry_run);
            }
            WalkAction::PreparePin {
                set,
                pin,
                root,
                entries,
                hooks,
            } => {
                if dry_run {
                    eprintln!(
                        "  {} {} prepared file(s) for set {} (blake3 {})",
                        color::bold("Would write"),
                        entries.len(),
                        color::cyan(set),
                        short_pin(pin)
                    );
                    continue;
                }
                let base = directory.join(root);
                for (rel, bytes, executable) in entries {
                    let dest = base.join(rel);
                    if let Some(dir) = dest.parent() {
                        fs::create_dir_all(dir)?;
                    }
                    // Writing through an existing symlink would land in
                    // whatever it names; drop it first, the way the
                    // library copy does.
                    if dest.is_symlink() {
                        let _ = fs::remove_file(&dest);
                    }
                    ensure_writable(&dest);
                    fs::write(&dest, bytes)?;
                    outcome.wrote(&root.join(rel));
                    let mode = if *executable { 0o755 } else { 0o644 };
                    let _ = fs::set_permissions(
                        &dest,
                        std::os::unix::fs::PermissionsExt::from_mode(mode),
                    );
                    normalize_mtime(&dest);
                }
                // The declared transformations run over the staged
                // subtree through the registered hooks, before the
                // prepared bytes are hashed for the ledger.
                run_staged_hooks(hooks, &base, dry_run);
                eprintln!(
                    "  {} {} prepared file(s) for set {} (blake3 {})",
                    color::bold_green("Prepared"),
                    entries.len(),
                    color::cyan(set),
                    short_pin(pin)
                );
            }
            WalkAction::MigrateFlat { flat, new_rel } => {
                migrate_flat_plugin(flat, new_rel, &lib_dir, lib_dest, dry_run);
            }
        }
    }
    Ok(())
}

/// Anchor the carried snippet directory in a carried `share/alsa/alsa.conf`.
/// Nothing happens on a dry run; a config that cannot be patched is a
/// warning, since the host's snippets then keep deciding the default.
fn patch_carried_alsa_conf(directory: &Path, dry_run: bool) {
    if dry_run {
        return;
    }
    match patch_alsa_conf_hooks(&directory.join("share/alsa/alsa.conf")) {
        Ok(true) => eprintln!(
            "  {} share/alsa/alsa.conf hooks (carried snippets load after the host's)",
            color::bold_green("Patched")
        ),
        Ok(false) => {}
        Err(e) => eprintln!(
            "  {} could not patch share/alsa/alsa.conf hooks: {e}",
            color::bold_red("warning:")
        ),
    }
}

/// A registered staged-resource procedure: the tree root it works on,
/// and whether the run reports without writing.
type StagedHook = fn(&Path, bool);

/// The registered staged-resource procedure of one build-hook identity.
///
/// Every staged mutation a rule requests dispatches through this
/// lookup, so the walk holds no branch keyed to one rule's identity,
/// and a descriptor reaches a procedure only by declaring the identity
/// in its build hooks. The match is exhaustive over the closed
/// registry, so adding a variant without registering its procedure
/// fails to compile.
fn staged_hook(id: BuildHookId) -> StagedHook {
    match id {
        BuildHookId::AlsaConfPatch => patch_carried_alsa_conf,
    }
}

/// Run a rule's declared build hooks through the registry, in
/// declaration order. The procedures themselves honor a dry run.
fn run_staged_hooks(hooks: &[BuildHookId], directory: &Path, dry_run: bool) {
    for id in hooks {
        staged_hook(*id)(directory, dry_run);
    }
}

/// A plugin a run before plugin directories existed left flat in `lib/`
/// is dead weight there: nothing looks for it. With the subdirectory
/// copy just written, retarget the tree's own alias links that point at
/// the flat copy and drop it, so a re-bundle repairs the tree instead of
/// carrying both. The new destination and every link are taken relative
/// to the tree root.
fn migrate_flat_plugin(
    flat: &Path,
    new_rel: &Path,
    lib_dir: &Path,
    lib_dest: &Path,
    dry_run: bool,
) {
    let links = alias_links_to(lib_dest, flat);
    if dry_run {
        eprintln!(
            "  {} {} (a flat copy in lib/ would move there)",
            color::bold("Would migrate"),
            new_rel.display()
        );
        return;
    }
    for link in links {
        let from = link
            .strip_prefix(lib_dest)
            .unwrap_or(&link)
            .parent()
            .map(|p| lib_dir.join(p))
            .unwrap_or_else(|| lib_dir.to_path_buf());
        let target = link_target(&from, new_rel);
        if let Err(e) = fs::remove_file(&link) {
            eprintln!(
                "  {} could not retarget {}: {e}",
                color::bold_red("warning:"),
                link.display()
            );
            continue;
        }
        if let Err(e) = std::os::unix::fs::symlink(&target, &link) {
            eprintln!(
                "  {} failed to retarget {} -> {}: {e}",
                color::bold_red("warning:"),
                link.display(),
                target
            );
            continue;
        }
    }
    match fs::remove_file(flat) {
        Ok(()) => eprintln!(
            "  {} {} (moved out of lib/)",
            color::bold_green("Moved"),
            new_rel.display()
        ),
        Err(e) => eprintln!(
            "  {} could not remove the flat copy {}: {e}",
            color::bold_red("warning:"),
            flat.display()
        ),
    }
}

/// The symlinks under `root` whose targets resolve to `target`, relative
/// to their own directory. Links out of the tree are not ours to move.
fn alias_links_to(root: &Path, target: &Path) -> Vec<PathBuf> {
    fn normalize(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for comp in path.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                comp => out.push(comp.as_os_str()),
            }
        }
        out
    }
    let wanted = normalize(target);
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = dir.join(entry.file_name());
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_symlink()
                && let Ok(link) = fs::read_link(&path)
            {
                let resolved = normalize(&dir.join(link));
                if resolved == wanted {
                    found.push(path);
                }
            }
        }
    }
    found
}

/// One bundling invocation: the tree, what is scanned, where the
/// contents come from, and how strictly the outcome is audited.
#[derive(Clone, Default)]
pub struct BundleOptions {
    pub directory: PathBuf,
    /// Restricts *which* binaries are scanned for dependencies.
    pub target: Option<PathBuf>,
    /// Decides *what* is bundled: the architecture and libc family every
    /// candidate is filtered against.
    ///
    /// Distinct from `target`, which only narrows the scan. Without this the
    /// family came from whichever path sorted first, so one musl helper in a
    /// glibc tree silently dropped the entrypoint's own loader.
    pub primary: Option<PathBuf>,
    pub lib_dir: PathBuf,
    /// The library directory list the manifest will carry, raw: an `auto`
    /// entry expands on the assembled tree when the redirection decision
    /// resolves it, through the packer's own resolution. The low-level
    /// `bundle-libs` command passes `auto`, since the pack command that
    /// follows names its own list.
    pub lib_dirs: Vec<String>,
    /// The recipe's own `[paths]` pairs: their count reserves the
    /// paths record's room before the inferred pairs' cap, and their
    /// absolute paths are kept out of the inference, so a spelling the
    /// recipe already answers for cannot spend a bound slot. The
    /// low-level `bundle-libs` command carries no recipe and passes
    /// none.
    pub recipe_pairs: Vec<PathPair>,
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
    /// Run the target and bundle every host library it loaded at runtime,
    /// the way `dlopen` by computed name hides from `DT_NEEDED`.
    pub trace_run: bool,
    /// How long the traced run may go before it is stopped.
    pub trace_seconds: u64,
    /// Arguments for the traced run.
    pub trace_args: Vec<String>,
    /// Keep only what a trace observed in the directories inference
    /// collects: a data directory keeps the files the trace opened, a
    /// plugin directory the members it loaded, plus keep patterns and
    /// the members' own dependencies. Without a trace nothing is
    /// trimmed.
    pub trim_inferred: bool,
    /// Additional sonames added to the dlopen scan allow-list.
    pub dlopen_extra: Vec<String>,
    /// Take the bundle's contents from a pinned sysroot's package database
    /// instead of scanning this machine. The host is never consulted, the
    /// framework bundlers stay off since the closure already holds what
    /// the packages declare, and an unresolved soname is an error.
    pub sysroot: Option<SysrootOptions>,
    /// Sets pinned without a sysroot package database: the AppDir is the
    /// author's own tree, taken as it stands, and the sets are recorded
    /// for the runtime to fetch at launch. Host-scan rules apply, and the
    /// verifier reports what the sets supply instead of warning about it.
    pub sets: Vec<sysroot::SharedSet>,
    /// The build's resolved entrypoints with their own set selections,
    /// which the assembly audits each against the assembled tree: a set
    /// an entrypoint does not select satisfies nothing of its. Empty
    /// when no selection table was resolved, as in a bare `bundle-libs`
    /// invocation, which audits only the declared sets.
    pub entrypoint_selections: Vec<AuditedEntrypoint>,
    /// A GL build pinned without a sysroot, named whole by the recipe.
    pub platform_pin: Option<onelf_sysroot::platform::Pin>,
    /// The rule catalog discovery activates: the built-ins alone by
    /// default, or the built-ins with a recipe's local rules and disables.
    pub rules: crate::rules::RuleCatalog,
    /// Fail on an unresolved required dependency whatever the source,
    /// the contract of high-level `onelf build` and `onelf run --bundle`.
    /// A sysroot build is always strict. Without this a host scan keeps
    /// the low-level `bundle-libs` behavior: unresolved needs are
    /// reported as warnings and nothing is omitted.
    pub strict: bool,
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

/// Attest the bytes the staging flow left at the paths the generated
/// ledger records, once its own ELF rewriting (RUNPATH rewriting,
/// loader scrubbing, bootstrap injection) has run over them, so every
/// entry attests the final bytes and a later rebuild stops on a
/// publisher's edit instead of excusing the rewriting as the build's
/// own doing. A tree without a ledger gets none written.
fn attest_staged_outputs(opts: &BundleOptions) -> io::Result<()> {
    if opts.directory.join(sysroot::GENERATED_FILE).is_file() {
        sysroot::attest_generated(&opts.directory)?;
    }
    Ok(())
}

/// Report the rules the closure activated, in catalog order, each with
/// the soname that activated it, and how many resource groups activation
/// queued. Bounded like every summary, so no catalog can flood output.
fn report_activations(activations: &[RuleActivation]) {
    if activations.is_empty() {
        return;
    }
    const SHOWN_RULES: usize = 8;
    let shown: Vec<String> = activations
        .iter()
        .take(SHOWN_RULES)
        .map(|a| format!("{} ({})", a.rule, a.reason))
        .collect();
    let hidden = activations.len().saturating_sub(SHOWN_RULES);
    let more = if hidden > 0 {
        format!(" and {hidden} more")
    } else {
        String::new()
    };
    let groups: usize = activations.iter().map(|a| a.groups.len()).sum();
    eprintln!(
        "  {} {} activated by soname prefix: {}{}; {} group(s) queued",
        color::bold("Rules:"),
        activations.len(),
        shown.join(", "),
        more,
        groups
    );
}

/// Mirror the groups the activator has queued, appending the rules that
/// activated since the last call with their groups in declaration order:
/// the same sequence the queue was filled in, so a popped group item is
/// identified by taking the front of this.
fn mirror_new_groups(
    discovery: &Discovery,
    known: &mut HashSet<String>,
    mirror: &mut VecDeque<(String, String)>,
) {
    for activation in discovery.activations() {
        if !known.insert(activation.rule.clone()) {
            continue;
        }
        for group in activation.groups {
            mirror.push_back((activation.rule.clone(), group));
        }
    }
}

/// Report one plugin group's collection, placed at `destination`: the
/// group's finding, which names its source directories or why nothing
/// was usable, and as progress what was collected and every member
/// refused with its reason, bounded so a hostile directory cannot flood
/// the build log.
fn report_collection(collection: &collect::GroupCollection, destination: &str) {
    let label = format!("{}/{}", collection.rule, collection.group);
    if collection.plugins.is_empty() {
        let mut reason = format!(
            "group {}: no usable plugin group ({} location(s) unavailable, {} candidate(s) \
             skipped)",
            collection.group,
            collection.misses.len(),
            collection.skipped.len()
        );
        for miss in collection.misses.iter().take(3) {
            reason.push_str("; ");
            reason.push_str(miss);
        }
        completeness::record(
            Finding::new(destination, Outcome::OptionalOmission, reason).of_rule(&collection.rule),
        );
    } else {
        let dirs: Vec<String> = collection
            .search_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        completeness::record(
            Finding::new(
                destination,
                Outcome::Selected,
                format!(
                    "group {}: {} plugin(s) from {}",
                    collection.group,
                    collection.plugins.len(),
                    dirs.join(", ")
                ),
            )
            .of_rule(&collection.rule),
        );
        eprintln!(
            "  {} {} plugin(s) for {} from {} director{}",
            color::bold_green("Collected"),
            collection.plugins.len(),
            label,
            collection.search_dirs.len(),
            if collection.search_dirs.len() == 1 {
                "y"
            } else {
                "ies"
            },
        );
        for reason in collection.misses.iter().take(3) {
            eprintln!("    {reason}");
        }
    }
    const SHOWN_SKIPS: usize = 4;
    for (name, reason) in collection.skipped.iter().take(SHOWN_SKIPS) {
        eprintln!("    {} skipped: {reason}", color::dim(name));
    }
    let hidden = collection.skipped.len().saturating_sub(SHOWN_SKIPS);
    if hidden > 0 {
        eprintln!("    and {hidden} more skipped");
    }
}

/// How the selected source answers for one needed object. Every
/// resolution outcome the dependency walk reaches folds into one of
/// these classes, and every report names the class with the same
/// wording, so a missing optional dependency is never confused with an
/// unselected provider or with a deliberate removal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceVerdict {
    /// An admitted source resolved it and the walk carries it.
    Carried,
    /// A selected package holds it, but the policy or a trace removed
    /// it. A removal the build asked for is a conflict, never an
    /// omission and never a file to restore.
    Pruned {
        /// The selected package that owns the removed file.
        owner: String,
    },
    /// Only a package the build handed to the host or to a shared set
    /// whole holds it.
    HostProvider {
        /// The package handed whole to the host.
        owner: String,
    },
    /// Only a package the recipe never selected holds it. The build
    /// never selects a package on its own.
    UnselectedProvider {
        /// The unselected optional package that owns the file.
        owner: String,
    },
    /// Nothing in the selected source provides it.
    Absent,
}

impl SourceVerdict {
    /// Whether the verdict leaves the selected source without a copy
    /// the loader could reach, so an auto-selected consumer of the
    /// object may be omitted for it. A deliberate removal is not one
    /// of these: it stops the build instead.
    fn omittable(&self) -> bool {
        matches!(
            self,
            SourceVerdict::Absent
                | SourceVerdict::UnselectedProvider { .. }
                | SourceVerdict::HostProvider { .. }
        )
    }

    /// The reason phrase the reports append, naming the rule and the
    /// library or package involved.
    fn reason(&self) -> String {
        match self {
            SourceVerdict::Carried => {
                "the selected source holds it off every loader path".to_string()
            }
            SourceVerdict::Pruned { owner } => {
                format!("{owner} holds it, but the policy or a trace removed it")
            }
            SourceVerdict::HostProvider { owner } => format!("left to the host package {owner}"),
            SourceVerdict::UnselectedProvider { owner } => {
                format!("provided only by the unselected optional package {owner}")
            }
            SourceVerdict::Absent => "no selected source provides it".to_string(),
        }
    }
}

/// Classifies needed sonames against the selected source, so the
/// dependency walk, the drop pass and the final error all name the same
/// outcome for the same object.
struct SourceClassifier<'a> {
    /// The files named exactly like some soname, with the package that
    /// owns each, in the database's own order.
    providers: HashMap<String, Vec<(String, String)>>,
    /// The populated universe, which records what the build admitted
    /// and what it left out. `None` without a sysroot, where every
    /// unmet need is simply absent from the host.
    universe: Option<&'a sysroot::SysrootUniverse>,
}

impl<'a> SourceClassifier<'a> {
    /// The classifier over a database already read, against the
    /// universe the populated build recorded.
    fn from_db(db: Database, universe: &'a sysroot::SysrootUniverse) -> Self {
        let mut providers: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for package in db.packages() {
            for file in &package.files {
                if file.ends_with('/') {
                    continue;
                }
                if let Some(name) = Path::new(file).file_name().and_then(|n| n.to_str()) {
                    providers
                        .entry(name.to_string())
                        .or_default()
                        .push((package.name.clone(), file.clone()));
                }
            }
        }
        Self {
            providers,
            universe: Some(universe),
        }
    }

    /// The classifier for a sysroot build: the database at `root`,
    /// indexed once, against the universe the populated build recorded.
    fn for_sysroot(root: &Path, universe: &'a sysroot::SysrootUniverse) -> io::Result<Self> {
        Ok(Self::from_db(Database::read(root)?, universe))
    }

    /// The classifier for a host-scan build, which has no selected
    /// source to consult.
    fn host_only() -> Self {
        Self {
            providers: HashMap::new(),
            universe: None,
        }
    }

    /// The verdict for `soname`. A deliberate removal outranks every
    /// other answer, since a policy conflict stops the build where an
    /// omission only records one, and a copy the selected source holds
    /// outranks a provider it never admitted.
    fn classify(&self, soname: &str) -> SourceVerdict {
        let Some(universe) = self.universe else {
            return SourceVerdict::Absent;
        };
        let mut verdict = SourceVerdict::Absent;
        for (_owner, file) in self.providers.get(soname).into_iter().flatten() {
            let candidate = if universe.admits(file) {
                SourceVerdict::Carried
            } else {
                match universe.refusal(file) {
                    sysroot::Refusal::Pruned { owner } => SourceVerdict::Pruned { owner },
                    sysroot::Refusal::UnselectedPackage { owner } => {
                        SourceVerdict::UnselectedProvider { owner }
                    }
                    sysroot::Refusal::HostPackage { owner } => {
                        SourceVerdict::HostProvider { owner }
                    }
                    sysroot::Refusal::Absent => SourceVerdict::Absent,
                }
            };
            if matches!(candidate, SourceVerdict::Pruned { .. }) {
                return candidate;
            }
            verdict = rank_verdicts(verdict, candidate);
        }
        verdict
    }
}

/// The stronger of two verdicts over providers of one soname, in the
/// fixed order reports rely on: carried, then unselected, then host,
/// then absent.
fn rank_verdicts(a: SourceVerdict, b: SourceVerdict) -> SourceVerdict {
    fn rank(verdict: &SourceVerdict) -> u8 {
        match verdict {
            SourceVerdict::Carried => 3,
            SourceVerdict::UnselectedProvider { .. } => 2,
            SourceVerdict::HostProvider { .. } => 1,
            SourceVerdict::Pruned { .. } | SourceVerdict::Absent => 0,
        }
    }
    if rank(&a) >= rank(&b) { a } else { b }
}

/// The tree-relative destinations explicitly included sonames occupy:
/// the library directory's flat copy, since an include is never a
/// collected group member.
fn explicit_include_paths(opts: &BundleOptions) -> HashSet<PathBuf> {
    opts.include
        .iter()
        .map(|soname| opts.lib_dir.join(soname))
        .collect()
}

#[cfg(test)]
mod source_verdict_tests {
    use super::*;
    use sysroot::fixture::{package, populate_universe};

    /// A test's own scratch root, unique per call site.
    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("onelf-verdict-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// An in-memory database over `packages` and the universe with
    /// `owner` selected and `optional` added, pruned by the patterns in
    /// `policy_lines`.
    fn classified(
        root: &Path,
        packages: &[onelf_sysroot::Package],
        owner: &str,
        policy_lines: &[&str],
    ) -> (Database, sysroot::SysrootUniverse) {
        let mut db = Database::default();
        for package in packages {
            db.insert(package.clone());
        }
        let policy = if policy_lines.is_empty() {
            None
        } else {
            let path = root.join("policy.txt");
            fs::write(&path, policy_lines.join("\n")).unwrap();
            Some(Policy::load(&path).unwrap())
        };
        let u = populate_universe(root, packages, owner, &[], policy.as_ref(), None);
        (db, u)
    }

    /// A package with `depends` and `files`, relative to the root.
    fn pkg(name: &str, depends: &[&str], files: &[&str]) -> onelf_sysroot::Package {
        onelf_sysroot::Package {
            name: name.to_string(),
            version: "1.0-1".to_string(),
            depends: depends.iter().map(|s| s.to_string()).collect(),
            files: files.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn an_absent_soname_classifies_absent() {
        let root = scratch("absent");
        let packages = [package("app", &["usr/bin/app"])];
        let (db, u) = classified(&root, &packages, "app", &[]);
        let classifier = SourceClassifier::from_db(db, &u);
        assert_eq!(
            classifier.classify("libnothing.so.9"),
            SourceVerdict::Absent,
            "no package owns a file of that name"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unselected_provider_names_its_package() {
        let root = scratch("unselected");
        let packages = [
            package("app", &["usr/bin/app"]),
            package("extra", &["usr/lib/extra/libextra.so.1"]),
        ];
        let (db, u) = classified(&root, &packages, "app", &[]);
        let classifier = SourceClassifier::from_db(db, &u);
        assert_eq!(
            classifier.classify("libextra.so.1"),
            SourceVerdict::UnselectedProvider {
                owner: "extra".to_string()
            },
            "the provider is real but the recipe never selected it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pruned_provider_names_the_policy_conflict() {
        let root = scratch("pruned");
        let packages = [
            pkg("app", &["core"], &["usr/bin/app"]),
            pkg("core", &[], &["usr/lib/libpinned.so.1"]),
        ];
        let (db, u) = classified(&root, &packages, "app", &["usr/lib/libpinned*"]);
        let classifier = SourceClassifier::from_db(db, &u);
        assert_eq!(
            classifier.classify("libpinned.so.1"),
            SourceVerdict::Pruned {
                owner: "core".to_string()
            },
            "a selected package held it and the policy removed it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_kept_soname_classifies_carried() {
        let root = scratch("kept");
        let packages = [
            pkg("app", &["core"], &["usr/bin/app"]),
            pkg("core", &[], &["usr/lib/libcore.so.1"]),
        ];
        let (db, u) = classified(&root, &packages, "app", &[]);
        let classifier = SourceClassifier::from_db(db, &u);
        assert_eq!(
            classifier.classify("libcore.so.1"),
            SourceVerdict::Carried,
            "the build copied it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_host_provider_names_its_package() {
        let root = scratch("host");
        let mut db = Database::default();
        db.insert(package("app", &["usr/bin/app"]));
        db.insert(package("mesa", &["usr/lib/libGL.so.1"]));
        let mut left_out = HashSet::new();
        left_out.insert("usr/lib/libGL.so.1".to_string());
        let mut for_universe = Database::default();
        for_universe.insert(package("app", &["usr/bin/app"]));
        for_universe.insert(package("mesa", &["usr/lib/libGL.so.1"]));
        let u = sysroot::SysrootUniverse::admit(
            &root,
            for_universe,
            &["usr/bin/app".to_string()],
            &left_out,
            &["app".to_string()],
        );
        let classifier = SourceClassifier::from_db(db, &u);
        assert_eq!(
            classifier.classify("libGL.so.1"),
            SourceVerdict::HostProvider {
                owner: "mesa".to_string()
            },
            "the package was handed to the host whole"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_trace_pruned_provider_names_the_conflict_too() {
        let root = scratch("trace-pruned");
        let packages = [
            pkg("app", &["core"], &["usr/bin/app"]),
            pkg("core", &[], &["usr/lib/libpinned.so.1"]),
        ];
        let mut db = Database::default();
        for package in &packages {
            db.insert(package.clone());
        }
        let trace = onelf_sysroot::Trace::from_paths(vec!["usr/bin/app".to_string()]);
        let u = populate_universe(&root, &packages, "app", &[], None, Some(&trace));
        let classifier = SourceClassifier::from_db(db, &u);
        assert_eq!(
            classifier.classify("libpinned.so.1"),
            SourceVerdict::Pruned {
                owner: "core".to_string()
            },
            "a trace removal is the conflict class, like a policy removal"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pruned_provider_outranks_an_unselected_one() {
        let root = scratch("outranks");
        let packages = [
            pkg("app", &["core"], &["usr/bin/app"]),
            pkg("core", &[], &["usr/lib/libdual.so.1"]),
            pkg("extra", &[], &["opt/extra/lib/libdual.so.1"]),
        ];
        let (db, u) = classified(&root, &packages, "app", &["usr/lib/libdual*"]);
        let classifier = SourceClassifier::from_db(db, &u);
        assert_eq!(
            classifier.classify("libdual.so.1"),
            SourceVerdict::Pruned {
                owner: "core".to_string()
            },
            "the removal is the conflict class, ahead of the unselected provider"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_host_scan_classifies_everything_absent() {
        let classifier = SourceClassifier::host_only();
        assert_eq!(
            classifier.classify("libanything.so.1"),
            SourceVerdict::Absent,
            "without a selected source there is nothing to consult"
        );
    }
}

#[cfg(test)]
mod carry_tests {
    use super::*;
    use sysroot::fixture::{package, populate_universe};

    /// A sysroot member the tree already holds with different content is
    /// refused while its siblings go through, so a carry is planned
    /// partly: the finding and the one admitted member. Collecting rolls
    /// that back whole, so the refused member must be the only thing the
    /// plan objects to and the sibling the only member it hands out.
    #[test]
    fn a_refused_sysroot_member_leaves_one_sibling_and_a_finding() {
        let root = std::env::temp_dir().join(format!("onelf-carry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sysroot = root.join("sysroot");
        let appdir = root.join("app");
        fs::create_dir_all(sysroot.join("usr/share/examplefw")).unwrap();
        fs::create_dir_all(appdir.join("share/examplefw")).unwrap();
        fs::write(sysroot.join("usr/share/examplefw/a.conf"), "conf\n").unwrap();
        fs::write(sysroot.join("usr/share/examplefw/b.conf"), "more\n").unwrap();
        fs::write(appdir.join("share/examplefw/a.conf"), "authored\n").unwrap();
        let packages = [package(
            "examplefw",
            &[
                "usr/lib/libexamplefw.so.1",
                "usr/share/examplefw/a.conf",
                "usr/share/examplefw/b.conf",
            ],
        )];
        let universe = populate_universe(&sysroot, &packages, "examplefw", &[], None, None);
        let hit = CandidateHit {
            candidate: 0,
            base: SourceBase::InstallationPrefix,
            kind: ResourceKind::Directory,
            source: sysroot.join("usr/share/examplefw"),
            resolved: sysroot.join("usr/share/examplefw"),
            root: sysroot.clone(),
            destination: PathBuf::from("share/examplefw"),
        };
        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let members = admitted_members(
            &universe,
            &hit,
            &mut plan,
            "inferred from libexamplefw.so.1",
        )
        .unwrap();
        assert_eq!(
            members,
            vec![(
                sysroot.join("usr/share/examplefw/b.conf"),
                PathBuf::from("share/examplefw/b.conf"),
            )],
            "the refused member is not handed out, its sibling is"
        );
        assert!(plan.has_findings(), "the refusal leaves a finding");
        let _ = fs::remove_dir_all(&root);
    }
}

/// Bundle the shared libraries the tree's ELF files need into its
/// library directory, rewrite the ELF files for the bundled layout,
/// and record what the build generated.
///
/// Every destination the dependency walk intends to write is resolved
/// into a plan first, so conflicting resources, overlapping file and
/// directory destinations, destinations that would leave the tree, and
/// destinations already holding different bytes than the resource
/// requires all stop the build before anything is written. Sources are
/// stamped at planning time and re-checked before each copy, so a
/// source that changes while the bundle is written fails the assembly
/// instead of mixing bytes. Bundling writes only into the AppDir it is
/// given: the output package is written later, by packing, and only
/// after assembly succeeds, so a failed build never replaces an
/// existing output.
///
/// The completeness findings the run reached are printed as one sorted
/// block when it ends, whether it succeeded or not, and kept for
/// [`take_findings`]. A dry run writes nothing into the AppDir, fetches
/// nothing and runs nothing: a set build not already verified on this
/// machine is reported as unverified, a traced run is reported as not
/// exercised, and a sysroot build populates a private scratch copy of
/// the AppDir, which is removed afterwards, since its scan needs the
/// populated tree.
pub fn bundle_libs(opts: &BundleOptions) -> io::Result<()> {
    publish_activations(Vec::new());
    publish_pairs(Vec::new());
    let _no_fetch = opts.dry_run.then(sysroot::forbid_fetch);
    let result = if opts.dry_run && opts.sysroot.is_some() && opts.directory.is_dir() {
        ScratchTree::copy_of(&opts.directory).and_then(|scratch| {
            bundle_tree(&BundleOptions {
                directory: scratch.path().to_path_buf(),
                ..opts.clone()
            })
        })
    } else {
        bundle_tree(opts)
    };
    completeness::report(&activate::published_activations());
    result
}

/// A private copy of a tree, removed when dropped.
pub(crate) struct ScratchTree(PathBuf);

impl ScratchTree {
    /// Copy `root` into a fresh private directory: directories made,
    /// regular files cloned as their own inodes, symlinks remade as
    /// they are.
    pub(crate) fn copy_of(root: &Path) -> io::Result<Self> {
        let scratch = ScratchTree(private_temp_dir("dry-run")?);
        for entry in jwalk::WalkDir::new(root).skip_hidden(false).sort(true) {
            let entry = entry.map_err(|e| io::Error::other(e.to_string()))?;
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let dest = scratch.0.join(rel);
            let kind = entry.file_type();
            if kind.is_dir() {
                fs::create_dir_all(&dest)?;
            } else if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&path)?, &dest)?;
            } else if kind.is_file() {
                clone_file(&path, &dest)?;
            }
        }
        Ok(scratch)
    }

    /// Where the copy lives.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Give `dest` the bytes and permission bits of the regular file at
/// `source` as a new inode: a reflink where the filesystem shares
/// extents, a plain copy otherwise. Never a hardlink, so an edit to one
/// side cannot reach the other through a shared inode.
pub(crate) fn clone_file(source: &Path, dest: &Path) -> io::Result<()> {
    let from = fs::File::open(source)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    if rustix::fs::ioctl_ficlone(&to, &from).is_err() {
        io::copy(&mut &from, &mut to)?;
    }
    to.set_permissions(from.metadata()?.permissions())
}

/// The bundling run [`bundle_libs`] reports on.
fn bundle_tree(opts: &BundleOptions) -> io::Result<()> {
    publish_prepared(Vec::new());
    if !opts.directory.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{}: not a directory", opts.directory.display()),
        ));
    }

    // The provider audits run on the verified exports of the declared
    // sets, read once here and shared by both: the all-selected
    // collision audit stops contradictory declarations before anything
    // is written, and the per-entrypoint audit below refuses a tree an
    // entrypoint cannot launch from. Both fetch nothing the declared
    // sets' own reporting does not, and a dry run consults no build at
    // all, so it fetches nothing either. A database-backed sysroot
    // declares its sets in its own section, and they are audited the
    // same way.
    let declared_sets: Vec<sysroot::SharedSet> = opts
        .sets
        .iter()
        .chain(opts.sysroot.iter().flat_map(|sr| sr.sets.iter()))
        .cloned()
        .collect();
    let set_audit = if !declared_sets.is_empty() {
        let (providers, unchecked) = sysroot::audit_providers(&declared_sets);
        for provider in &providers {
            completeness::record(Finding::new(
                format!("set {}", provider.set),
                Outcome::Selected,
                format!(
                    "verified pinned build {}, providing {} soname(s)",
                    short_pin(&provider.pin),
                    provider.provides.len()
                ),
            ));
        }
        for (set, why) in &unchecked {
            completeness::record(Finding::new(
                format!("set {set}"),
                Outcome::Unverified,
                format!("{why}; it satisfies no dependency here"),
            ));
        }
        audit_declared_sets(opts, &providers)?;
        Some((providers, unchecked))
    } else {
        None
    };

    let mut host_files: HashSet<String> = HashSet::new();
    let mut set_sonames: HashSet<String> = HashSet::new();
    let mut sysroot_universe: Option<sysroot::SysrootUniverse> = None;
    let sysroot_platform = match &opts.sysroot {
        Some(sr) => {
            let (report, platform) = sysroot::populate(&opts.directory, sr)?;
            sysroot::print_report(sr, &report);
            host_files = report.host_files;
            set_sonames = report.set_sonames;
            sysroot_universe = Some(report.universe);
            Some(platform)
        }
        None => {
            // A record from an earlier sysroot build would describe a
            // bundle this run is about to change. A dry run changes
            // nothing, so the record still holds. The records a
            // sysroot-less recipe pins are (re)written here.
            if !opts.dry_run {
                let _ = fs::remove_file(opts.directory.join(sysroot::PROVENANCE_FILE));
                sysroot::record_sets(&opts.directory, &opts.sets, opts.platform_pin.as_ref())?;
            }
            None
        }
    };
    let frameworks = opts.sysroot.is_none();

    // The classifier every outcome report and every drop decision asks:
    // it answers, for a soname the bundle does not carry, whether the
    // selected source is without it, holds it behind an unselected
    // optional package, handed it to the host, or had it removed by the
    // policy or a trace.
    let classifier = match (&opts.sysroot, sysroot_universe.as_ref()) {
        (Some(sr), Some(universe)) => SourceClassifier::for_sysroot(&sr.root, universe)?,
        _ => SourceClassifier::host_only(),
    };

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
    // The primary binary decides, when the caller named one. Otherwise fall
    // back to the first ELF in the sorted walk, which is what a bare
    // `bundle-libs` on an unlabelled directory has always done.
    let primary = resolve_primary(opts);
    let target_class = primary
        .as_deref()
        .and_then(read_elf_class)
        .or_else(|| target_elfs.iter().find_map(|f| read_elf_class(f)));
    let target_machine = primary
        .as_deref()
        .and_then(read_elf_machine)
        .or_else(|| target_elfs.iter().find_map(|f| read_elf_machine(f)));

    let detected = detect_frameworks(&target_elfs);
    // The GPU stack is never bundled on detection alone: the driver that
    // works is the host's, and the launch resolver takes it from there.
    // Bundling Mesa is the publisher's explicit choice, for packages that
    // must run where the host has no GL at all. The client-side frameworks
    // (Wayland, GTK data) are bundled on detection as before.
    let want_gl = frameworks && opts.gl && !opts.no_gl;
    let want_dri = frameworks && opts.dri && !opts.no_dri;
    let want_vulkan = frameworks && opts.vulkan && !opts.no_vulkan;
    let want_wayland = frameworks && (opts.wayland || detected.wayland) && !opts.no_wayland;
    let want_gtk = frameworks && (opts.gtk || detected.gtk) && !opts.no_gtk;

    // Report frameworks detection turned on that the user did not request,
    // and frameworks the user explicitly suppressed, so the outcome is visible.
    let auto: Vec<&str> = [
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
    let driver_stack: Vec<&str> = [
        (detected.gl && !want_gl, "gl"),
        (detected.dri && !want_dri, "dri"),
        (detected.vulkan && !want_vulkan, "vulkan"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    if frameworks && !driver_stack.is_empty() {
        eprintln!(
            "  {} {} detected; the host's drivers are used at launch. \
             Pass --gl, --dri or --vulkan to bundle Mesa instead.",
            color::bold("Frameworks:"),
            driver_stack.join(", ")
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
        .chain(
            sysroot_platform
                .iter()
                .flat_map(|p| p.prefixes().iter().map(String::as_str)),
        )
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

    // The destinations the assembly intends to write, resolved against
    // each other and against the tree before any of them is written.
    // The framework copiers without replacement authority plan into it
    // first, and the dependency walk below plans the rest.
    let mut plan = DestinationPlan::load(&opts.directory)?;
    // The host file each framework-copied library came from, which a
    // host scan's descriptor discovery searches after the tree copy.
    let mut copied_from: HashMap<String, PathBuf> = HashMap::new();
    if want_wayland {
        let (written, sources) = bundle_wayland(
            &opts.directory,
            &opts.lib_dir,
            &opts.search_path,
            &excludes,
            target_class,
            target_machine,
            opts.dry_run,
            opts.strip,
            &mut plan,
        )?;
        if !opts.dry_run {
            sysroot::record_generated(&opts.directory, written)?;
        }
        copied_from.extend(sources);
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
        if let Some(interp) = parse_interp(path)
            && let Some(name) = Path::new(&interp).file_name().and_then(|n| n.to_str())
        {
            needed_by
                .entry(name.to_string())
                .or_insert_with(|| format!("{requirer} (PT_INTERP)"));
        }
        // Collect RPATH/RUNPATH directories from input binaries. An
        // absolute one names the sysroot's filesystem in sysroot mode, a
        // `$ORIGIN` one the tree itself.
        let sysroot_root = opts.sysroot.as_ref().map(|sr| sr.root.as_path());
        for dir in parse_rpaths_under(path, sysroot_root)
            .into_iter()
            .chain(parse_needed_dirs_under(path, sysroot_root))
        {
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

    let mut traced_opened: Vec<PathBuf> = Vec::new();
    // What the target loads when it runs: dlopen by computed name leaves
    // nothing in DT_NEEDED for the scan above to find. The run is against
    // this machine, so it has no meaning for a sysroot build, whose trace
    // tier covers that ground.
    if opts.trace_run && opts.dry_run {
        completeness::record(Finding::new(
            opts.target
                .as_deref()
                .map(|t| t.display().to_string())
                .unwrap_or_default(),
            Outcome::Unchecked,
            "a dry run does not run the target, so the libraries a traced run \
             would load are not listed",
        ));
        if opts.trim_inferred && opts.sysroot.is_none() {
            completeness::record(Finding::new(
                opts.target
                    .as_deref()
                    .map(|t| t.display().to_string())
                    .unwrap_or_default(),
                Outcome::Unchecked,
                "a dry run does not run the target, so trim-inferred trims \
                 nothing and the inferred directories stay whole",
            ));
        }
    } else if opts.trace_run {
        if opts.sysroot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--trace-run runs the target against this machine, which a sysroot build does not use; record a trace with ONELF_TRACE instead",
            ));
        }
        let Some(target) = resolve_primary(opts) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--trace-run needs --target to name what to run",
            ));
        };
        let family = program_interp(&target)
            .as_deref()
            .and_then(libc_family_from_interp);
        eprintln!(
            "{} {} for up to {}s to see what it loads...",
            color::bold("Running"),
            target
                .strip_prefix(&opts.directory)
                .unwrap_or(&target)
                .display(),
            opts.trace_seconds
        );
        // The tree's own library directories come first: `LD_LIBRARY_PATH`
        // is searched before any run path, and a host copy found there
        // would stand in for a library the tree ships. `--search-path`
        // follows, then the host's directories, which a tree bundled
        // before needs because its loader has them scrubbed out.
        let tree = opts.directory.canonicalize()?;
        let mut library_path: Vec<PathBuf> = Vec::new();
        let tree_dirs = std::iter::once(opts.directory.join(&opts.lib_dir))
            .chain(elf_files.iter().flat_map(|f| parse_rpaths(f)))
            .filter_map(|d| d.canonicalize().ok())
            .filter(|d| d.starts_with(&tree));
        let host_dirs = onelf_format::drivers::cache_dirs()
            .into_iter()
            .map(PathBuf::from)
            .chain(STANDARD_LIB_PATHS.iter().map(PathBuf::from));
        for dir in tree_dirs
            .chain(opts.search_path.iter().cloned())
            .chain(host_dirs)
        {
            if dir.is_dir() && !library_path.contains(&dir) {
                library_path.push(dir);
            }
        }
        let loader = trace_loader(&target, &library_path, target_class, target_machine)?;
        let (loaded, opened) = discover::loaded_libraries(
            &target,
            &opts.trace_args,
            opts.trace_seconds,
            family,
            &opts.directory,
            &library_path,
            loader.as_deref(),
        )?;
        traced_opened = opened;
        let (loaded, gpu_stack) = split_gpu_stack(loaded);
        if !gpu_stack.is_empty() {
            let names: Vec<&str> = gpu_stack
                .iter()
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect();
            eprintln!(
                "  {} the host's GPU stack: {}. Pass --gl, --dri or --vulkan \
                 to bundle Mesa instead.",
                color::bold("Left to"),
                names.join(", ")
            );
        }
        let mut traced = 0usize;
        for path in loaded {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if needed_by.contains_key(name) {
                continue;
            }
            needed_by.insert(name.to_string(), "trace run".into());
            if let Some(dir) = path.parent()
                && !rpath_dirs.contains(&dir.to_path_buf())
            {
                rpath_dirs.push(dir.to_path_buf());
            }
            eprintln!("  {} {}", color::bold_green("Loaded"), name);
            traced += 1;
        }
        if traced == 0 {
            eprintln!("  nothing beyond what DT_NEEDED already names");
        }
    }

    // The [bundle] trim-inferred policy, built once the traces exist: a
    // sysroot build reads the recipe's trace tiers, a host scan takes
    // what its traced run opened. The key without a trace trims
    // nothing, the way a recipe without the key does.
    let trim = if opts.trim_inferred {
        match &opts.sysroot {
            // A sysroot build trims by its recorded trace alone; the
            // traced host run cannot apply to it.
            Some(sr) if sr.trace.is_some() => {
                Some(Trim::load(sr.trace.as_deref(), sr.keep.as_deref())?)
            }
            Some(_) => None,
            // A dry run skips the traced run, so the host scan has
            // observed nothing and trims nothing: trimming by an empty
            // set would plan every member away and dispose the copies
            // an earlier build carried into the tree.
            None if opts.trace_run && !opts.dry_run => {
                let tree = opts.directory.canonicalize().ok();
                Some(Trim::from_paths(traced_opened, tree.as_deref()))
            }
            None => None,
        }
    } else {
        None
    };

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

    // An explicit include is a required object, so an exclusion that
    // would drop it is a conflict the build stops on, never a silent
    // narrowing of what the author asked for.
    for lib in &opts.include {
        if let Some(exclude) = excludes.iter().find(|pat| lib.starts_with(**pat)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "explicitly included {lib} matches the exclusion {exclude}; drop the \
                     include or the exclusion, the build will not do both"
                ),
            ));
        }
    }
    // Filter excluded
    needed_by.retain(|soname, _| !is_excluded(soname, &excludes));

    // Filter libs already present in the directory tree. Stale stub loaders
    // are deleted only in the non-dry-run path so they get re-resolved.
    let (_, stubs) = find_existing_libs(&opts.directory);
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
    // Only the library directory counts as present. The RPATH the packer
    // writes reaches that directory and nothing else, so a copy elsewhere
    // in the tree (a vendor's `opt/` install, a Python extension's
    // private library) does not satisfy a need; one is copied in.
    // Sonames already here stay in the walk: their data may never have
    // been carried, and a plugin may sit flat where nothing loads it,
    // and both repairs need the walk to resolve a source the repair may
    // answer from. The walk skips the copy itself and keeps the tree's
    // file.
    let reachable: HashSet<String> = fs::read_dir(opts.directory.join(&opts.lib_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|name| name.contains(".so"))
        .collect();

    // Descriptor activation seeds from every soname already in play: the
    // libraries the tree already bundles and the sonames the packaged
    // sets supply, over the built-ins and the recipe's local rules less
    // the disabled ones.
    let mut discovery = Discovery::new(
        onelf_format::catalog::builtins(),
        opts.rules.local(),
        opts.rules.disabled(),
    )?;
    let mut present: Vec<String> = reachable.iter().cloned().collect();
    present.extend(set_sonames.iter().cloned());
    present.sort();
    present.dedup();
    for soname in &present {
        discovery.observe(soname)?;
    }

    if needed_by.is_empty() {
        eprintln!("All dependencies satisfied, nothing to bundle.");
        let activations = discovery.activations();
        report_activations(&activations);
        publish_activations(activations);
        // PT_INTERP + RUNPATH rewrites still need to run. A prior bundle
        // may have left stale paths (e.g. from an older onelf version)
        // that either don't resolve under the current CWD policy or
        // still rely on LD_LIBRARY_PATH.
        if !opts.dry_run {
            let lib_dest = opts.directory.join(&opts.lib_dir);
            let (rewritten, _scrubbed, unguaranteed, self_extract) =
                finalize_tree(&opts.directory, &opts.lib_dir);
            if rewritten > 0 {
                eprintln!(
                    "{} RUNPATH to reach {} in {} binaries",
                    color::bold("Rewrote"),
                    opts.lib_dir.display(),
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
            attest_staged_outputs(opts)?;
            // After injection, so the audit sees the final DT_NEEDED set.
            verify_needs(
                opts,
                sysroot_platform.as_ref(),
                &set_sonames,
                &classifier,
                &HashSet::new(),
                &[],
            )?;
        }
        return Ok(());
    }

    // target_class / target_machine were derived up front (before the
    // framework bundlers) from the --target-respecting set and are reused
    // here so the dependency walk resolves against the same architecture.

    // Determine target libc family from PT_INTERP. Used to skip spurious
    // cross-libc transitive dependencies (e.g. libgcc_s on a glibc host pulls
    // in libc.so.6 + ld-linux, which can't be used by a musl-linked binary).
    let target_libc = primary
        .as_deref()
        .and_then(program_interp)
        .as_deref()
        .and_then(libc_family_from_interp)
        .or_else(|| {
            elf_files.iter().find_map(|f| {
                program_interp(f)
                    .as_deref()
                    .and_then(libc_family_from_interp)
            })
        });

    // A tree spanning two libc families has its minority dependencies
    // filtered out below. Say which family won, so that is a decision the
    // packager can see rather than a silent narrowing.
    if let Some(chosen) = target_libc {
        let others: Vec<&PathBuf> = elf_files
            .iter()
            .filter(|f| {
                program_interp(f)
                    .as_deref()
                    .and_then(libc_family_from_interp)
                    .is_some_and(|fam| fam != chosen)
            })
            .collect();
        if !others.is_empty() {
            eprintln!(
                "  {} tree mixes libc families; bundling for {:?}{}",
                color::bold("Note:"),
                chosen,
                primary
                    .as_deref()
                    .and_then(|p| p.strip_prefix(&opts.directory).ok())
                    .map(|p| format!(" (from {})", p.display()))
                    .unwrap_or_default()
            );
            for f in others.iter().take(5) {
                eprintln!(
                    "    other family: {}",
                    f.strip_prefix(&opts.directory).unwrap_or(f).display()
                );
            }
        }
    }

    // Drop sonames from the initial queue that belong to the wrong libc family.
    if let Some(target) = target_libc {
        needed_by.retain(|soname, _| libc_family_of_soname(soname).is_none_or(|fam| fam == target));
    }

    // In sysroot mode the universe is the sysroot: no loader cache, no
    // store, only its library directories.
    let mut ldconfig_cache = if opts.sysroot.is_some() {
        HashMap::new()
    } else {
        build_lib_cache()
    };
    let mut search_paths: Vec<PathBuf> = opts
        .sysroot
        .iter()
        .flat_map(|sr| {
            ["usr/lib", "usr/lib64", "lib", "lib64"]
                .into_iter()
                .filter_map(move |d| onelf_sysroot::path::in_root(&sr.root, Path::new(d)).ok())
                .filter(|d| d.is_dir())
                .chain(onelf_sysroot::ldconf::search_dirs(&sr.root))
        })
        .collect();
    search_paths.extend(opts.search_path.iter().cloned());
    search_paths.extend(rpath_dirs);
    let lib_dest = opts.directory.join(&opts.lib_dir);
    // The identity the tree admission reads. A development run hands a
    // canonicalized AppDir over while a build passes the path the
    // author spelled, so both spellings decide which search
    // directories are the tree's own.
    let tree_root = fs::canonicalize(&opts.directory).unwrap_or_else(|_| opts.directory.clone());
    // The directories the resource groups themselves discovered their
    // members in. A group draws from the roots the walk admitted, the
    // same boundary the carried-soname repair reads, so a member's
    // placement repair follows its group's own discovery.
    let mut repair_extra: HashSet<PathBuf> = HashSet::new();

    let mut not_found: Vec<(String, String)> = Vec::new();
    // In sysroot mode the policy holds for dependencies too: a library it
    // prunes is not brought back because something bundled needs it. The
    // verifier then names that something, which is the file to prune next.
    let policy = opts
        .sysroot
        .as_ref()
        .and_then(|sr| sr.policy.as_deref())
        .map(Policy::load)
        .transpose()?;
    let pruned_by_policy = |src: &Path| -> bool {
        let (Some(sr), Some(policy)) = (opts.sysroot.as_ref(), policy.as_ref()) else {
            return false;
        };
        src.strip_prefix(&sr.root)
            .ok()
            .is_some_and(|rel| policy.matches(&rel.to_string_lossy()))
    };
    // The same hold for whatever else the build deliberately removed
    // from the selected source: a trace-pruned file stays pruned, and
    // the walk never restores it from the sysroot tree behind the
    // pruning's back. A soname the policy names is covered above even
    // when no package owns it; this covers the rest.
    let pruned_from_source = |src: &Path| -> bool {
        pruned_by_policy(src)
            || opts.sysroot.as_ref().is_some_and(|sr| {
                let Some(universe) = sysroot_universe.as_ref() else {
                    return false;
                };
                src.strip_prefix(&sr.root).ok().is_some_and(|rel| {
                    let rel = rel.to_string_lossy();
                    !universe.admits(&rel)
                        && matches!(universe.refusal(&rel), sysroot::Refusal::Pruned { .. })
                })
            })
    };
    // Likewise a file of a package left to the host whole.
    let with_host_package = |src: &Path| -> bool {
        opts.sysroot.as_ref().is_some_and(|sr| {
            src.strip_prefix(&sr.root)
                .ok()
                .is_some_and(|rel| host_files.contains(rel.to_string_lossy().as_ref()))
        })
    };
    // What a found library is: inside a sysroot its links resolve there,
    // never on the packer's machine.
    let sysroot_root = opts.sysroot.as_ref().map(|sr| sr.root.as_path());
    let real_path = |src: &Path| -> PathBuf {
        match sysroot_root.and_then(|root| Some((root, src.strip_prefix(root).ok()?))) {
            Some((root, rel)) => {
                onelf_sysroot::path::in_root(root, rel).unwrap_or_else(|_| src.to_path_buf())
            }
            None => fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf()),
        }
    };
    let mut left_out: Vec<(String, String)> = Vec::new();
    let mut left_with_host: Vec<(String, String)> = Vec::new();
    let mut already_processed: HashSet<String> = HashSet::new();
    let mut expanded_nix: HashSet<PathBuf> = HashSet::new();
    // The objects the walk resolved, which descriptor discovery searches
    // its declared group directories against, and the names already in it.
    let mut admitted: Vec<AdmittedLibrary> = Vec::new();
    let mut admitted_names: HashSet<String> = HashSet::new();
    // The AppDir destinations activated groups declared for the plugins
    // and helpers they collected, which supersede flat placement for
    // those sonames.
    let mut group_dests: HashMap<String, PathBuf> = HashMap::new();
    // Where each collected group member was found, which the walk copies
    // from instead of resolving the name through the search paths.
    let mut group_sources: HashMap<String, PathBuf> = HashMap::new();
    // The collected helper executables among them, whose declared
    // destinations sit wherever the tree root keeps them, not
    // necessarily in the library directory.
    let mut helper_dests: HashSet<String> = HashSet::new();
    // BLAKE3(content) -> the destination the file was written to,
    // relative to the tree root, so aliases with identical bytes
    // symlink instead of copy.
    let mut bundled_by_hash: HashMap<[u8; 32], PathBuf> = HashMap::new();
    // The walk's writes, queued for the executor once the plan holds no
    // finding.
    let mut actions: Vec<WalkAction> = Vec::new();
    // What the declared sets supply when an activated group's tree carry
    // misses: read verified out of each pin's build into the pin's own
    // subtree. The declarations live beside the tree in a build without
    // a sysroot and in the sysroot section of a database-backed one.
    // A set-less build consults nothing here, and neither does a dry
    // run, which fetches nothing.
    // Only a set some entrypoint selects is ever read for preparation:
    // a declared set no selection names is never fetched.
    let selected = |sets: &[sysroot::SharedSet]| -> Vec<sysroot::SharedSet> {
        sets.iter()
            .filter(|set| selected_by_an_entrypoint(opts, &set.name))
            .cloned()
            .collect()
    };
    let tree_sets = selected(&opts.sets);
    let sysroot_sets = selected(
        opts.sysroot
            .as_ref()
            .map(|sr| sr.sets.as_slice())
            .unwrap_or(&[]),
    );
    let mut preparation =
        SetPreparation::new(&tree_sets, &sysroot_sets, &opts.lib_dir, opts.dry_run);
    // The destinations a required descriptor group placed, which the
    // drop pass never omits to make the assembly succeed.
    let mut required_dests: HashSet<PathBuf> = HashSet::new();
    // The walk's initial objects enter the shared discovery queue, and
    // their sonames activate rules. Seeds are sorted, so the drain order
    // is a function of the input set, not of enumeration order.
    let mut seeds: Vec<String> = needed_by.keys().cloned().collect();
    seeds.sort();
    for soname in &seeds {
        discovery.observe(soname)?;
    }
    discovery.seed(&seeds);
    // The groups the activator queued, as rule and identity pairs in
    // queue order, and the rules already mirrored.
    let mut known_groups: HashSet<String> = HashSet::new();
    let mut group_queue: VecDeque<(String, String)> = VecDeque::new();
    mirror_new_groups(&discovery, &mut known_groups, &mut group_queue);
    // Name service modules glibc loads by dlopen, let through the excluded
    // `libnss_` prefix when libc lands: `compat` on every glibc, and
    // `files` and `dns` too before 2.34 folded those into libc.so.6.
    // Without them every user, group, host and service lookup fails on a
    // host whose own glibc did not win.
    let mut nss_allow: HashSet<String> = HashSet::new();

    // On NixOS: pre-expand cache for libs already in the dest dir from previous runs,
    // so their transitive nix deps are discoverable.
    if Path::new("/nix/store").is_dir() && opts.sysroot.is_none() {
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
                None,
                LookupScope::Host,
            ) {
                let resolved = fs::canonicalize(&src).unwrap_or(src);
                expand_nix_cache(&resolved, &mut ldconfig_cache, &mut expanded_nix);
            }
        }
    }

    // The directories bundled objects compile in, found each time the
    // queue drains over the objects admitted since, until a round finds
    // nothing new: an inferred plugin group's members are objects too.
    let mut inference = infer::Inference::default();
    let mut inferred_read: HashSet<PathBuf> = HashSet::new();
    let mut inferred_carried: Vec<(PathBuf, PathBuf, Carry)> = Vec::new();
    // Every activated plugin group's destination with the host library
    // path its found candidate spells, for the plugin-dirs record: a
    // described group has no compiled-in spellings, so the descriptor's
    // own declaration is what names its host counterpart.
    let mut described_dirs: Vec<(String, String)> = Vec::new();
    // Collected members the copy carries without owning their name, as
    // file name and source path, for the inference rounds to read their
    // own compiled-in directories from.
    let mut member_scan: Vec<(String, PathBuf)> = Vec::new();
    let described: Vec<&Descriptor> = onelf_format::catalog::builtins()
        .iter()
        .chain(
            opts.rules
                .local()
                .iter()
                .filter(|rule| !opts.rules.disabled().contains(&rule.id)),
        )
        .collect();
    let tree_files: Vec<(String, PathBuf)> = elf_files
        .iter()
        .filter_map(|file| Some((file.file_name()?.to_str()?.to_string(), real_path(file))))
        .collect();
    loop {
        let Some(item) = discovery.pop() else {
            // Every bundled object is read: the libraries the walk
            // admitted and every other ELF file of the tree, programs
            // among them, since a framework can keep its plugins beside
            // its own binaries, as Xorg does in `lib/xorg/modules`. A host
            // scan's tree copy of a library takes its prefix from the host
            // file it stands for, which is where its compiled-in paths
            // point. A rule's library was already looked up when it was
            // admitted, so it keeps what that lookup found. The twin only
            // places a prefix, so its libc family is not read.
            let tree_roots: [&Path; 2] = [&tree_root, &opts.directory];
            // The tree's relative spelling would claim every relative
            // path, a sysroot's among them, so only absolute ones place
            // a prefix below the source's root.
            let prefix_trees: Vec<&Path> = tree_roots
                .iter()
                .copied()
                .filter(|root| root.is_absolute())
                .collect();
            let twin_dirs = host_dirs(&search_paths, &tree_roots);
            let twin_of = |name: &str, file: &Path, admitted_rule_copy: bool| {
                let library = name.ends_with(".so") || name.contains(".so.");
                let in_tree = tree_roots.iter().any(|root| file.starts_with(root));
                if opts.sysroot.is_some() || !in_tree || !library || admitted_rule_copy {
                    return None;
                }
                host_twin(
                    name,
                    &copied_from,
                    &tree_roots,
                    &twin_dirs,
                    &ldconfig_cache,
                    (target_class, target_machine, None),
                    LookupScope::WithoutStore,
                )
            };
            let mut objects: Vec<infer::Object> = Vec::new();
            for lib in &admitted {
                if !inferred_read.insert(lib.path().to_path_buf()) {
                    continue;
                }
                let twin = match lib.provenance() {
                    Some(host) => Some(host.to_path_buf()),
                    None => twin_of(lib.soname(), lib.path(), discovery.triggers(lib.soname())),
                };
                objects.push(infer::Object {
                    name: lib.soname(),
                    file: lib.path(),
                    twin,
                });
            }
            for (name, path) in &tree_files {
                if inferred_read.insert(path.clone()) {
                    objects.push(infer::Object {
                        name,
                        file: path,
                        twin: twin_of(name, path, false),
                    });
                }
            }
            let scanned = std::mem::take(&mut member_scan);
            for (name, path) in &scanned {
                if inferred_read.insert(path.clone()) {
                    objects.push(infer::Object {
                        name,
                        file: path,
                        twin: twin_of(name, path, false),
                    });
                }
            }
            let fresh = inference.read(
                &objects,
                &infer::Scope {
                    sysroot: sysroot_universe.as_ref(),
                    trees: &prefix_trees,
                    exclude: &opts.exclude,
                    descriptors: &described,
                    lib_dir: &opts.lib_dir,
                },
            )?;
            if fresh.is_empty() {
                break;
            }
            for source in fresh {
                let Some(dir) = inference.get(&source).cloned() else {
                    continue;
                };
                collect_inferred(
                    &dir,
                    &mut inferred_carried,
                    opts,
                    sysroot_universe.as_ref(),
                    trim.as_ref(),
                    (target_class, target_machine),
                    &excludes,
                    &reachable,
                    &mut WalkState {
                        already_processed: &mut already_processed,
                        search_paths: &mut search_paths,
                        repair_extra: &mut repair_extra,
                        group_dests: &mut group_dests,
                        group_sources: &mut group_sources,
                        needed_by: &mut needed_by,
                        discovery: &mut discovery,
                        plan: &mut plan,
                        actions: &mut actions,
                        scan: &mut member_scan,
                    },
                )?;
            }
            continue;
        };
        // The activator hands a popped group out as an opaque key; the
        // activation report names the same rules and groups in the order
        // the queue was filled, so mirroring it identifies each popped
        // group without reaching into the activator.
        mirror_new_groups(&discovery, &mut known_groups, &mut group_queue);
        let soname = match item {
            WorkItem::Object(soname) => soname,
            WorkItem::Group(_) => {
                let Some((rule, group)) = group_queue.pop_front() else {
                    continue;
                };
                let Some(descriptor) = opts.rules.find(&rule) else {
                    continue;
                };
                // Plugin groups collect here, into the object queue.
                if let Some(declared) = descriptor
                    .resources
                    .plugins
                    .iter()
                    .find(|g| g.id.as_str() == group)
                {
                    // The roots the rule's own triggering libraries
                    // admitted: a directory another installation keeps
                    // beside an unrelated admitted library never answers
                    // for this rule, the same boundary the data groups
                    // read, less the host provenance only data may use.
                    let owners: Vec<AdmittedLibrary> = admitted
                        .iter()
                        .filter(|lib| {
                            descriptor
                                .triggers
                                .iter()
                                .any(|t| t.matches_soname(lib.soname()))
                        })
                        .map(AdmittedLibrary::without_provenance)
                        .collect();
                    let mut collection = collect::collect_group(
                        &rule,
                        declared,
                        &owners,
                        sysroot_universe.as_ref(),
                        target_class,
                        target_machine,
                    )?;
                    let origin = format!("rule {rule} group {group}");
                    exclude_recipe_members(&mut collection, &opts.exclude);
                    // Where the group's found candidate spells its host
                    // counterpart, whether or not a member survives the
                    // recipe's excludes: the record names destinations
                    // the tree actually holds, decided later.
                    if described_group_names_record(&collection.plugins, &collection.counterpart) {
                        described_dirs.push((
                            declared.destination.as_str().to_owned(),
                            collection.counterpart.clone(),
                        ));
                    }
                    // The group's finding is recorded before any member
                    // is planned, so a build that fails on a member
                    // still reports the group.
                    report_collection(&collection, declared.destination.as_str());
                    let firsts = plan_collection(
                        &mut collection,
                        &origin,
                        &opts.directory,
                        &mut WalkState {
                            already_processed: &mut already_processed,
                            search_paths: &mut search_paths,
                            repair_extra: &mut repair_extra,
                            group_dests: &mut group_dests,
                            group_sources: &mut group_sources,
                            needed_by: &mut needed_by,
                            discovery: &mut discovery,
                            plan: &mut plan,
                            actions: &mut actions,
                            scan: &mut member_scan,
                        },
                    )?;
                    queue_collection(
                        &collection,
                        &origin,
                        &excludes,
                        &reachable,
                        &firsts,
                        &mut WalkState {
                            already_processed: &mut already_processed,
                            search_paths: &mut search_paths,
                            repair_extra: &mut repair_extra,
                            group_dests: &mut group_dests,
                            group_sources: &mut group_sources,
                            needed_by: &mut needed_by,
                            discovery: &mut discovery,
                            plan: &mut plan,
                            actions: &mut actions,
                            scan: &mut member_scan,
                        },
                        sysroot_universe.as_ref(),
                    )?;
                    if declared.requirement == Requirement::Required {
                        required_dests
                            .extend(collection.plugins.iter().map(|p| p.destination.clone()));
                    }
                    continue;
                }
                // Data groups carry here. Helper groups enqueue their
                // declared executables into the object queue, and a
                // helper names an associated plugin or data group.
                if let Some(declared) = descriptor
                    .resources
                    .data
                    .iter()
                    .find(|g| g.id.as_str() == group)
                {
                    carry_data_group(
                        &opts.directory,
                        descriptor,
                        declared,
                        &admitted,
                        sysroot_universe.as_ref(),
                        &mut plan,
                        &mut actions,
                        &mut preparation,
                        &mut required_dests,
                    )?;
                }
                if let Some(declared) = descriptor
                    .resources
                    .helpers
                    .iter()
                    .find(|h| h.id.as_str() == group)
                {
                    collect_helper_group(
                        &opts.directory,
                        descriptor,
                        declared,
                        &admitted,
                        sysroot_universe.as_ref(),
                        &mut search_paths,
                        &mut repair_extra,
                        &mut group_dests,
                        &mut helper_dests,
                        &mut needed_by,
                        &mut already_processed,
                        &mut discovery,
                        &mut preparation,
                        &mut plan,
                        &mut required_dests,
                    )?;
                }
                continue;
            }
        };
        if already_processed.contains(&soname)
            || (is_excluded(&soname, &excludes) && !nss_allow.contains(&soname))
        {
            continue;
        }
        already_processed.insert(soname.clone());

        // The soname may already sit in the tree from a previous run. A
        // re-run still owes it the repair work below: the data beside its
        // source (a tree bundled before the data carry existed has the
        // library but not the data) and a plugin left flat where nothing
        // loads it. Both need a source the repair may answer from; when
        // none of them holds the soname, the tree keeps what it has, as
        // the old skip did. For a collected group member the declared destination is the place
        // already held; for every other soname the flat library copy.
        let already_flat = if helper_dests.contains(&soname) {
            group_dests
                .get(&soname)
                .is_some_and(|dest| opts.directory.join(dest).exists())
        } else {
            lib_dest.join(&soname).exists()
        };

        let requirer = needed_by
            .get(&soname)
            .cloned()
            .unwrap_or_else(|| "?".into());

        // A soname the tree already carries repairs only from sources
        // the build declares or owns; a soname absent from it resolves
        // the way the host-scan walk always has.
        let located = if let Some(found) = group_sources.get(&soname) {
            Some(found.clone())
        } else if already_flat {
            let sources: Vec<PathBuf> = search_paths
                .iter()
                .filter(|dir| {
                    repair_extra.contains(*dir)
                        || repair_admits(
                            dir,
                            &tree_root,
                            &opts.directory,
                            &opts.search_path,
                            sysroot_root,
                        )
                })
                .cloned()
                .collect();
            locate_lib(
                &soname,
                &ldconfig_cache,
                &sources,
                target_class,
                target_machine,
                sysroot_root,
                LookupScope::Declared,
            )
        } else {
            locate_lib(
                &soname,
                &ldconfig_cache,
                &search_paths,
                target_class,
                target_machine,
                sysroot_root,
                LookupScope::Host,
            )
        };
        match located.map(|src| {
            let resolved = real_path(&src);
            (src, resolved)
        }) {
            // The soname link and the file it names are both checked: a
            // policy may name either.
            Some((src, resolved)) if pruned_from_source(&src) || pruned_from_source(&resolved) => {
                if already_flat {
                    continue;
                }
                left_out.push((soname, requirer));
            }
            Some((src, resolved)) if with_host_package(&src) || with_host_package(&resolved) => {
                if already_flat {
                    continue;
                }
                left_with_host.push((soname, requirer));
            }
            Some((src, resolved)) => {
                // A bundled tree's RUNPATH names its own lib directory,
                // so the search resolves the tree's own copies first, and
                // a tree cannot be its own source for repair. When the
                // soname sits flat from an older run and the candidate is
                // that flat copy, look it up again past the tree's own
                // directories, among the sources a repair may answer
                // from: the data to carry lives beside that source, and
                // a plugin's subdirectory is decided by where it keeps
                // the plugin. When none of them holds the soname, the
                // tree keeps what it has.
                let flat = lib_dest.join(&soname);
                let resolved = if already_flat && is_same_file(&real_path(&src), &flat) {
                    let outside: Vec<PathBuf> = search_paths
                        .iter()
                        .filter(|dir| {
                            (repair_extra.contains(*dir)
                                || repair_admits(
                                    dir,
                                    &tree_root,
                                    &opts.directory,
                                    &opts.search_path,
                                    sysroot_root,
                                ))
                                && !dir.starts_with(&tree_root)
                                && !dir.starts_with(&opts.directory)
                        })
                        .cloned()
                        .collect();
                    match locate_lib(
                        &soname,
                        &ldconfig_cache,
                        &outside,
                        target_class,
                        target_machine,
                        sysroot_root,
                        LookupScope::Declared,
                    ) {
                        Some(host) if !is_same_file(&host, &flat) => real_path(&host),
                        _ => resolved,
                    }
                } else {
                    resolved
                };
                // Every object the walk resolved is a root descriptor
                // discovery may search its group directories against.
                // A host scan's tree copy of a rule's library also
                // searches the host installation it came from or
                // resolves to.
                if admitted_names.insert(soname.clone()) {
                    let host_copy = opts.sysroot.is_none()
                        && (resolved.starts_with(&tree_root)
                            || resolved.starts_with(&opts.directory))
                        && discovery.triggers(&soname);
                    admitted.push(if host_copy {
                        admit_tree_copy(
                            &soname,
                            &resolved,
                            &copied_from,
                            &[&tree_root, &opts.directory],
                            &search_paths,
                            &ldconfig_cache,
                            (target_class, target_machine, target_libc),
                        )
                    } else {
                        AdmittedLibrary::new(soname.clone(), resolved.clone())
                    });
                }
                let size = fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
                // A collected group member's declaration decides its
                // destination; every other soname lies flat in the
                // library directory.
                let rel = match group_dests.get(&soname) {
                    Some(dest) => dest.clone(),
                    None => opts.lib_dir.join(&soname),
                };
                let dest = opts.directory.join(&rel);
                // A plugin a pre-fix run left flat: the subdirectory copy
                // below replaces it. A helper never sat flat, so an
                // existing destination is simply kept.
                let stale_flat = !helper_dests.contains(&soname)
                    && already_flat
                    && rel != opts.lib_dir.join(&soname);
                // A plugin keeping its subdirectory is already bundled on
                // a re-run, or the candidate is the tree's own file, found
                // through a search dir inside the tree. A flat copy is
                // still migrated into the subdirectory, where its loader
                // looks.
                if dest.exists() || is_same_file(&resolved, &dest) {
                    if stale_flat {
                        actions.push(WalkAction::MigrateFlat {
                            flat: lib_dest.join(&soname),
                            new_rel: rel.clone(),
                        });
                    }
                    continue;
                }

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

                let bytes = fs::read(&resolved).ok();
                let content_hash: Option<[u8; 32]> =
                    bytes.as_deref().map(|b| blake3::hash(b).into());
                if let Some(b) = bytes.as_deref() {
                    plan.note_source(&resolved, b)?;
                }
                if soname == "libc.so.6" {
                    // `compat` is still a module on every glibc, and a host
                    // whose nsswitch.conf names it with nothing after it
                    // (openSUSE does) would resolve no user at all without
                    // it. `files` and `dns` joined libc in 2.34.
                    let old = bytes
                        .as_deref()
                        .is_some_and(|b| !b.windows(10).any(|w| w == b"GLIBC_2.34"));
                    let modules: &[&str] = if old {
                        &["libnss_compat.so.2", "libnss_files.so.2", "libnss_dns.so.2"]
                    } else {
                        &["libnss_compat.so.2"]
                    };
                    for module in modules {
                        nss_allow.insert(module.to_string());
                        needed_by
                            .entry(module.to_string())
                            .or_insert_with(|| "libc.so.6 (nsswitch)".into());
                        discovery.enqueue(module)?;
                    }
                    for compat in GLIBC_COMPAT_LIBS {
                        needed_by
                            .entry(compat.to_string())
                            .or_insert_with(|| "libc.so.6 (compatibility)".into());
                        discovery.enqueue(compat)?;
                    }
                }
                if let Some(hash) = content_hash
                    && let Some(existing) = bundled_by_hash.get(&hash).cloned()
                {
                    let target = link_target(rel.parent().unwrap_or(Path::new("")), &existing);
                    if plan.admit_link(
                        &rel,
                        &target,
                        &format!("{soname} (needed by {requirer})"),
                        Existing::Overwrite,
                    )? == Admission::Write
                    {
                        actions.push(WalkAction::LinkAlias {
                            soname: soname.clone(),
                            resolved: resolved.clone(),
                            rel: rel.clone(),
                            existing: existing.clone(),
                            requirer: requirer.clone(),
                            size,
                            stale_flat,
                        });
                    }
                    if stale_flat {
                        actions.push(WalkAction::MigrateFlat {
                            flat: lib_dest.join(&soname),
                            new_rel: rel.clone(),
                        });
                    }
                    continue;
                }

                let write = match bytes.as_deref() {
                    Some(b) => {
                        plan.admit_file(
                            &rel,
                            plan::file_hash(b),
                            &format!("{soname} (needed by {requirer})"),
                            Existing::SkipOnAny,
                        )? == Admission::Write
                    }
                    None => true,
                };
                if write {
                    actions.push(WalkAction::CopyLib {
                        soname: soname.clone(),
                        resolved: resolved.clone(),
                        rel: rel.clone(),
                        size,
                        requirer: requirer.clone(),
                        stale_flat,
                        libc_data: soname == "libc.so.6",
                    });
                }

                if let Some(hash) = content_hash {
                    bundled_by_hash.insert(hash, rel.clone());
                }

                // Collect RPATHs from resolved lib for transitive dep resolution
                let sysroot_root = opts.sysroot.as_ref().map(|sr| sr.root.as_path());
                for dir in parse_rpaths_under(&resolved, sysroot_root) {
                    if !search_paths.contains(&dir) {
                        search_paths.push(dir);
                    }
                }

                // Resolve transitive dependencies
                if opts.recursive {
                    for dep in lib_needed {
                        // Only the library directory satisfies a need, as
                        // for the direct ones above: a copy elsewhere in the
                        // tree is where the loader never looks.
                        if already_processed.contains(&dep)
                            || is_excluded(&dep, &excludes)
                            || reachable.contains(&dep)
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
                        discovery.enqueue(&dep)?;
                    }
                }
            }
            None => {
                // Already in the tree from a previous run, a framework
                // copier or the publisher: the tree keeps its copy, as
                // the skip in place always did. Without a host copy there
                // is nothing to repair with, but in a host scan a rule's
                // library still answers its groups, from the tree copy
                // and the host installation it stands for.
                if already_flat {
                    if opts.sysroot.is_none()
                        && discovery.triggers(&soname)
                        && admitted_names.insert(soname.clone())
                    {
                        let copy = match group_dests.get(&soname) {
                            Some(dest) => opts.directory.join(dest),
                            None => lib_dest.join(&soname),
                        };
                        admitted.push(admit_tree_copy(
                            &soname,
                            &real_path(&copy),
                            &copied_from,
                            &[&tree_root, &opts.directory],
                            &search_paths,
                            &ldconfig_cache,
                            (target_class, target_machine, target_libc),
                        ));
                    }
                    continue;
                }
                not_found.push((soname, requirer));
            }
        }
    }

    // An explicit include is a required object: whatever would remove
    // it after the fact fails the build instead, naming the include,
    // the outcome the walk reached and the package involved. The build
    // neither selects the unselected provider nor restores a pruned
    // file to satisfy it.
    for lib in &opts.include {
        let listed = |list: &[(String, String)]| list.iter().any(|(s, _)| s == lib);
        let verdict = classifier.classify(lib);
        let outcome = if listed(&left_out) {
            Some(match verdict {
                SourceVerdict::Pruned { owner } => {
                    format!("{owner} holds it, but the policy or a trace removed it")
                }
                _ => "the policy or a trace removes it from the selected source".to_string(),
            })
        } else if listed(&left_with_host) {
            Some(match verdict {
                SourceVerdict::HostProvider { owner } => {
                    format!("left to the host package {owner}")
                }
                _ => "the selected source hands it to a host package whole".to_string(),
            })
        } else if (opts.sysroot.is_some() || opts.strict)
            && listed(&not_found)
            && !matches!(verdict, SourceVerdict::Carried)
        {
            Some(verdict.reason())
        } else {
            None
        };
        if let Some(reason) = outcome {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "explicitly included {lib} does not enter the bundle: {reason}; an \
                     explicit include cannot be dropped, and the build neither selects a \
                     package nor restores a file to carry it"
                ),
            ));
        }
    }

    // Activation has reached its fixed point: every soname the closure
    // encountered has been matched, and no group queues further work
    // until the resource discovery phases feed their objects back.
    let activations = discovery.activations();
    report_activations(&activations);
    publish_activations(activations);
    // The pins whose resources the sets supplied: each write is already
    // admitted to the plan, so a finding anywhere stops the build here,
    // before any tree write and any pin write.
    publish_prepared(preparation.queue_actions(&mut actions));

    // The interpreter alias links the loader needs, planned here so a
    // finding anywhere in the plan stops the build before the first
    // copy runs.
    let mut interp_links: Vec<(String, String)> = Vec::new();
    {
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
            // The walk copies the interpreter itself when a program names
            // it, so only a destination nothing else writes takes the alias.
            if target.exists()
                || target.is_symlink()
                || plan.is_planned(&opts.lib_dir.join(&interp_name))
            {
                continue;
            }
            let Some(libc_name) = libc_alias_for(&interp_name) else {
                continue;
            };
            let app_rel = opts.lib_dir.join(&interp_name);
            let origin = format!("{interp_name} (loader alias of {libc_name})");
            if plan.admit_link(&app_rel, &libc_name, &origin, Existing::SkipOnAny)?
                == Admission::Write
            {
                interp_links.push((interp_name, libc_name));
            }
        }
    }
    if plan.has_findings() {
        return Err(plan.error());
    }
    let mut outcome = WalkOutcome::default();
    let executed = execute_walk(
        &opts.directory,
        opts.dry_run,
        opts.strip,
        &lib_dest,
        &actions,
        &plan,
        &mut outcome,
    );
    // What the walk wrote is the build's own output whether or not it
    // finished, so the next build can tell it from a publisher's edit.
    if !opts.dry_run {
        sysroot::record_generated(&opts.directory, std::mem::take(&mut outcome.written))?;
    }
    executed?;
    let mut copied = outcome.copied;

    // Summary
    copied.sort_by(|a, b| a.0.cmp(&b.0));
    not_found.sort();
    left_out.sort();
    left_with_host.sort();

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

    // The outcome lists carry the classification with them, so each
    // finding says whether a library is absent, left to an unselected
    // optional package, or deliberately removed, and names the package
    // involved.
    for (lib, requirer) in &left_with_host {
        completeness::record(Finding::new(
            lib,
            Outcome::Selected,
            format!(
                "{} (needed by {requirer})",
                classifier.classify(lib).reason()
            ),
        ));
    }
    for (lib, requirer) in &left_out {
        completeness::record(Finding::new(
            lib,
            Outcome::Removed,
            format!(
                "{} (needed by {requirer})",
                classifier.classify(lib).reason()
            ),
        ));
    }
    for (lib, requirer) in &not_found {
        let reason = if opts.sysroot.is_some() {
            format!(
                "{} (needed by {requirer})",
                classifier.classify(lib).reason()
            )
        } else {
            format!("the host scan did not find it (needed by {requirer})")
        };
        completeness::record(Finding::new(lib, Outcome::Unresolved, reason));
    }

    // Ensure each ELF's PT_INTERP basename exists in lib_dest as a file or
    // symlink. On musl the loader is referenced as ld-musl-*.so.1 but bundled
    // as libc.musl-*.so.1 (both are names for the same file on disk); without
    // the alias the kernel can't find the interpreter at runtime. The links
    // were planned before anything was written; this only executes them.
    if !opts.dry_run {
        let mut linked: Vec<String> = Vec::new();
        for (interp_name, libc_name) in &interp_links {
            let target = lib_dest.join(interp_name);
            if target.exists() || target.is_symlink() {
                continue;
            }
            let libc_path = lib_dest.join(libc_name);
            if !libc_path.exists() {
                continue;
            }
            if let Err(e) = std::os::unix::fs::symlink(libc_name, &target) {
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
                linked.push(
                    opts.lib_dir
                        .join(interp_name)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        sysroot::record_generated(&opts.directory, linked)?;
    }

    // Strip RPATHs from all ELF files in the directory for portability.
    // Hardcoded absolute paths (e.g. /nix/store/...) won't exist on the
    // target system; LD_LIBRARY_PATH (set by the runtime) is used instead.
    if !opts.dry_run {
        let (rewritten, scrubbed, unguaranteed, self_extract) =
            finalize_tree(&opts.directory, &opts.lib_dir);
        if rewritten > 0 {
            eprintln!(
                "{} RUNPATH to reach {} in {} binaries",
                color::bold("Rewrote"),
                opts.lib_dir.display(),
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

    // The redirection decision needs the final tree, so it is taken here:
    // the rewrites land before the ledger attests the copies, and the
    // needs audit runs on the rewritten bytes. The record lands before
    // the rewrite, since a build that fails from the first rewritten
    // byte on still reruns against bytes only the record can interpret;
    // the drop pass may empty a carried directory the choice named, so
    // what survives is recorded again after the audit, and only the
    // report waits for it.
    // The carried inferred directories, as the redirection decision's
    // own input: the plan's dirs the tree holds, whether it just plans
    // to copy them or already holds them from a previous build, so a
    // rebuild re-decides a held directory and its pair re-enters the
    // record; the tree's own anchor record still wins for anchors. The
    // record's pairs are extracted from the same set, so an omitted
    // directory that shares a carried one's destination cannot name
    // its spellings into the record.
    let carried_dirs: Vec<&infer::InferredDir> = inferred_carried
        .iter()
        .filter(|(.., carry)| matches!(*carry, Carry::Planned | Carry::Held))
        .filter_map(|(_, source, _)| inference.get(source))
        // A plan the walk emptied leaves no destination: a member
        // every strict-libc skip condemned, say, copies nothing.
        // The record's keys are positional, so an anchor over
        // nothing would consume a key the record cannot carry.
        .filter(|dir| opts.directory.join(&dir.destination).is_dir())
        .collect();
    // Anchoring a directory keeps it in the tree, so it stays a plugin
    // directory the launch must consider, and a directory riding a
    // carried parent has no carry entry of its own but is carried all
    // the same: the plugin-dirs record takes this whole set, while the
    // redirection choice below takes the one without the anchored.
    let plugin_candidates: Vec<&infer::InferredDir> = carried_dirs
        .iter()
        .copied()
        .chain(
            inference
                .all()
                .filter(|dir| rides_carried(dir, &inferred_carried)),
        )
        .collect();
    let (redirections, carried_dirs) = if opts.dry_run {
        (Vec::new(), Vec::new())
    } else {
        let persisted = redir::persisted(&opts.directory)?;
        // A destination the record anchors keeps that anchor: a fresh
        // one over it could move the recorded key, which the merge
        // refuses, so the choice never sees it as carried.
        let carried_dirs: Vec<&infer::InferredDir> = carried_dirs
            .iter()
            .filter(|dir| !redir::persisted_anchored(&persisted, dir.destination.as_path()))
            .copied()
            .collect();
        let held: std::collections::HashSet<PathBuf> = inferred_carried
            .iter()
            .filter(|(.., carry)| *carry == Carry::Held)
            .map(|(dest, ..)| dest.clone())
            .collect();
        let tree_elfs = find_elf_files(&opts.directory);
        // A directory the tree already holds takes the rewrite too when
        // an anchor names it: an object newly bundled this run may carry
        // fresh spellings for it, and only an anchor reaches those
        // bytes. A directory that rides such a carried parent has no
        // carry entry of its own, so the scan's own view names it; the
        // same source test collection applies decides a child rides,
        // and one overlapping from another source stays out.
        let rewrite_dirs: Vec<&infer::InferredDir> = inferred_carried
            .iter()
            .filter(|(.., carry)| matches!(*carry, Carry::Planned | Carry::Held))
            .filter_map(|(_, source, _)| inference.get(source))
            .chain(
                inference
                    .all()
                    .filter(|dir| rides_carried(dir, &inferred_carried)),
            )
            .collect();
        let lib_dirs = crate::pack::resolve_lib_dirs(&opts.lib_dirs, &opts.directory);
        // The launch evaluates only the rules the package carries, which
        // are the catalog's enabled descriptors, so a disabled built-in's
        // runtime mapping covers nothing.
        let enabled_rules = opts.rules.enabled();
        let fresh = redir::choose(
            &carried_dirs,
            &enabled_rules,
            &lib_dirs,
            &opts.directory,
            &held,
        );
        let redirections = redir::merge(fresh, &persisted, &opts.directory)?;
        // The record lands before the rewrite, so a decision the
        // rewrite would refuse stops first: a fresh anchor's merged
        // key can be longer than the one its eligibility was checked
        // against, and a refused rewrite must not leave a record
        // naming it.
        redir::ensure_spellings_fit(&redirections, &persisted, &rewrite_dirs)?;
        // The record lands before the rewrite: a rewrite that fails
        // part way, or any build that fails after it, leaves the tree
        // carrying bytes only the record can interpret, and every
        // anchor the decision holds is known before the first byte
        // moves. The drop pass may still empty a carried directory, so
        // what survives is recorded again after the audit, and only the
        // report waits for it.
        let survived: Vec<redir::Redirection> = redirections
            .iter()
            .filter(|r| r.resolves(&opts.directory))
            .cloned()
            .collect();
        redir::write_record(&survived, &opts.directory)?;
        // The plugin-dirs record lands before the rewrite for the same
        // reason: an anchor rewrite replaces the compiled-in spellings
        // the record names, so any build that fails after it leaves
        // bytes only the record can explain. The write after the audit
        // below stays what a successful build publishes, pruned to
        // what the drop pass left.
        let plugin_dirs = merge_plugin_dirs(
            plugin_dir_records(
                &opts.directory,
                &plugin_candidates,
                &described_dirs,
                &opts.lib_dir,
            ),
            persisted_plugin_dirs(&opts.directory)?,
            &opts.directory,
        );
        write_plugin_dirs_record(&opts.directory, &plugin_dirs)?;
        // The rewrite lands exactly what the record holds: an anchor
        // the record cannot name would leave a path nothing resolves.
        let (files, occurrences) = redir::rewrite_anchors(&survived, &rewrite_dirs, &tree_elfs)?;
        if occurrences > 0 {
            eprintln!(
                "{} {} compiled-in paths to anchor paths in {} binaries",
                color::bold("Rewrote"),
                occurrences,
                files
            );
        }
        (redirections, carried_dirs)
    };

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
        attest_staged_outputs(opts)?;
        datadirs::record(&datadirs::unanswered(
            &opts.directory,
            &find_elf_files(&opts.directory),
        ));
        // After injection, so the audit sees the final DT_NEEDED set.
        let inferred_dests = inferred_carried
            .iter()
            .filter(|(_, _, carry)| *carry == Carry::Planned)
            .map(|(dest, _, _)| dest.clone())
            .collect::<Vec<_>>();
        verify_needs(
            opts,
            sysroot_platform.as_ref(),
            &set_sonames,
            &classifier,
            &required_dests,
            &inferred_dests,
        )?;
        // The assembled tree is what a launch resolves against, so the
        // build's own entrypoints are audited against it before the
        // packer runs: each with its own selection, so a set the
        // selection leaves out satisfies nothing and the failure names
        // who asked, what for, and who could have supplied it.
        if let Some((providers, unchecked)) = set_audit.as_ref() {
            audit_entrypoint_selections(opts, sysroot_platform.as_ref(), providers, unchecked)?;
        }
        // The choice was taken before the audit; what it names is
        // reported, and recorded again for what the drop pass left,
        // only while the bundled copy is in place.
        let survived: Vec<redir::Redirection> = redirections
            .into_iter()
            .filter(|r| r.resolves(&opts.directory))
            .collect();
        redir::write_record(&survived, &opts.directory)?;
        // The drop pass may have emptied a carried directory, so what
        // the tree still holds is what the record names, with a
        // previous record folding in for what this build rediscovered
        // nothing about.
        let plugin_dirs = merge_plugin_dirs(
            plugin_dir_records(
                &opts.directory,
                &plugin_candidates,
                &described_dirs,
                &opts.lib_dir,
            ),
            persisted_plugin_dirs(&opts.directory)?,
            &opts.directory,
        );
        write_plugin_dirs_record(&opts.directory, &plugin_dirs)?;
        publish_pairs(redir::record_pairs(
            &survived,
            carried_dirs,
            &opts.recipe_pairs,
        ));
        redir::report(&survived);
    }

    Ok(())
}

/// One object the drop pass omitted: where it sat relative to the tree,
/// the names its dependants ask for it by, the need that condemned it,
/// and the verdict recorded for that need.
struct Dropped {
    object: PathBuf,
    names: Vec<String>,
    need: String,
    verdict: SourceVerdict,
}

/// Whether a described group names its destination in the record: one
/// that collected no member and spells no counterpart leaves the
/// record alone, so a held destination keeps the naming a previous
/// build recorded for it.
fn described_group_names_record(plugins: &[collect::CollectedPlugin], counterpart: &str) -> bool {
    !plugins.is_empty() || !counterpart.is_empty()
}

/// Whether an inferred directory rides a carried parent: some carried
/// directory holds it, at the same place in the parent's copy that it
/// holds in its own source. Such a child has no carry entry of its own,
/// so the carry lists and the record must name it from the scan.
fn rides_carried(dir: &infer::InferredDir, carried: &[(PathBuf, PathBuf, Carry)]) -> bool {
    carried.iter().any(|(dest, source, carry)| {
        matches!(*carry, Carry::Planned | Carry::Held)
            && match dir.destination.strip_prefix(dest) {
                Ok(rel) if !rel.as_os_str().is_empty() => dir.source == source.join(rel),
                _ => false,
            }
    })
}

/// The plugin directories the tree holds, as the record names them: an
/// inferred one with its compiled-in spellings and its path below the
/// library directory, a described one with the host path its found
/// candidate spelled. Only destinations the tree still holds are
/// named, sorted by the relative side.
fn plugin_dir_records(
    directory: &Path,
    inferred: &[&infer::InferredDir],
    described: &[(String, String)],
    lib_dir: &Path,
) -> Vec<PluginDir> {
    let mut out: Vec<PluginDir> = inferred
        .iter()
        .filter(|dir| dir.kind == infer::InferredKind::Plugins)
        .filter(|dir| directory.join(&dir.destination).is_dir())
        .map(|dir| PluginDir {
            relative: dir.destination.to_string_lossy().into_owned(),
            lib_relative: dir
                .destination
                .strip_prefix(lib_dir)
                .map(|below| below.to_string_lossy().into_owned())
                .unwrap_or_default(),
            absolutes: dir.compiled.iter().cloned().collect(),
        })
        .chain(
            described
                .iter()
                .filter(|(dest, _)| directory.join(dest).is_dir())
                .map(|(dest, spelling)| PluginDir {
                    relative: dest.clone(),
                    lib_relative: spelling.clone(),
                    absolutes: Vec::new(),
                }),
        )
        .collect();
    // Two groups may declare one destination, which the plan dedupes to
    // one entry, so the record names it once: the first collector wins,
    // the way the first hit spells a shared group's counterpart.
    let mut seen = HashSet::new();
    out.retain(|dir| seen.insert(dir.relative.clone()));
    out.sort_by(|a, b| a.relative.cmp(&b.relative));
    out
}

/// The record a previous build left, or none: an absent record names no
/// directories, and a damaged one stops the build, since the tree's
/// bytes may name directories only the record can explain.
fn persisted_plugin_dirs(directory: &Path) -> io::Result<Vec<PluginDir>> {
    let path = directory.join(plugindirs::PLUGIN_DIRS_FILE);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    (&file)
        .take(plugindirs::MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    plugindirs::decode(&bytes).map_err(io::Error::other)
}

/// Fold the record a previous build left into the fresh records: a
/// directory this build carries again takes the fresh naming, keeping
/// any recorded spelling no fresh record names, one the tree still
/// holds but this build discovered nothing about keeps its recorded
/// naming, and one the tree has lost leaves with the record.
fn merge_plugin_dirs(
    fresh: Vec<PluginDir>,
    persisted: Vec<PluginDir>,
    directory: &Path,
) -> Vec<PluginDir> {
    if persisted.is_empty() {
        return fresh;
    }
    // A recorded naming whose compiled-in spellings the fresh records
    // name already leaves, so a directory the tree carries elsewhere
    // owns its spellings and cannot leave the record naming them twice.
    let named: HashSet<String> = fresh
        .iter()
        .flat_map(|dir| dir.absolutes.iter().cloned())
        .collect();
    let mut out = fresh;
    for old in persisted {
        match out.iter().position(|dir| dir.relative == old.relative) {
            // The fresh naming wins, but a spelling only the old record
            // holds, an alias an earlier build's rewrite has anchored
            // away, still names a host counterpart the tree carries:
            // it rides along.
            Some(at) => out[at]
                .absolutes
                .extend(old.absolutes.into_iter().filter(|a| !named.contains(a))),
            None => {
                if old.absolutes.iter().any(|a| named.contains(a))
                    || !directory.join(Path::new(&old.relative)).is_dir()
                {
                    continue;
                }
                out.push(old);
            }
        }
    }
    out.sort_by(|a, b| a.relative.cmp(&b.relative));
    out
}

/// Write `.onelf/plugin-dirs` for the directories the tree holds, and
/// record the file in the generated ledger. Carrying no directory drops
/// a record a previous build left, with its ledger entry.
fn write_plugin_dirs_record(directory: &Path, dirs: &[PluginDir]) -> io::Result<()> {
    if dirs.is_empty() {
        let stale = directory.join(plugindirs::PLUGIN_DIRS_FILE);
        match fs::remove_file(&stale) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(()) => {}
        }
        // The entry goes even when the record file was already gone: a
        // ledger attesting a missing file attests nothing.
        sysroot::remove_generated(directory, &[plugindirs::PLUGIN_DIRS_FILE.to_string()])?;
        return Ok(());
    }
    let bytes = plugindirs::encode(dirs).map_err(io::Error::other)?;
    let record = directory.join(plugindirs::PLUGIN_DIRS_FILE);
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&record, bytes)?;
    normalize_mtime(&record);
    sysroot::record_generated(directory, vec![plugindirs::PLUGIN_DIRS_FILE.to_string()])?;
    Ok(())
}

#[cfg(test)]
mod plugin_dirs_record_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("onelf-plugdirs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn inferred(
        destination: &str,
        kind: infer::InferredKind,
        compiled: &[&str],
    ) -> infer::InferredDir {
        infer::InferredDir {
            source: PathBuf::from("/usr").join(destination),
            prefix: PathBuf::from("/usr"),
            rel: PathBuf::from(destination),
            destination: PathBuf::from(destination),
            kind,
            size: 0,
            compiled: compiled.iter().map(|s| s.to_string()).collect(),
            objects: ["libdemo.so.1".to_string()].into_iter().collect(),
        }
    }

    /// Only the carried plugin directories the tree still holds are
    /// named, a data directory is not, and the described groups ride in
    /// with the spelling their candidate spelled, all sorted.
    #[test]
    fn the_record_names_what_the_tree_holds_sorted() {
        let dir = tmp("records");
        fs::create_dir_all(dir.join("lib/demo")).unwrap();
        fs::create_dir_all(dir.join("share/examplefw")).unwrap();
        fs::create_dir_all(dir.join("share/data")).unwrap();
        let plugin = inferred("lib/demo", infer::InferredKind::Plugins, &["/usr/lib/demo"]);
        let data = inferred(
            "share/data",
            infer::InferredKind::Data,
            &["/usr/share/data"],
        );
        let gone = inferred("lib/gone", infer::InferredKind::Plugins, &["/usr/lib/gone"]);

        let records = plugin_dir_records(
            &dir,
            &[&plugin, &data, &gone],
            &[
                ("lib/gone2".to_string(), "gone2".to_string()),
                ("share/examplefw".to_string(), String::new()),
            ],
            Path::new("lib"),
        );

        let relatives: Vec<&str> = records.iter().map(|d| d.relative.as_str()).collect();
        assert_eq!(relatives, ["lib/demo", "share/examplefw"]);
        assert_eq!(records[0].lib_relative, "demo");
        assert_eq!(records[0].absolutes, ["/usr/lib/demo".to_string()]);
        assert_eq!(records[1].lib_relative, "");
        assert!(records[1].absolutes.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The record is written with the encoded bytes, and a build that
    /// carries none drops a previous build's record and its ledger
    /// entry, twice over.
    #[test]
    fn the_record_is_written_and_dropped_with_its_ledger_entry() {
        let dir = tmp("write");
        fs::create_dir_all(dir.join("lib/demo")).unwrap();
        let plugin = inferred("lib/demo", infer::InferredKind::Plugins, &["/usr/lib/demo"]);

        write_plugin_dirs_record(
            &dir,
            &plugin_dir_records(&dir, &[&plugin], &[], Path::new("lib")),
        )
        .unwrap();
        let record = dir.join(plugindirs::PLUGIN_DIRS_FILE);
        assert_eq!(
            fs::read(&record).unwrap(),
            plugindirs::encode(&[PluginDir {
                relative: "lib/demo".to_string(),
                lib_relative: "demo".to_string(),
                absolutes: ["/usr/lib/demo".to_string()].to_vec(),
            }])
            .unwrap()
        );
        assert!(
            sysroot::read_generated(&dir)
                .unwrap()
                .iter()
                .any(|entry| entry.path == plugindirs::PLUGIN_DIRS_FILE),
            "the record joins the generated ledger"
        );

        write_plugin_dirs_record(&dir, &[]).unwrap();
        assert!(!record.exists());
        assert!(
            !sysroot::read_generated(&dir)
                .unwrap()
                .iter()
                .any(|entry| entry.path == plugindirs::PLUGIN_DIRS_FILE),
            "the ledger entry drops with the record"
        );

        write_plugin_dirs_record(&dir, &[]).unwrap();
        assert!(!record.exists(), "dropping without a record stays quiet");

        // A ledger entry whose record file other means have lost goes
        // with the next empty drop all the same.
        sysroot::record_generated(&dir, vec![plugindirs::PLUGIN_DIRS_FILE.to_string()]).unwrap();
        write_plugin_dirs_record(&dir, &[]).unwrap();
        assert!(
            !sysroot::read_generated(&dir)
                .unwrap()
                .iter()
                .any(|entry| entry.path == plugindirs::PLUGIN_DIRS_FILE),
            "a ledger entry attesting a missing record drops too"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A rebuild that discovers nothing fresh keeps a held directory's
    /// recorded naming while the tree holds it, a directory the tree has
    /// lost leaves, and a fresh naming wins over the recorded one, with
    /// the recorded alias spelling riding along.
    #[test]
    fn a_rebuild_keeps_the_held_directories_the_tree_holds() {
        let dir = tmp("merge");
        fs::create_dir_all(dir.join("lib/demo")).unwrap();
        fs::create_dir_all(dir.join("lib/held")).unwrap();

        // What a previous build recorded: lib/demo (carried again this
        // build), lib/gone (the tree has lost it) and lib/held (this
        // build discovers nothing about).
        fs::create_dir_all(dir.join(".onelf")).unwrap();
        std::fs::write(
            dir.join(plugindirs::PLUGIN_DIRS_FILE),
            plugindirs::encode(&[
                PluginDir {
                    relative: "lib/demo".to_string(),
                    lib_relative: "demo".to_string(),
                    absolutes: ["/usr/lib/demo".to_string()].to_vec(),
                },
                PluginDir {
                    relative: "lib/gone".to_string(),
                    lib_relative: "gone".to_string(),
                    absolutes: ["/usr/lib/gone".to_string()].to_vec(),
                },
                PluginDir {
                    relative: "lib/held".to_string(),
                    lib_relative: "held".to_string(),
                    absolutes: ["/usr/lib/held".to_string()].to_vec(),
                },
            ])
            .unwrap(),
        )
        .unwrap();

        let merged = merge_plugin_dirs(
            vec![PluginDir {
                relative: "lib/demo".to_string(),
                lib_relative: String::new(),
                absolutes: Vec::new(),
            }],
            persisted_plugin_dirs(&dir).unwrap(),
            &dir,
        );

        let relatives: Vec<&str> = merged.iter().map(|d| d.relative.as_str()).collect();
        assert_eq!(relatives, ["lib/demo", "lib/held"]);
        assert_eq!(merged[0].lib_relative, "", "the fresh naming wins");
        assert_eq!(
            merged[0].absolutes,
            ["/usr/lib/demo".to_string()],
            "the recorded alias spelling rides along"
        );
        assert_eq!(
            merged[1].absolutes,
            ["/usr/lib/held".to_string()],
            "the held directory keeps its recorded naming"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Two groups declaring one destination name it once, the first
    /// collector winning, and a fresh record whose absolutes a recorded
    /// entry also names takes the spellings over: the directory the tree
    /// carries elsewhere owns them and the record cannot name them twice.
    #[test]
    fn one_destination_is_named_once_and_owns_its_spellings() {
        let dir = tmp("dedupe");
        fs::create_dir_all(dir.join("lib/demo")).unwrap();
        fs::create_dir_all(dir.join("lib/shared")).unwrap();
        let plugin = inferred("lib/demo", infer::InferredKind::Plugins, &["/usr/lib/demo"]);

        let records = plugin_dir_records(
            &dir,
            &[&plugin],
            &[
                ("lib/shared".to_string(), "shared".to_string()),
                ("lib/shared".to_string(), String::new()),
            ],
            Path::new("lib"),
        );
        let relatives: Vec<&str> = records.iter().map(|d| d.relative.as_str()).collect();
        assert_eq!(relatives, ["lib/demo", "lib/shared"]);
        assert_eq!(records[1].lib_relative, "shared", "the first group wins");

        let merged = merge_plugin_dirs(
            records,
            vec![PluginDir {
                relative: "lib/shared".to_string(),
                lib_relative: "elsewhere".to_string(),
                absolutes: ["/usr/lib/demo".to_string()].to_vec(),
            }],
            &dir,
        );
        let relatives: Vec<&str> = merged.iter().map(|d| d.relative.as_str()).collect();
        assert_eq!(relatives, ["lib/demo", "lib/shared"]);
        assert_eq!(
            merged[1].lib_relative, "shared",
            "the fresh naming owns the spelling"
        );
        assert_eq!(
            merged
                .iter()
                .find(|d| d.relative == "lib/demo")
                .unwrap()
                .absolutes,
            ["/usr/lib/demo".to_string()],
        );
        assert!(
            merged
                .iter()
                .find(|d| d.relative == "lib/shared")
                .unwrap()
                .absolutes
                .is_empty(),
            "the claimed spelling is not named twice"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A directory this build rediscovers keeps the recorded spellings
    /// no fresh record names: an alias an earlier build's rewrite has
    /// anchored away still names a host counterpart the tree carries.
    #[test]
    fn a_rediscovered_directory_keeps_its_unclaimed_aliases() {
        let dir = tmp("aliases");
        fs::create_dir_all(dir.join("lib/demo")).unwrap();

        let merged = merge_plugin_dirs(
            vec![PluginDir {
                relative: "lib/demo".to_string(),
                lib_relative: "demo".to_string(),
                absolutes: ["/usr/lib/demo-direct".to_string()].to_vec(),
            }],
            vec![PluginDir {
                relative: "lib/demo".to_string(),
                lib_relative: "demo".to_string(),
                absolutes: [
                    "/usr/lib/demo-direct".to_string(),
                    "/usr/lib/demo-link".to_string(),
                ]
                .to_vec(),
            }],
            &dir,
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].lib_relative, "demo");
        assert_eq!(
            merged[0].absolutes,
            [
                "/usr/lib/demo-direct".to_string(),
                "/usr/lib/demo-link".to_string(),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A damaged record stops the read: bytes nothing can explain must
    /// not pass for naming no directories.
    #[test]
    fn a_damaged_record_stops_the_read() {
        let dir = tmp("damaged");
        fs::create_dir_all(dir.join(".onelf")).unwrap();
        std::fs::write(dir.join(plugindirs::PLUGIN_DIRS_FILE), b"not a record").unwrap();

        assert!(persisted_plugin_dirs(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A group that collected no member and spells no counterpart names
    /// nothing; either alone is naming enough.
    #[test]
    fn a_found_nothing_group_names_the_record_by_a_member_or_a_spelling() {
        assert!(!described_group_names_record(&[], ""));
        assert!(described_group_names_record(
            &[collect::CollectedPlugin {
                soname: "libfw.so".to_string(),
                destination: PathBuf::from("lib/fw"),
                resolved: PathBuf::from("/usr/lib/fw/libfw.so"),
            }],
            ""
        ));
        assert!(described_group_names_record(&[], "fw"));
    }
}

/// Remove bundled objects that could never load: their need is a soname
/// the classification found absent from the selected source, held only
/// behind an unselected optional package, or handed to the host.
///
/// A distribution's closure carries plugins for stacks that were never
/// installed, Qt modules and Python bindings for optional dependencies,
/// and each would otherwise be one more line of policy. Only what this
/// build generated is dropped, and only for one of those three verdicts.
/// What the assembly requires stays, each as a finding: the primary
/// entrypoint and every named manifest entrypoint, a file the publisher
/// placed by hand, an object at an explicitly included soname's
/// destination, and anything at or under a destination a required
/// descriptor group placed, listed in `required`. A soname a selected
/// package holds but the policy or a trace removed is a conflict, not a
/// reason to drop the object that needs it, so that finding stays too
/// and the build fails naming the removal.
///
/// `droppable` narrows the pass further: when it names directories,
/// only objects at or under one of them are dropped, whatever the
/// audit condemns elsewhere. A non-strict build passes the carried
/// inferred directories, the only optional things it carries, and a
/// strict build passes none, keeping every generated object a
/// candidate.
///
/// Returns the dropped objects in path order. `findings` is re-audited
/// until nothing changes, because dropping a library leaves its own
/// dependants unloadable.
fn drop_unloadable(
    opts: &BundleOptions,
    findings: &mut Vec<(PathBuf, Vec<String>)>,
    audit: impl Fn() -> Vec<(PathBuf, Vec<String>)>,
    classifier: &SourceClassifier,
    required: &HashSet<PathBuf>,
    droppable: &[PathBuf],
) -> io::Result<Vec<Dropped>> {
    let generated: HashSet<PathBuf> = sysroot::read_generated(&opts.directory)?
        .into_iter()
        .map(|entry| PathBuf::from(entry.path))
        .collect();
    let mut kept: HashSet<PathBuf> = explicit_include_paths(opts);
    kept.extend(
        opts.primary
            .as_deref()
            .or(opts.target.as_deref())
            .map(|t| t.strip_prefix(&opts.directory).unwrap_or(t).to_path_buf()),
    );
    kept.extend(
        opts.entrypoint_selections
            .iter()
            .filter_map(|e| e.path.clone()),
    );
    let mut gone: HashSet<String> = HashSet::new();
    let mut dropped: Vec<Dropped> = Vec::new();
    loop {
        let mut progressed = false;
        for (object, libs) in findings.iter() {
            let rel = object.strip_prefix(&opts.directory).unwrap_or(object);
            if kept.contains(rel)
                || !generated.contains(rel)
                || required.iter().any(|dest| rel.starts_with(dest))
                || (!droppable.is_empty() && !droppable.iter().any(|dest| rel.starts_with(dest)))
            {
                continue;
            }
            // One deliberate removal among the unmet needs keeps the
            // object, so the conflict surfaces instead of hiding behind
            // an omission of the object that names it.
            let condemned = |s: &str| {
                gone.contains(s)
                    || (!recipe_excludes(s, &opts.exclude) && classifier.classify(s).omittable())
            };
            if !libs.iter().all(|s| condemned(s)) {
                continue;
            }
            let Some(soname) = libs.iter().find(|s| condemned(s)) else {
                continue;
            };
            // What goes with it: the file's own name, and the soname
            // its dependants actually ask for. A library is dropped as
            // `libfoo.so.1.2.3` and needed as `libfoo.so.1`, so
            // recording only the former leaves everything that needed it
            // looking satisfied, and the failure surfaces later as an
            // unmet need nothing can explain.
            let mut names: Vec<String> = object
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .into_iter()
                .chain(crate::bundle::elf::parse_soname(object))
                .collect();
            names.dedup();
            // A link beside an earlier removal in this same pass may
            // have gone with it already: what is gone needs neither
            // removal nor report.
            if let Err(e) = fs::symlink_metadata(object) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(e);
            }
            fs::remove_file(object)?;
            // Restricted removals leave sibling links to the sweep
            // below: it alone knows what the protections and the
            // ledger keep.
            if droppable.is_empty() {
                remove_dangling_links_beside(object);
            }
            gone.extend(names.iter().cloned());
            dropped.push(Dropped {
                object: rel.to_path_buf(),
                names,
                need: soname.clone(),
                verdict: classifier.classify(soname),
            });
            progressed = true;
        }
        if !progressed {
            break;
        }
        *findings = audit();
    }
    if !droppable.is_empty() {
        loop {
            let before = dropped.len();
            sweep_dangling_aliases(opts, droppable, &kept, required, &generated, &mut dropped)?;
            if dropped.len() == before {
                break;
            }
        }
    }
    dropped.sort_by(|a, b| (&a.object, &a.need).cmp(&(&b.object, &b.need)));
    Ok(dropped)
}

/// After the omission pass, a deduplicated alias in a carried
/// directory can dangle behind the file it duplicated: the need that
/// condemned the original was satisfied beside the alias, so the alias
/// never had a finding of its own, and the original's removal left it
/// pointing at nothing. A member that cannot load is omitted all the
/// same: a generated link that named a removed object goes, matched by
/// the exact tree path it points at, and the omission is reported for
/// the need that took its target. A link that dangled for some other
/// reason is the publisher's, and stays, and what an entrypoint names,
/// an explicit include, or a required destination needs stays too,
/// exactly as in the pass above.
fn sweep_dangling_aliases(
    opts: &BundleOptions,
    droppable: &[PathBuf],
    kept: &HashSet<PathBuf>,
    required: &HashSet<PathBuf>,
    generated: &HashSet<PathBuf>,
    dropped: &mut Vec<Dropped>,
) -> io::Result<()> {
    // The tree root's spelling varies (`app`, `./app`, an absolute
    // path), so both sides of the target comparison resolve to real
    // absolute paths before the tree-relative remainder is taken.
    let root = fs::canonicalize(&opts.directory).unwrap_or_else(|_| opts.directory.clone());
    for dest in droppable {
        for entry in jwalk::WalkDir::new(opts.directory.join(dest))
            .skip_hidden(false)
            .sort(true)
        {
            let Ok(entry) = entry else { continue };
            let link = entry.file_type().is_symlink();
            let path = entry.path().to_path_buf();
            if !link || path.exists() {
                continue;
            }
            let rel = path
                .strip_prefix(&opts.directory)
                .unwrap_or(&path)
                .to_path_buf();
            if kept.contains(&rel)
                || required.iter().any(|dest| rel.starts_with(dest))
                || !generated.contains(&rel)
            {
                continue;
            }
            let Ok(target) = fs::read_link(&path) else {
                continue;
            };
            // Where the link actually points, lexically from its own
            // real directory: a dedup alias names its canonical copy's
            // tree path, and only that exact object's omission takes
            // the alias with it. A link outside the tree names nothing
            // the pass removed, and stays.
            let Ok(base) = fs::canonicalize(path.parent().unwrap_or(Path::new("."))) else {
                continue;
            };
            let joined = base.join(target);
            let mut resolved = PathBuf::new();
            for comp in joined.components() {
                match comp {
                    std::path::Component::ParentDir => {
                        resolved.pop();
                    }
                    std::path::Component::CurDir => {}
                    comp => resolved.push(comp.as_os_str()),
                }
            }
            let Ok(target_rel) = resolved.strip_prefix(&root) else {
                continue;
            };
            let Some(prior) = dropped.iter().find(|d| d.object == target_rel) else {
                continue;
            };
            let Some(alias) = path.file_name().and_then(|n| n.to_str().map(String::from)) else {
                continue;
            };
            let need = prior.need.clone();
            let verdict = prior.verdict.clone();
            fs::remove_file(&path)?;
            dropped.push(Dropped {
                object: rel,
                names: vec![alias],
                need,
                verdict,
            });
        }
    }
    Ok(())
}

/// The `libfoo.so -> libfoo.so.1 -> libfoo.so.1.2` chains a dropped
/// library leaves behind point nowhere and are removed with it. Only
/// links that led to the dropped object go: a link that dangled for
/// some other reason is the publisher's, and stays.
pub(crate) fn remove_dangling_links_beside(object: &Path) {
    let (Some(dir), Some(name)) = (object.parent(), object.file_name()) else {
        return;
    };
    let mut gone: HashSet<std::ffi::OsString> = HashSet::from([name.to_os_string()]);
    loop {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut removed = false;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_symlink() || path.exists() {
                continue;
            }
            let Ok(target) = fs::read_link(&path) else {
                continue;
            };
            let led_here = target.parent().is_none_or(|p| p.as_os_str().is_empty())
                && target.file_name().is_some_and(|t| gone.contains(t));
            if led_here && fs::remove_file(&path).is_ok() {
                gone.insert(entry.file_name());
                removed = true;
            }
        }
        if !removed {
            return;
        }
    }
}

/// Whether the host may supply `soname` to a host-scan build: the libc
/// family a launch weighs against the host's copy, the driver stacks that
/// must match the running kernel, and the names the build leaves to the
/// host by its default or recipe exclusions. A strict host-scan build
/// reports such a need and does not fail on it.
fn host_platform_provider(soname: &str) -> bool {
    onelf_format::resolve::is_libc_family(soname)
        || libc_family_of_soname(soname).is_some()
        || onelf_format::drivers::DRIVER_FAMILIES
            .iter()
            .any(|prefix| soname.starts_with(prefix))
        || DEFAULT_EXCLUDES.iter().any(|p| soname.starts_with(p))
}

/// Whether the recipe's `[bundle] exclude` leaves `soname` out. That is
/// policy: the runtime takes only host-platform providers from the host,
/// so an excluded library a retained object needs is a conflict, never a
/// need the host is trusted to meet.
fn recipe_excludes(soname: &str, exclude: &[String]) -> bool {
    !host_platform_provider(soname) && exclude.iter().any(|p| soname.starts_with(p.as_str()))
}

/// The symlink members below the inferred destinations. Same-content
/// members are deduplicated into alias links, which the regular-file
/// audit cannot see, yet an alias loads or fails by its target's needs
/// exactly as the copied member it stands for would, so the omission
/// pass must hear about it in its own place. A dangling link names a
/// target nothing can audit, and is left to the walk that made it.
fn inferred_aliases(opts: &BundleOptions, inferred: &[PathBuf]) -> Vec<PathBuf> {
    let mut aliases = Vec::new();
    for dest in inferred {
        for entry in jwalk::WalkDir::new(opts.directory.join(dest))
            .skip_hidden(false)
            .sort(true)
        {
            let Ok(entry) = entry else { continue };
            let link = entry.file_type().is_symlink();
            let path = entry.path();
            if link && path.exists() {
                aliases.push(path.to_path_buf());
            }
        }
    }
    aliases
}

/// Audit the assembled tree for objects whose needs nothing in the
/// bundle satisfies where the loader will look.
///
/// A strict build, a sysroot build or a high-level host scan, fails on
/// an unresolved required need. Its auto-selected objects left
/// unloadable by the classification are omitted first, with the
/// recorded reason, and the audit runs again over the survivors. What
/// the classification cannot omit stays: the entrypoints, a publisher's
/// file, an explicitly included object, what a required descriptor
/// group placed, and any object whose condemning need is a deliberate
/// policy or trace removal. Those fail the build, and the error names
/// the object, the need, the entrypoints that reach it, and the package
/// involved. In a host scan a need the host platform may supply, see
/// [`host_platform_provider`], is reported and never fails or condemns.
///
/// Without `strict` a host scan only warns: the low-level `bundle-libs`
/// contract. The one exception is `inferred`, the destinations of the
/// inferred directories the build carried: inferred directories are
/// optional whatever the audit's strictness, so a member of one whose
/// needs nothing satisfies is omitted and reported, while the rest of
/// its directory is still carried. What a set-only build's pinned sets
/// ship, read from their builds, is neither: it is reported as theirs,
/// since the launch fetches it.
fn verify_needs(
    opts: &BundleOptions,
    platform: Option<&PlatformLine>,
    set_sonames: &HashSet<String>,
    classifier: &SourceClassifier,
    required: &HashSet<PathBuf>,
    inferred: &[PathBuf],
) -> io::Result<()> {
    let strict = opts.sysroot.is_some() || opts.strict;
    // Only a non-strict scan audits the inferred aliases: a strict
    // build's audit is unrestricted and its behavior must not shift,
    // while the alias members the walk deduplicated into symlinks are
    // invisible to the regular-file walk yet load or fail by their
    // targets' needs exactly as a copied member would.
    let follow = if strict {
        Vec::new()
    } else {
        inferred_aliases(opts, inferred)
    };
    let mut findings = audit_unbundled_needs(&opts.directory, &opts.lib_dir, &follow);
    let mut set_report = None;
    let mut set_provided: HashSet<String> = HashSet::new();
    if opts.sysroot.is_none() && !opts.sets.is_empty() {
        let (provides, unchecked) = sysroot::take_set_provides(&opts.sets, &mut findings);
        set_provided.extend(provides.iter().flat_map(|p| p.sonames.iter().cloned()));
        set_report = Some((provides, unchecked));
    }
    let open = |s: &str| {
        !platform.is_some_and(|p| p.matches_soname(s))
            && !set_sonames.contains(s)
            && !set_provided.contains(s)
    };
    let host_allowed = |s: &str| opts.sysroot.is_none() && host_platform_provider(s);
    let condemnable = |findings: &mut Vec<(PathBuf, Vec<String>)>| {
        for (_, libs) in findings.iter_mut() {
            libs.retain(|s| open(s) && !host_allowed(s));
        }
        findings.retain(|(_, libs)| !libs.is_empty());
    };
    let audit = || {
        let mut findings = audit_unbundled_needs(&opts.directory, &opts.lib_dir, &follow);
        condemnable(&mut findings);
        findings
    };
    for (_, libs) in &mut findings {
        libs.retain(|s| open(s));
    }
    findings.retain(|(_, libs)| !libs.is_empty());
    let mut dropped = Vec::new();
    if strict {
        let mut failing = findings.clone();
        condemnable(&mut failing);
        dropped = drop_unloadable(opts, &mut failing, audit, classifier, required, &[])?;
        if !dropped.is_empty() {
            findings = audit_unbundled_needs(&opts.directory, &opts.lib_dir, &follow);
            for (_, libs) in &mut findings {
                libs.retain(|s| open(s));
            }
            findings.retain(|(_, libs)| !libs.is_empty());
            report_dropped(&dropped);
        }
    } else if !inferred.is_empty() {
        let mut failing = findings.clone();
        condemnable(&mut failing);
        let omitted = drop_unloadable(opts, &mut failing, audit, classifier, required, inferred)?;
        if !omitted.is_empty() {
            findings = audit_unbundled_needs(&opts.directory, &opts.lib_dir, &follow);
            for (_, libs) in &mut findings {
                libs.retain(|s| open(s));
            }
            findings.retain(|(_, libs)| !libs.is_empty());
            report_dropped(&omitted);
        }
    }
    if let Some((provides, unchecked)) = &set_report {
        sysroot::report_set_provides(provides, unchecked);
    }
    report_unbundled_needs(&findings);
    if !strict {
        return Ok(());
    }
    let mut failing = findings;
    condemnable(&mut failing);
    let Some((object, libs)) = first_failure(opts, &failing) else {
        return Ok(());
    };
    let rel = object.strip_prefix(&opts.directory).unwrap_or(object);
    let shown = rel.display();
    let soname = &libs[0];
    let verdict = classifier.classify(soname);
    let explicit = explicit_include_paths(opts).contains(rel);
    let omitted = dropped.iter().find(|d| d.names.iter().any(|n| n == soname));
    let why = if let Some(d) = omitted {
        format!(
            "{shown} needs {soname}, which the build omitted because {} needs {}: {}",
            d.object.display(),
            d.need,
            d.verdict.reason()
        )
    } else if explicit {
        format!(
            "{shown} is explicitly included and needs {soname}: {}; an explicit include \
             cannot be dropped",
            verdict.reason()
        )
    } else if let SourceVerdict::Pruned { owner } = &verdict {
        format!(
            "{shown} needs {soname}, which {owner} holds but the policy or a trace \
             removed; the conflict stops the build rather than dropping the object or \
             restoring the file"
        )
    } else if recipe_excludes(soname, &opts.exclude) {
        format!(
            "{shown} needs {soname}, which [bundle] exclude leaves out; the runtime takes \
             only drivers and the libc family from the host, so the object could not load. \
             Remove the exclusion, or exclude the object that needs it"
        )
    } else if opts.sysroot.is_none() {
        format!(
            "{shown} needs {soname}, which neither the tree, the host libraries the scan \
             searched, a host-platform provider, nor a selected set supplies; add it with \
             --search-path or [bundle] search-paths"
        )
    } else if matches!(verdict, SourceVerdict::Absent) {
        format!(
            "{shown} needs {soname}, which is neither in the sysroot closure, on the \
             platform line, nor in a shared set"
        )
    } else {
        format!("{shown} needs {soname}: {}", verdict.reason())
    };
    let reached = entrypoints_reaching(opts, rel, soname);
    let by = if reached.is_empty() {
        String::new()
    } else {
        format!(" (entrypoint {})", reached.join(", "))
    };
    let more = match failing.len() {
        1 => String::new(),
        n => format!(" ({} more listed above)", n - 1),
    };
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{why}{by}{more}"),
    ))
}

/// Record what the drop pass omitted, one optional omission per object.
fn report_dropped(dropped: &[Dropped]) {
    for d in dropped {
        completeness::record(Finding::new(
            d.object.display().to_string(),
            Outcome::OptionalOmission,
            format!("omitted, needs {} ({})", d.need, d.verdict.reason()),
        ));
    }
}

/// The names of the build's entrypoints whose statically known closure
/// holds `object` asking for `soname`, sorted, so a failure names who is
/// affected. Read only on the failure path.
fn entrypoints_reaching(opts: &BundleOptions, object: &Path, soname: &str) -> Vec<String> {
    let lib_root = opts.directory.join(&opts.lib_dir);
    let mut names: Vec<String> = opts
        .entrypoint_selections
        .iter()
        .filter(|e| {
            e.path.as_ref().is_some_and(|path| {
                entrypoint_requirements(&opts.directory, &lib_root, &opts.directory.join(path))
                    .iter()
                    .any(|(o, s)| Path::new(o) == object && s == soname)
            })
        })
        .map(|e| format!("'{}'", e.name))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The finding the failure names: an explicit include's own finding
/// outranks the rest, since dropping what the author explicitly asked
/// for is the one outcome the build never accepts silently. Within
/// each rank the audit's order stands.
fn first_failure<'a>(
    opts: &BundleOptions,
    findings: &'a [(PathBuf, Vec<String>)],
) -> Option<&'a (PathBuf, Vec<String>)> {
    let explicit = explicit_include_paths(opts);
    let mut ranked: Vec<&(PathBuf, Vec<String>)> = findings.iter().collect();
    ranked.sort_by_key(|(object, _)| {
        let rel = object.strip_prefix(&opts.directory).unwrap_or(object);
        !explicit.contains(rel)
    });
    ranked.into_iter().next()
}

/// Findings the provider audit reported. Bounded: the failure shows the
/// first few and counts the rest, so a broken recipe cannot flood the
/// terminal.
const SHOWN_PROVIDER_FINDINGS: usize = 8;

/// Providers listed per collision finding before the rest are counted.
const SHOWN_COLLIDING_PROVIDERS: usize = 3;

/// One entrypoint the assembly audit covers: the name the manifest
/// gives it, the executable in the tree its requirements root at, and
/// the sets its recipe selects.
///
/// `path` is `None` when the audit covers the selection alone. The
/// requirements of the entrypoint's objects are then not audited,
/// because nothing roots them; the launch's provider view and its
/// simultaneous-provider collisions still are, and a build can ask for
/// that much before it has written anything, from inputs that exist
/// before the assembly starts.
#[derive(Debug, Clone)]
pub struct AuditedEntrypoint {
    /// The name the manifest gives the entrypoint.
    pub name: String,
    /// The entrypoint's executable, relative to the AppDir, when its
    /// requirements are audited.
    pub path: Option<PathBuf>,
    /// The normalized selection the recipe declares for it.
    pub selection: recipe::SetSelection,
}

/// Audit the required providers and the simultaneous set providers of
/// every entrypoint given, against the assembled tree. Ok means every
/// entrypoint's needs resolve inside the tree, the sonames its selected
/// sets verifiably ship, or the permitted host closure, and that no two
/// of its selected sets export one loader-namespace name with different
/// content. The audit reads the tree and the providers and writes
/// nothing, so a caller may run it before the assembly touches the
/// AppDir and fail without leaving a trace.
///
/// The audit is per entrypoint and never across them: a provider a
/// selection does not name satisfies nothing, however many other
/// entrypoints select it, and two selections that name different
/// providers of one soname never conflict. Selected providers whose
/// shared export hashes the same are one provider and deduplicate
/// without a finding. A set exporting no soname, a private plugin say,
/// contributes no export, so matching basenames across sets are never a
/// collision. An object the tree itself carries outranks every set, and
/// its satisfying the need ends the question.
///
/// A need the selection cannot supply is a failure naming the
/// entrypoint, the requesting object, the soname, and, when some
/// declared but unselected set exports that soname, that set. Two
/// selected providers of one soname whose exports hash differently is a
/// failure naming the entrypoint, the soname, and one provider from
/// each side of the disagreement, with their pins.
pub fn audit_entrypoint_providers(
    directory: &Path,
    lib_dir: &Path,
    entrypoints: &[AuditedEntrypoint],
    providers: &[sysroot::SetProvider],
    host: &dyn Fn(&str) -> bool,
) -> io::Result<()> {
    let mut ordered: Vec<&AuditedEntrypoint> = entrypoints.iter().collect();
    ordered.sort_by(|a, b| a.name.cmp(&b.name));
    let mut findings: Vec<String> = Vec::new();
    for entrypoint in ordered {
        match selected_providers(&entrypoint.selection, providers) {
            Ok(selected) => {
                findings.extend(provider_collisions(&entrypoint.name, &selected));
                if let Some(path) = &entrypoint.path {
                    findings.extend(unsatisfied_requirements(
                        directory,
                        lib_dir,
                        path,
                        &entrypoint.name,
                        &selected,
                        providers,
                        host,
                    ));
                }
            }
            Err(name) => findings.push(format!(
                "entrypoint '{}': the selection names set {name}, which the recipe \
                 does not declare",
                entrypoint.name
            )),
        }
    }
    findings.sort();
    findings.dedup();
    if findings.is_empty() {
        return Ok(());
    }
    let mut message = String::from("provider audit failed:");
    for finding in findings.iter().take(SHOWN_PROVIDER_FINDINGS) {
        message.push_str("\n  ");
        message.push_str(finding);
    }
    let hidden = findings.len().saturating_sub(SHOWN_PROVIDER_FINDINGS);
    if hidden > 0 {
        message.push_str(&format!("\n  and {hidden} more findings"));
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, message))
}

/// Whether some entrypoint of the build selects the set `name`. A build
/// without a resolved selection table, a bare `bundle-libs`, selects
/// every set it declares.
fn selected_by_an_entrypoint(opts: &BundleOptions, name: &str) -> bool {
    opts.entrypoint_selections.is_empty()
        || opts
            .entrypoint_selections
            .iter()
            .any(|e| match &e.selection {
                recipe::SetSelection::All => true,
                recipe::SetSelection::Names(names) => names.iter().any(|n| n == name),
            })
}

/// The declared providers a selection names, in canonical order by set
/// name. A name the recipe never declares is the error, since a launch
/// would refuse the selection for the same reason.
fn selected_providers<'a>(
    selection: &recipe::SetSelection,
    providers: &'a [sysroot::SetProvider],
) -> Result<Vec<&'a sysroot::SetProvider>, String> {
    match selection {
        recipe::SetSelection::All => Ok(providers.iter().collect()),
        recipe::SetSelection::Names(names) => {
            let mut selected = Vec::with_capacity(names.len());
            for name in names {
                match providers.iter().find(|p| &p.set == name) {
                    Some(provider) => selected.push(provider),
                    None => return Err(name.clone()),
                }
            }
            selected.sort_by(|a, b| a.set.cmp(&b.set));
            Ok(selected)
        }
    }
}

/// The collision findings one entrypoint's simultaneous providers make:
/// sonames exported by two or more of them whose exports hash
/// differently. Exports that hash the same are the same bytes however
/// many declarations repeat them, and deduplicate silently; the pins in
/// a finding name the builds whose bytes differ.
fn provider_collisions(entrypoint: &str, selected: &[&sysroot::SetProvider]) -> Vec<String> {
    let mut by_export: std::collections::BTreeMap<
        &str,
        Vec<(&sysroot::SetProvider, &sysroot::SetExport)>,
    > = std::collections::BTreeMap::new();
    for provider in selected {
        for export in &provider.exports {
            by_export
                .entry(export.soname.as_str())
                .or_default()
                .push((provider, export));
        }
    }
    let mut findings = Vec::new();
    for (soname, mut exports) in by_export {
        // One group per content hash, so a finding names a set from
        // each side of the disagreement and identical providers stay
        // out of it.
        exports.sort_by(|a, b| (&a.1.blake3, &a.0.set).cmp(&(&b.1.blake3, &b.0.set)));
        let mut groups: Vec<(&[u8; 32], &sysroot::SetProvider)> = Vec::new();
        for (provider, export) in &exports {
            match groups.last() {
                Some((hash, _)) if *hash == &export.blake3 => {}
                _ => groups.push((&export.blake3, provider)),
            }
        }
        if groups.len() < 2 {
            continue;
        }
        let listed: Vec<String> = groups
            .iter()
            .take(SHOWN_COLLIDING_PROVIDERS)
            .map(|(_, p)| format!("{} (blake3 {})", p.set, short_pin(&p.pin)))
            .collect();
        let more = groups.len().saturating_sub(SHOWN_COLLIDING_PROVIDERS);
        let extra = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        findings.push(format!(
            "entrypoint '{entrypoint}': {soname} is exported with different bytes by \
             sets {}{extra}, so they cannot be selected together",
            listed.join(" and ")
        ));
    }
    findings
}

/// A pin a finding can name: the head of the digest and its length, so
/// two pins that differ in the head still read apart.
fn short_pin(pin: &str) -> String {
    let head: String = pin.chars().take(12).collect();
    format!("{head}..({} chars)", pin.chars().count())
}

/// The requirements one entrypoint's executable and the objects it
/// reaches ask for and the tree cannot resolve: every `DT_NEEDED` that
/// names neither the object's own directory under the library root, the
/// library directory, the `../lib` beside the object, a run path inside
/// the tree, nor the dynamic loader, with the object that asked for it.
/// Following a resolved need visits the object it named, so the closure
/// is the entrypoint's own, never the whole tree's, which is what keeps
/// one entrypoint's selection from answering for another's needs.
fn entrypoint_requirements(
    directory: &Path,
    lib_root: &Path,
    entry: &Path,
) -> Vec<(String, String)> {
    // Each directory is listed once per audit, however many objects
    // resolve against it.
    let mut listings: HashMap<PathBuf, std::rc::Rc<HashSet<String>>> = HashMap::new();
    let mut listed = |dir: &Path| -> std::rc::Rc<HashSet<String>> {
        listings
            .entry(dir.to_path_buf())
            .or_insert_with(|| std::rc::Rc::new(names_in(dir)))
            .clone()
    };
    let lib_names = listed(lib_root);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut queue: std::collections::VecDeque<PathBuf> =
        std::collections::VecDeque::from([entry.to_path_buf()]);
    let mut requirements: Vec<(String, String)> = Vec::new();
    while let Some(object) = queue.pop_front() {
        let Ok(real) = fs::canonicalize(&object) else {
            continue;
        };
        if !seen.insert(real.clone()) {
            continue;
        }
        let Ok(needed) = parse_needed(&real) else {
            continue;
        };
        let parent = real.parent().unwrap_or(directory).to_path_buf();
        // An object under the library directory resolves against its own
        // directory, the way its siblings do; elsewhere, the `../lib`
        // beside the object is the convention the bundled layout keeps.
        let parent_names = if parent.starts_with(lib_root) {
            listed(&parent)
        } else {
            std::rc::Rc::default()
        };
        let origin_names = listed(&parent.join("../lib"));
        let mut run_dirs: Vec<(PathBuf, std::rc::Rc<HashSet<String>>)> =
            parse_rpaths_under(&real, Some(directory))
                .into_iter()
                .filter(|dir| dir.starts_with(directory))
                .map(|dir| {
                    let names = listed(&dir);
                    (dir, names)
                })
                .collect();
        run_dirs.sort_by(|a, b| a.0.cmp(&b.0));
        for soname in needed {
            let bare = Path::new(&soname)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(soname.as_str());
            if is_dynamic_loader(bare) {
                continue;
            }
            let found = if parent_names.contains(bare) {
                Some(parent.join(bare))
            } else if lib_names.contains(bare) {
                Some(lib_root.join(bare))
            } else if origin_names.contains(bare) {
                Some(parent.join("../lib").join(bare))
            } else {
                run_dirs
                    .iter()
                    .find(|(_, names)| names.contains(bare))
                    .map(|(dir, _)| dir.join(bare))
            };
            match found {
                Some(path) => queue.push_back(path),
                None => {
                    let rel = object
                        .strip_prefix(directory)
                        .unwrap_or(&object)
                        .to_string_lossy()
                        .into_owned();
                    requirements.push((rel, soname));
                }
            }
        }
    }
    requirements.sort();
    requirements.dedup();
    requirements
}

/// The file names a directory resolves to: an entry whose target is
/// gone satisfies nothing, so a dangling soname link reads as absent,
/// the way the loader sees it.
fn names_in(dir: &Path) -> HashSet<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().exists())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect()
}

/// The findings for one entrypoint's requirements: a need the tree
/// cannot resolve and its selection cannot supply is one line naming
/// the entrypoint, the requesting object, the soname, and what was
/// found instead, which is a declared set that ships the soname the
/// selection left out, when there is one.
fn unsatisfied_requirements(
    directory: &Path,
    lib_dir: &Path,
    entry: &Path,
    entrypoint: &str,
    selected: &[&sysroot::SetProvider],
    providers: &[sysroot::SetProvider],
    host: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let resolved = directory.join(entry);
    if !resolved.is_file() {
        return vec![format!(
            "entrypoint '{entrypoint}': {} is not in the tree",
            entry.display()
        )];
    }
    let lib_root = directory.join(lib_dir);
    let requirements = entrypoint_requirements(directory, &lib_root, &resolved);
    let ships = |p: &sysroot::SetProvider, soname: &str| p.provides.iter().any(|s| s == soname);
    let mut findings = Vec::new();
    for (object, soname) in requirements {
        if selected.iter().any(|p| ships(p, &soname)) || host(&soname) {
            continue;
        }
        let mut owners: Vec<&sysroot::SetProvider> =
            providers.iter().filter(|p| ships(p, &soname)).collect();
        owners.sort_by(|a, b| a.set.cmp(&b.set));
        let why = if owners.is_empty() {
            "which nothing in the application, its selection, or the host line \
             provides"
                .to_string()
        } else {
            let names: Vec<&str> = owners.iter().map(|p| p.set.as_str()).collect();
            format!(
                "which {} exports and the entrypoint does not select",
                names.join(" and ")
            )
        };
        findings.push(format!(
            "entrypoint '{entrypoint}': {object} needs {soname}: {why}"
        ));
    }
    findings
}

/// Audit a build's declared sets before the assembly writes anything.
///
/// What the recipe declares is what every entrypoint launches with when
/// its selection list is omitted, so two declared sets exporting one
/// soname with different pins stop the build here, with both pins
/// named, instead of writing a tree whose launch would refuse the same
/// pair. A set whose exports cannot be verified claims nothing here and
/// keeps its existing reporting, so a build that cannot reach a pinned
/// build fails, when it fails, for the reason it always did. The
/// entrypoints' own selections are audited later, against the assembled
/// tree, by [`audit_entrypoint_selections`].
fn audit_declared_sets(opts: &BundleOptions, providers: &[sysroot::SetProvider]) -> io::Result<()> {
    let name = opts
        .primary
        .as_deref()
        .or(opts.target.as_deref())
        .and_then(|path| Path::new(path).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("main")
        .to_string();
    audit_entrypoint_providers(
        &opts.directory,
        &opts.lib_dir,
        &[AuditedEntrypoint {
            name,
            path: None,
            selection: recipe::SetSelection::All,
        }],
        providers,
        &|_| false,
    )
}

/// Audit the build's real entrypoints against the assembled tree, each
/// with its own selection.
///
/// By the time this runs, the tree holds everything the assembly copied
/// for it, which is what a launch resolves against, so an entrypoint's
/// statically known dependency edges resolve the way the loader will:
/// inside the tree, from a set its selection names, or from what the
/// host is permitted to supply, which is the platform line's sonames,
/// the libc family a launch may weigh against the host's copy, and the
/// driver stacks that must match the running kernel. A need nothing of
/// the three answers fails the build naming the entrypoint, the object
/// that asked, the soname, and, when a declared set exports it, that
/// set, so a recipe whose CLI left out the one set that carries its
/// library is refused here rather than at first launch.
///
/// A set whose exports could not be verified claims nothing, so a
/// selection resting on one fails the same way, with the set's own
/// reporting naming why. The audit reads the tree and the verified set
/// exports and writes nothing.
fn audit_entrypoint_selections(
    opts: &BundleOptions,
    platform: Option<&PlatformLine>,
    providers: &[sysroot::SetProvider],
    unchecked: &[(String, String)],
) -> io::Result<()> {
    if opts.entrypoint_selections.is_empty() {
        return Ok(());
    }
    // A set whose build could not be checked still answers its
    // selection, claiming nothing: the audit then names what the
    // selection could not supply instead of pretending the recipe
    // named a set it never declared.
    let mut audited = providers.to_vec();
    for (name, _) in unchecked {
        if audited.iter().any(|p| p.set == *name) {
            continue;
        }
        audited.push(sysroot::SetProvider {
            set: name.clone(),
            pin: String::new(),
            provides: Vec::new(),
            exports: Vec::new(),
        });
    }
    let host = |soname: &str| {
        platform.is_some_and(|p| p.matches_soname(soname))
            || onelf_format::resolve::is_libc_family(soname)
            || onelf_format::drivers::DRIVER_FAMILIES
                .iter()
                .any(|prefix| soname.starts_with(prefix))
    };
    audit_entrypoint_providers(
        &opts.directory,
        &opts.lib_dir,
        &opts.entrypoint_selections,
        &audited,
        &host,
    )
}

/// Pin a bundled file's mtime so repeated `bundle-libs` runs produce
/// metadata-identical trees (`fs::copy` otherwise stamps "now"). Honors
/// `SOURCE_DATE_EPOCH`, else the Unix epoch. Best-effort: errors ignored.
pub(crate) fn normalize_mtime(path: &Path) {
    let secs = crate::pack::source_date_epoch()
        .and_then(|s| i64::try_from(s).ok())
        .unwrap_or(0);
    let t = rustix::fs::Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    // By path rather than through an open for writing, which a read-only
    // copy refuses.
    let _ = rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &rustix::fs::Timestamps {
            last_access: t,
            last_modification: t,
        },
        rustix::fs::AtFlags::empty(),
    );
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

/// Split traced libraries into the application's and the host's GPU
/// stack, which a traced run maps on whatever machine it runs on: the
/// driver families the launch resolver leaves to the host, the Mesa and
/// vendor implementations behind them, and what the run loaded only as
/// their dependency. On Mesa that is libGLX_mesa, libgallium, libLLVM and
/// radeonsi_dri.so, on NVIDIA libGLX_nvidia. None of it is the app's to
/// ship: the driver that works is the host's, and bundling Mesa is what
/// `--gl`, `--dri` and `--vulkan` are for.
fn split_gpu_stack(libs: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    const IMPLEMENTATIONS: &[&str] = &[
        "libGLX_",
        "libEGL_",
        "libglapi.so",
        "libgallium",
        "libLLVM",
        "libvulkan_",
        "libVkLayer_",
        "libxatracker",
        "libvdpau",
    ];
    const DRIVER_SUFFIXES: &[&str] = &["_dri.so", "_drv_video.so"];
    let name = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string()
    };
    let mut gpu: HashSet<String> = libs
        .iter()
        .map(|p| name(p))
        .filter(|n| {
            onelf_format::drivers::DRIVER_FAMILIES
                .iter()
                .chain(IMPLEMENTATIONS)
                .any(|f| n.starts_with(f))
                || DRIVER_SUFFIXES.iter().any(|s| n.ends_with(s))
        })
        .collect();
    let needs: Vec<(String, Vec<String>)> = libs
        .iter()
        .map(|p| (name(p), parse_needed(p).unwrap_or_default()))
        .collect();
    // A library only GPU libraries asked for came with them. One nothing
    // traced asked for was opened by the application itself.
    loop {
        let more: Vec<String> = needs
            .iter()
            .map(|(n, _)| n)
            .filter(|n| !gpu.contains(*n))
            .filter(|n| {
                let mut needers = needs.iter().filter(|(_, deps)| deps.contains(n)).peekable();
                needers.peek().is_some() && needers.all(|(m, _)| gpu.contains(m))
            })
            .cloned()
            .collect();
        if more.is_empty() {
            break;
        }
        gpu.extend(more);
    }
    libs.into_iter().partition(|p| !gpu.contains(&name(p)))
}

/// The loader to start `target` under for a traced run, when the one its
/// `PT_INTERP` names is not on this machine. A tree built elsewhere names
/// `/lib/ld-musl-x86_64.so.1` on a glibc host, say, and the kernel cannot
/// start it at all. The loader is looked for by name in `dirs`, as musl's
/// libc too, since that is the same file.
fn trace_loader(
    target: &Path,
    dirs: &[PathBuf],
    class: Option<u8>,
    machine: Option<u16>,
) -> io::Result<Option<PathBuf>> {
    let Some(interp) = parse_interp(target) else {
        return Ok(None);
    };
    if Path::new(&interp).exists() {
        return Ok(None);
    }
    let Some(name) = Path::new(&interp).file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    let mut names = vec![name.to_string()];
    names.extend(libc_alias_for(name));
    if libc_family_from_interp(name) == Some(LibcFamily::Musl) {
        names.push("libc.so".to_string());
    }
    let found = dirs
        .iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|c| {
            c.is_file()
                && read_elf_class(c).is_some_and(|k| class.is_none_or(|t| k == t))
                && read_elf_machine(c).is_some_and(|m| machine.is_none_or(|t| m == t))
        });
    match found {
        Some(loader) => Ok(Some(loader)),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "--trace-run cannot start {}: its interpreter {interp} is not on this \
                 machine; pass --search-path with a directory that holds it",
                target.display()
            ),
        )),
    }
}

/// Absolute path of the binary that decides architecture and libc family.
///
/// `--target` implies it, since scoping the scan to one binary also states
/// which one matters. Returns `None` when nothing designates one, leaving
/// the caller to fall back to the first ELF found.
fn resolve_primary(opts: &BundleOptions) -> Option<PathBuf> {
    let named = opts.primary.as_ref().or(opts.target.as_ref())?;
    let path = if named.is_absolute() {
        named.clone()
    } else {
        opts.directory.join(named)
    };
    path.is_file().then_some(path)
}

/// Keeps the paths [`exclude_from_walk`] installed out of the bundling
/// walk until it drops.
pub struct WalkExclusion {
    installed: usize,
}

impl Drop for WalkExclusion {
    fn drop(&mut self) {
        WALK_EXCLUSIONS.with(|list| {
            let mut list = list.borrow_mut();
            let keep = list.len().saturating_sub(self.installed);
            list.truncate(keep);
        });
    }
}

thread_local! {
    /// Canonical files the bundling walk leaves untouched for the
    /// lifetime of one [`WalkExclusion`]. [`find_elf_files`] filters
    /// them out of its discovery, so every pass that walks the tree,
    /// dependency resolution included, skips them.
    static WALK_EXCLUSIONS: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// Keep `paths` out of the bundling walk's ELF discovery and rewriting
/// until the returned guard drops. A build names the declared
/// `[input] binary` source, which is the author's file rather than
/// AppDir content, while the imported copy at the command's path is
/// processed like any bundled binary.
pub fn exclude_from_walk(paths: Vec<PathBuf>) -> WalkExclusion {
    let resolved: Vec<PathBuf> = paths
        .into_iter()
        .filter_map(|path| fs::canonicalize(&path).ok())
        .collect();
    let installed = resolved.len();
    WALK_EXCLUSIONS.with(|list| list.borrow_mut().extend(resolved));
    WalkExclusion { installed }
}

/// Every regular file under `dir` that starts with the ELF magic.
///
/// Symlinks are not followed: the tree is rewritten in place, and a link
/// pointing outside it would have the host's library patched. A link to a
/// file inside the tree is reached through the file itself. Paths
/// [`exclude_from_walk`] installed are not discovered, so no pass that
/// walks the tree rewrites them.
fn find_elf_files(dir: &Path) -> Vec<PathBuf> {
    let excluded = WALK_EXCLUSIONS.with(|list| list.borrow().clone());
    let root = if excluded.is_empty() {
        None
    } else {
        Some(fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
    };
    let mut result = Vec::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false).sort(true) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if let Some(root) = &root {
            let resolved = path
                .strip_prefix(dir)
                .map(|rel| root.join(rel))
                .unwrap_or_else(|_| path.to_path_buf());
            if excluded.contains(&resolved) {
                continue;
            }
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
/// A match must be a *versioned* soname, `lib<name>.so.<digit>...`, to
/// flag a framework. We deliberately do not require a NUL boundary before
/// the match: Rust's string-merging optimization packs string literals
/// together without NUL separators, so genuine dlopen sonames in Rust
/// binaries (e.g. the wgpu/khronos-egl/wayland-backend strings in
/// `amdgpu_top`) appear mid-blob like `...eglWaitSynclibEGL.so.1libEGL.so...`.
/// Requiring the version suffix is precise enough to keep those while still
/// rejecting:
/// - Unversioned soname text in prose (e.g. `"Library libwayland-client.so
///   could not be loaded."`, where the `.so` is followed by a space).
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
        } else if let Some(s) = start.take()
            && i - s >= 5
            && let Ok(text) = std::str::from_utf8(&data[s..i])
        {
            let match_builtin = DLOPEN_CANDIDATES.contains(&text);
            let match_extra = extra.iter().any(|c| c == text);
            if (match_builtin || match_extra) && !found.iter().any(|x| x == text) {
                found.push(text.to_string());
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod plugin_dir_tests {
    use super::*;

    /// A sysroot data carry whose destination differs from the source
    /// copies only the files the build admitted: a file the policy
    /// pruned under the carried directory stays out, and every carried
    /// file is recorded as the build's own output.
    #[test]
    fn a_sysroot_carry_copies_only_admitted_files() {
        let dir = std::env::temp_dir().join(format!("onelf-carry-admit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sysroot");
        let tree = dir.join("tree");
        fs::create_dir_all(&tree).unwrap();
        let files = [
            "usr/lib/libtcl8.6.so",
            "usr/share/tcltk/tcl8.6/init.tcl",
            "usr/share/tcltk/tcl8.6/encoding/ascii.enc",
        ];
        for file in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, file.as_bytes()).unwrap();
        }
        let packages = [sysroot::fixture::package("app", &files)];
        let policy_path = dir.join("policy.txt");
        fs::write(&policy_path, "usr/share/tcltk/tcl8.6/encoding/*\n").unwrap();
        let policy = Policy::load(&policy_path).unwrap();
        let universe =
            sysroot::fixture::populate_universe(&root, &packages, "app", &[], Some(&policy), None);
        let descriptor = onelf_format::catalog::builtins()
            .iter()
            .find(|d| d.id.as_str() == "tcl-8.6")
            .unwrap();
        let admitted = [AdmittedLibrary::new(
            "libtcl8.6.so",
            root.join("usr/lib/libtcl8.6.so"),
        )];
        let mut plan = DestinationPlan::load(&tree).unwrap();
        let mut actions: Vec<WalkAction> = Vec::new();
        let mut preparation = SetPreparation::new(&[], &[], Path::new("lib"), false);
        for declared in &descriptor.resources.data {
            carry_data_group(
                &tree,
                descriptor,
                declared,
                &admitted,
                Some(&universe),
                &mut plan,
                &mut actions,
                &mut preparation,
                &mut HashSet::new(),
            )
            .unwrap();
        }
        assert!(!plan.has_findings(), "{:?}", plan.findings());
        let mut outcome = WalkOutcome::default();
        execute_walk(
            &tree,
            false,
            false,
            &tree.join("lib"),
            &actions,
            &plan,
            &mut outcome,
        )
        .unwrap();
        assert!(tree.join("lib/tcl8.6/init.tcl").is_file());
        assert!(
            !tree.join("lib/tcl8.6/encoding").exists(),
            "the pruned file is not resurrected"
        );
        assert_eq!(outcome.written, vec!["lib/tcl8.6/init.tcl".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_targets_stay_inside_the_library_directory() {
        // Beside each other: the bare name.
        assert_eq!(
            link_target(Path::new("alsa-lib"), Path::new("alsa-lib/b.so")),
            "b.so"
        );
        // Flat link into a subdirectory.
        assert_eq!(
            link_target(Path::new(""), Path::new("alsa-lib/b.so")),
            "alsa-lib/b.so"
        );
        // A subdirectory link to a flat library climbs out.
        assert_eq!(
            link_target(Path::new("alsa-lib"), Path::new("b.so")),
            "../b.so"
        );
        // Flat to flat, the shape every alias had before.
        assert_eq!(link_target(Path::new(""), Path::new("b.so")), "b.so");
    }

    /// A temp tree shaped like a distribution: `usr/lib` flat, data under
    /// `usr/share`.
    fn dist_tree(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("onelf-libdata-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("usr/share/alsa/cards")).unwrap();
        fs::write(root.join("usr/share/alsa/alsa.conf"), b"conf").unwrap();
        fs::write(root.join("usr/share/alsa/cards/aliases.conf"), b"cards").unwrap();
        (root.clone(), root.join("usr/lib/libasound.so.2"))
    }

    /// Carry every data group of the rule `soname` activates, the way
    /// the planned walk does: the resolved library is the one admitted
    /// root, and the group's declared alternatives are examined in
    /// order. A soname no descriptor triggers returns without a carry.
    fn carry_soname_data(directory: &Path, soname: &str, resolved: &Path, dry_run: bool) {
        let descriptor = onelf_format::catalog::builtins()
            .iter()
            .find(|d| d.triggers.iter().any(|t| t.matches_soname(soname)));
        let Some(descriptor) = descriptor else {
            return;
        };
        let admitted = [AdmittedLibrary::new(soname, resolved)];
        let mut plan = DestinationPlan::load(directory).unwrap();
        let mut actions: Vec<WalkAction> = Vec::new();
        let mut preparation = SetPreparation::new(&[], &[], Path::new("lib"), false);
        for declared in &descriptor.resources.data {
            carry_data_group(
                directory,
                descriptor,
                declared,
                &admitted,
                None,
                &mut plan,
                &mut actions,
                &mut preparation,
                &mut HashSet::new(),
            )
            .unwrap();
        }
        assert!(!plan.has_findings(), "{:?}", plan.findings());
        execute_walk(
            directory,
            dry_run,
            false,
            &directory.join("lib"),
            &actions,
            &plan,
            &mut WalkOutcome::default(),
        )
        .unwrap();
    }

    #[test]
    fn a_carried_library_brings_its_data() {
        let (root, lib) = dist_tree("carry");
        let tree = root.join("tree");
        fs::create_dir_all(tree.join("lib")).unwrap();

        carry_soname_data(&tree, "libasound.so.2", &lib, false);

        assert_eq!(
            fs::read_to_string(tree.join("share/alsa/alsa.conf")).unwrap(),
            "conf"
        );
        assert!(tree.join("share/alsa/cards/aliases.conf").is_file());

        // A second run is a no-op, not a second copy.
        let before = fs::read_to_string(tree.join("share/alsa/alsa.conf")).unwrap();
        carry_soname_data(&tree, "libasound.so.2", &lib, false);
        assert_eq!(
            fs::read_to_string(tree.join("share/alsa/alsa.conf")).unwrap(),
            before
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The carried snippet directory loads after the host's distro
    /// snippets but before the user's own config files, so a pipewire
    /// host cannot steal the output while a deliberate `asound.conf`
    /// still can.
    #[test]
    fn a_carried_alsa_conf_loads_its_snippets_between_host_and_user() {
        let work = std::env::temp_dir().join(format!("onelf-alsa-hooks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let conf = work.join("alsa.conf");
        fs::write(
            &conf,
            r##"@hooks [
	{
		func load
		files [
			"/var/lib/alsa/conf.d"
			"/usr/etc/alsa/conf.d"
			"/etc/alsa/conf.d"
			"/etc/asound.conf|||/usr/etc/asound.conf"
			"~/.asoundrc"
		]
		errors false
	}
]

cards.@hooks [
	{
		func load
		files [
			"/etc/alsa/conf.d"
		]
	}
]

pcm.default cards.pcm.default
"##,
        )
        .unwrap();

        assert!(patch_alsa_conf_hooks(&conf).unwrap());

        let out = fs::read_to_string(&conf).unwrap();
        let host = out.find("\"/etc/alsa/conf.d\"").unwrap();
        let carried = out
            .find("{ @func datadir } \"/alsa.conf.d\"")
            .expect("carried entry injected");
        let user = out.find("\"/etc/asound.conf").unwrap();
        assert!(host < carried && carried < user);
        assert!(out.contains("cards.@hooks"));
        assert_eq!(out.matches("\"/etc/alsa/conf.d\"").count(), 2);

        // A second run is a no-op.
        assert!(!patch_alsa_conf_hooks(&conf).unwrap());
        assert_eq!(fs::read_to_string(&conf).unwrap(), out);

        // A config without a hook block is left alone.
        let plain = work.join("plain.conf");
        fs::write(&plain, "pcm.default cards.pcm.default\n").unwrap();
        assert!(!patch_alsa_conf_hooks(&plain).unwrap());

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn multiarch_and_store_layouts_are_both_served() {
        // Debian: the library two levels below the one share serves.
        let root = std::env::temp_dir().join(format!("onelf-libdata-arch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let multiarch = root.join("usr/lib/x86_64-linux-gnu");
        fs::create_dir_all(&multiarch).unwrap();
        fs::create_dir_all(root.join("usr/share/alsa/cards")).unwrap();
        fs::write(root.join("usr/share/alsa/alsa.conf"), b"conf").unwrap();
        fs::write(root.join("usr/share/alsa/cards/aliases.conf"), b"cards").unwrap();
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();
        carry_soname_data(
            &tree,
            "libasound.so.2",
            &multiarch.join("libasound.so.2"),
            false,
        );
        assert_eq!(
            fs::read_to_string(tree.join("share/alsa/alsa.conf")).unwrap(),
            "conf"
        );
        assert!(tree.join("share/alsa/cards/aliases.conf").is_file());

        // Nix: the store path carries its own share beside lib.
        let store = root.join("nix/store/x-xkb-data");
        fs::create_dir_all(store.join("lib")).unwrap();
        fs::create_dir_all(store.join("share/X11/xkb/symbols")).unwrap();
        fs::write(store.join("share/X11/xkb/symbols/us"), b"us").unwrap();
        let tree = root.join("store-tree");
        fs::create_dir_all(&tree).unwrap();
        carry_soname_data(
            &tree,
            "libxkbcommon.so.0",
            &store.join("lib/libxkbcommon.so.0"),
            false,
        );
        assert_eq!(
            fs::read_to_string(tree.join("share/X11/xkb/symbols/us")).unwrap(),
            "us"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A required data group whose alternatives exist nowhere in the
    /// source that provided the library fails the assembly, naming the
    /// rule, the destination and the examined locations.
    #[test]
    fn required_data_the_source_lacks_fails_the_assembly() {
        let (root, lib) = dist_tree("required-miss");
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();

        let descriptor = onelf_format::catalog::builtins()
            .iter()
            .find(|d| d.triggers.iter().any(|t| t.matches_soname("libX11.so.6")))
            .unwrap();
        let admitted = [AdmittedLibrary::new("libX11.so.6", &lib)];
        let mut plan = DestinationPlan::load(&tree).unwrap();
        let mut actions: Vec<WalkAction> = Vec::new();
        let declared = descriptor.resources.data.first().unwrap();
        let mut preparation = SetPreparation::new(&[], &[], Path::new("lib"), false);
        let err = carry_data_group(
            &tree,
            descriptor,
            declared,
            &admitted,
            None,
            &mut plan,
            &mut actions,
            &mut preparation,
            &mut HashSet::new(),
        )
        .expect_err("a required data miss must fail the assembly");
        let text = err.to_string();
        assert!(text.contains("rule x11"), "{text}");
        assert!(text.contains("requires directory data"), "{text}");
        assert!(text.contains("share/X11/locale"), "{text}");
        assert!(text.contains("absent"), "{text}");
        assert!(actions.is_empty(), "{actions:?}");
        assert!(!tree.join("share").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// An optional data group the source lacks is reported and the
    /// carry goes on without it.
    #[test]
    fn optional_data_the_source_lacks_warns_and_continues() {
        let root = std::env::temp_dir().join(format!("onelf-libdata-opt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        let lib = root.join("usr/lib/libgirepository-1.0.so.1");
        fs::write(&lib, b"elf").unwrap();
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();

        let descriptor = onelf_format::catalog::builtins()
            .iter()
            .find(|d| {
                d.triggers
                    .iter()
                    .any(|t| t.matches_soname("libgirepository-1.0.so.1"))
            })
            .unwrap();
        let admitted = [AdmittedLibrary::new("libgirepository-1.0.so.1", &lib)];
        let mut plan = DestinationPlan::load(&tree).unwrap();
        let mut actions: Vec<WalkAction> = Vec::new();
        let declared = descriptor.resources.data.first().unwrap();
        let mut preparation = SetPreparation::new(&[], &[], Path::new("lib"), false);
        carry_data_group(
            &tree,
            descriptor,
            declared,
            &admitted,
            None,
            &mut plan,
            &mut actions,
            &mut preparation,
            &mut HashSet::new(),
        )
        .unwrap();
        assert!(actions.is_empty(), "{actions:?}");
        assert!(!plan.has_findings());
        assert!(!tree.join("lib/girepository-1.0").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_owning_library_triggers_the_carry() {
        let (root, lib) = dist_tree("owner");
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();

        // The x11 variant links libxkbcommon but owns no data; the soname
        // prefix must not match it.
        carry_soname_data(&tree, "libxkbcommon-x11.so.0", &lib, false);
        assert!(!tree.join("share/X11").exists());

        // An unrelated library carries nothing either.
        carry_soname_data(&tree, "libpulse.so.0", &lib, false);
        assert!(!tree.join("share/alsa").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// A tree that already carries `share/alsa` from a run before the
    /// hook patch existed still gets its `alsa.conf` anchored, and only
    /// once.
    #[test]
    fn an_existing_carried_alsa_conf_still_gets_its_hooks_patched() {
        let (root, lib) = dist_tree("existing-patch");
        let tree = root.join("tree");
        let alsa = tree.join("share/alsa");
        fs::create_dir_all(alsa.join("alsa.conf.d")).unwrap();
        fs::write(
            alsa.join("alsa.conf"),
            "@hooks [\n\t{ func load files [ \"/etc/alsa/conf.d\" ] }\n]\n",
        )
        .unwrap();

        carry_soname_data(&tree, "libasound.so.2", &lib, false);

        let out = fs::read_to_string(alsa.join("alsa.conf")).unwrap();
        assert!(out.contains("{ @func datadir } \"/alsa.conf.d\""));

        // Nothing is rewritten again, and nothing else is copied over it.
        let before = out;
        carry_soname_data(&tree, "libasound.so.2", &lib, false);
        assert_eq!(fs::read_to_string(alsa.join("alsa.conf")).unwrap(), before);
        let _ = fs::remove_dir_all(&root);
    }

    /// libmagic's compiled database is a file, not a directory, and it
    /// lands at the one destination the declaration and the runtime's
    /// `MAGIC` mapping name, whichever alternative served it.
    #[test]
    fn a_carried_magic_database_is_the_file_the_runtime_names() {
        let root = std::env::temp_dir().join(format!("onelf-libdata-mg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("usr/share/file/misc")).unwrap();
        fs::write(root.join("usr/share/file/misc/magic.mgc"), b"compiled").unwrap();
        let lib = root.join("usr/lib/libmagic.so.1");
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();

        carry_soname_data(&tree, "libmagic.so.1", &lib, false);

        assert_eq!(
            fs::read(tree.join("share/misc/magic.mgc")).unwrap(),
            b"compiled"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The declared alternatives are ordered: the first that exists on
    /// the admitted root is the one taken, and a later alternative is
    /// never examined once one is found.
    #[test]
    fn the_first_existing_data_alternative_wins() {
        let root = std::env::temp_dir().join(format!("onelf-libdata-alt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("usr/share/file/misc")).unwrap();
        fs::create_dir_all(root.join("usr/share/misc")).unwrap();
        fs::write(root.join("usr/share/file/misc/magic.mgc"), b"first").unwrap();
        fs::write(root.join("usr/share/misc/magic.mgc"), b"second").unwrap();
        let lib = root.join("usr/lib/libmagic.so.1");
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();

        carry_soname_data(&tree, "libmagic.so.1", &lib, false);

        assert_eq!(
            fs::read(tree.join("share/misc/magic.mgc")).unwrap(),
            b"first"
        );
        assert!(!tree.join("share/file").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// With the first alternative absent, the next one serves.
    #[test]
    fn a_later_data_alternative_serves_when_the_first_is_absent() {
        let root = std::env::temp_dir().join(format!("onelf-libdata-alt2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("usr/share/misc")).unwrap();
        fs::write(root.join("usr/share/misc/magic.mgc"), b"second").unwrap();
        let lib = root.join("usr/lib/libmagic.so.1");
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();

        carry_soname_data(&tree, "libmagic.so.1", &lib, false);

        assert_eq!(
            fs::read(tree.join("share/misc/magic.mgc")).unwrap(),
            b"second"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A plugin left flat by an older onelf moves into its subdirectory
    /// on the next run, and alias links pointing at the flat copy follow
    /// it.
    #[test]
    fn a_stale_flat_plugin_migrates_with_its_aliases() {
        let work = std::env::temp_dir().join(format!("onelf-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        let lib = work.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let flat = lib.join("libasound_module_pcm_pulse.so");
        fs::write(&flat, b"plugin").unwrap();
        // An alias from the same old run: identical bytes, another name.
        let alias = lib.join("libasound_module_pcm_pulse_alt.so");
        std::os::unix::fs::symlink("libasound_module_pcm_pulse.so", &alias).unwrap();
        // A subdirectory link to it as well.
        fs::create_dir_all(lib.join("alsa-lib")).unwrap();
        let deep = lib.join("alsa-lib/linked.so");
        std::os::unix::fs::symlink("../libasound_module_pcm_pulse.so", &deep).unwrap();
        // The walk has already written the subdirectory copy by the time
        // it migrates.
        fs::copy(&flat, lib.join("alsa-lib/libasound_module_pcm_pulse.so")).unwrap();

        migrate_flat_plugin(
            &flat,
            Path::new("lib/alsa-lib/libasound_module_pcm_pulse.so"),
            Path::new("lib"),
            &lib,
            false,
        );

        assert!(!flat.exists());
        let moved = lib.join("alsa-lib/libasound_module_pcm_pulse.so");
        assert_eq!(fs::read(&moved).unwrap(), b"plugin");
        assert_eq!(
            fs::read_link(&alias).unwrap(),
            Path::new("../lib/alsa-lib/libasound_module_pcm_pulse.so")
        );
        // Same directory now: the bare name, as link_target resolves.
        assert_eq!(
            fs::read_link(&deep).unwrap(),
            Path::new("libasound_module_pcm_pulse.so")
        );
        let _ = fs::remove_dir_all(&work);
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;
    use onelf_format::catalog;

    /// Minimal ELF-shaped bytes: magic + EI_CLASS + e_machine, enough
    /// for the walk's checks to classify a stand-in file.
    fn fake_elf() -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2;
        v[18..20].copy_from_slice(&62u16.to_le_bytes());
        v
    }

    /// The gstreamer rule's scanner declaration, the one required
    /// helper the built-in catalog carries.
    fn scanner_declaration() -> HelperGroup {
        catalog::builtins()
            .iter()
            .find(|d| d.id.as_str() == "gstreamer")
            .unwrap()
            .resources
            .helpers
            .first()
            .unwrap()
            .clone()
    }

    /// A distribution tree whose framework library sits in `usr/lib`,
    /// with `files` naming further files below it, each written as a
    /// stand-in ELF object. Returns the tree and the library's path.
    fn dist_tree(tag: &str, files: &[&str]) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("onelf-helper-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let lib = root.join("usr/lib");
        fs::create_dir_all(&lib).unwrap();
        let gstreamer = lib.join("libgstreamer-1.0.so.0");
        fs::write(&gstreamer, fake_elf()).unwrap();
        for file in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, fake_elf()).unwrap();
        }
        (root, gstreamer)
    }

    /// What one collection recorded, and the queue it fed.
    struct Collected {
        destinations: HashMap<String, PathBuf>,
        helpers: HashSet<String>,
        discovery: Discovery,
    }

    /// Collect the scanner over one admitted framework library, the way
    /// the drain loop would.
    fn collect(
        tag: &str,
        declared: &HelperGroup,
        tree: &Path,
        lib: &Path,
    ) -> io::Result<Collected> {
        let admitted = [AdmittedLibrary::new("libgstreamer-1.0.so.0", lib)];
        let mut discovery = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
        discovery.observe("libgstreamer-1.0.so.0").unwrap();
        let mut destinations = HashMap::new();
        let mut helpers = HashSet::new();
        let mut needed_by = HashMap::new();
        let mut processed = HashSet::new();
        let mut search_paths = Vec::new();
        let mut repair_extra = HashSet::new();
        let mut preparation = SetPreparation::new(&[], &[], Path::new("lib"), false);
        let mut plan = DestinationPlan::load(tree)?;
        let descriptor = catalog::builtins()
            .iter()
            .find(|d| d.id.as_str() == "gstreamer")
            .unwrap();
        collect_helper_group(
            tree,
            descriptor,
            declared,
            &admitted,
            None,
            &mut search_paths,
            &mut repair_extra,
            &mut destinations,
            &mut helpers,
            &mut needed_by,
            &mut processed,
            &mut discovery,
            &mut preparation,
            &mut plan,
            &mut HashSet::new(),
        )?;
        let _ = tag;
        Ok(Collected {
            destinations,
            helpers,
            discovery,
        })
    }

    /// The objects a collection queued, in queue order.
    fn queued_objects(collected: &mut Collected) -> Vec<String> {
        std::iter::from_fn(|| collected.discovery.pop())
            .filter_map(|item| match item {
                WorkItem::Object(soname) => Some(soname),
                WorkItem::Group(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_collected_helper_enters_the_object_queue_at_its_destination() {
        let (root, lib) = dist_tree("queue", &["usr/lib/gstreamer-1.0/gst-plugin-scanner"]);
        let mut collected = collect("queue", &scanner_declaration(), &root, &lib).unwrap();

        assert_eq!(
            collected.destinations.get("gst-plugin-scanner"),
            Some(&PathBuf::from("libexec/gstreamer-1.0/gst-plugin-scanner"))
        );
        assert!(collected.helpers.contains("gst-plugin-scanner"));
        assert_eq!(queued_objects(&mut collected), ["gst-plugin-scanner"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_helper_source_directory_joins_the_search_path() {
        let (root, lib) = dist_tree("searchdir", &["usr/lib/gstreamer-1.0/gst-plugin-scanner"]);
        let admitted = [AdmittedLibrary::new("libgstreamer-1.0.so.0", &lib)];
        let mut discovery = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
        let mut destinations = HashMap::new();
        let mut helpers = HashSet::new();
        let mut needed_by = HashMap::new();
        let mut processed = HashSet::new();
        let mut search_paths = Vec::new();
        let mut repair_extra = HashSet::new();
        let mut preparation = SetPreparation::new(&[], &[], Path::new("lib"), false);
        let mut plan = DestinationPlan::load(&root).unwrap();
        let descriptor = catalog::builtins()
            .iter()
            .find(|d| d.id.as_str() == "gstreamer")
            .unwrap();
        collect_helper_group(
            &root,
            descriptor,
            &scanner_declaration(),
            &admitted,
            None,
            &mut search_paths,
            &mut repair_extra,
            &mut destinations,
            &mut helpers,
            &mut needed_by,
            &mut processed,
            &mut discovery,
            &mut preparation,
            &mut plan,
            &mut HashSet::new(),
        )
        .unwrap();
        assert!(
            search_paths.contains(&root.join("usr/lib/gstreamer-1.0")),
            "{search_paths:?}"
        );
        assert_eq!(
            needed_by.get("gst-plugin-scanner"),
            Some(&"rule gstreamer helper scanner".to_string())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_required_helper_the_source_lacks_fails_naming_the_locations() {
        let (root, lib) = dist_tree("required-miss", &[]);
        let err = match collect("required-miss", &scanner_declaration(), &root, &lib) {
            Ok(_) => panic!("a required helper with no candidate must fail the assembly"),
            Err(err) => err,
        };
        let text = err.to_string();
        assert!(text.contains("rule gstreamer"), "{text}");
        assert!(text.contains("helper scanner"), "{text}");
        assert!(
            text.contains("libexec/gstreamer-1.0/gst-plugin-scanner"),
            "{text}"
        );
        assert!(text.contains("absent"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_optional_helper_the_source_lacks_continues_the_build() {
        let (root, lib) = dist_tree("optional-miss", &[]);
        let mut declared = scanner_declaration();
        declared.requirement = Requirement::Optional;

        let mut collected = collect("optional-miss", &declared, &root, &lib).unwrap();

        assert!(collected.destinations.is_empty());
        assert!(collected.helpers.is_empty());
        assert!(
            queued_objects(&mut collected).is_empty(),
            "nothing is queued"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_first_declared_alternative_serves_the_helper() {
        let (root, lib) = dist_tree(
            "first-alt",
            &[
                "usr/lib/gstreamer-1.0/gst-plugin-scanner",
                "usr/libexec/gstreamer-1.0/gst-plugin-scanner",
            ],
        );
        let mut collected = collect("first-alt", &scanner_declaration(), &root, &lib).unwrap();

        assert_eq!(collected.destinations.len(), 1);
        assert_eq!(
            collected.helpers,
            HashSet::from(["gst-plugin-scanner".to_string()])
        );
        assert_eq!(queued_objects(&mut collected).len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_destination_the_tree_already_holds_satisfies_the_helper() {
        let (root, lib) = dist_tree("tree-held", &[]);
        let tree = root.join("pkg");
        fs::create_dir_all(tree.join("libexec/gstreamer-1.0")).unwrap();
        fs::write(
            tree.join("libexec/gstreamer-1.0/gst-plugin-scanner"),
            fake_elf(),
        )
        .unwrap();

        let mut collected = collect("tree-held", &scanner_declaration(), &tree, &lib)
            .expect("a held helper satisfies the group");

        assert!(collected.destinations.is_empty());
        assert!(collected.helpers.is_empty());
        assert!(
            queued_objects(&mut collected).is_empty(),
            "the held helper is not requeued"
        );
        let _ = fs::remove_dir_all(&root);
    }
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

        // A glibc loader's name but no ELF magic.
        let noelf = d.join("ld64.so.2");
        fs::write(&noelf, b"NixOS cannot run").unwrap();
        assert!(!is_nix_stub_ld(&noelf));

        // Loader-shaped but not a glibc loader's name.
        let musl = d.join("ld-musl-x86_64.so.1");
        fs::write(&musl, &stub).unwrap();
        assert!(!is_nix_stub_ld(&musl));

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
            std::slice::from_ref(&dir2),
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
            std::slice::from_ref(&d),
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

#[cfg(test)]
mod mtime_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A sysroot's files arrive read-only, and a directory cannot be
    /// opened for writing at all: neither may keep the time it was made.
    #[test]
    fn read_only_files_and_directories_are_pinned() {
        let dir = std::env::temp_dir().join(format!("onelf-mtime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("ro");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        let expected = std::env::var("SOURCE_DATE_EPOCH")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        for path in [&file, &dir.join("sub")] {
            normalize_mtime(path);
            let mtime = fs::metadata(path)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            assert_eq!(mtime, expected, "{}", path.display());
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod dangling_link_tests {
    use super::*;

    #[test]
    fn only_links_that_led_to_the_dropped_object_go() {
        let dir = std::env::temp_dir().join(format!("onelf-dangling-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("libfoo.so.1.2"), b"x").unwrap();
        std::os::unix::fs::symlink("libfoo.so.1.2", dir.join("libfoo.so.1")).unwrap();
        std::os::unix::fs::symlink("libfoo.so.1", dir.join("libfoo.so")).unwrap();
        // The publisher's own dangling link, unrelated to libfoo.
        std::os::unix::fs::symlink("libbar.so.9", dir.join("libbar.so")).unwrap();

        fs::remove_file(dir.join("libfoo.so.1.2")).unwrap();
        remove_dangling_links_beside(&dir.join("libfoo.so.1.2"));
        assert!(!dir.join("libfoo.so.1").is_symlink(), "the chain goes");
        assert!(!dir.join("libfoo.so").is_symlink(), "the whole chain goes");
        assert!(
            dir.join("libbar.so").is_symlink(),
            "an unrelated link stays"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod private_temp_dir_tests {
    use super::*;

    #[test]
    fn each_scratch_directory_is_new_and_private() {
        let a = private_temp_dir("probe").unwrap();
        let b = private_temp_dir("probe").unwrap();
        assert_ne!(a, b);
        for dir in [&a, &b] {
            let meta = fs::symlink_metadata(dir).unwrap();
            assert!(meta.is_dir());
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        }
        let _ = fs::remove_dir(&a);
        let _ = fs::remove_dir(&b);
    }
}

#[cfg(test)]
mod gpu_stack_tests {
    use super::*;

    #[test]
    fn a_traced_gpu_stack_is_left_to_the_host() {
        let paths = [
            "/usr/lib/libGLX_mesa.so.0",
            "/usr/lib/libEGL_nvidia.so.0",
            "/usr/lib/libgallium-25.2.so",
            "/usr/lib/dri/radeonsi_dri.so",
            "/usr/lib/libvulkan_radeon.so",
            "/usr/lib/libSDL2-2.0.so.0",
        ];
        let (app, host) = split_gpu_stack(paths.iter().map(PathBuf::from).collect());
        assert_eq!(app, vec![PathBuf::from("/usr/lib/libSDL2-2.0.so.0")]);
        assert_eq!(host.len(), 5);
    }
}

#[cfg(test)]
mod locale_tests {
    use super::*;

    /// Debian keeps the locale one level above its multiarch libc.
    #[test]
    fn c_utf8_is_carried_from_beside_a_multiarch_libc() {
        let work = std::env::temp_dir().join(format!("onelf-cutf8-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        let libc_dir = work.join("usr/lib/x86_64-linux-gnu");
        let locale = work.join("usr/lib/locale/C.utf8");
        fs::create_dir_all(&libc_dir).unwrap();
        fs::create_dir_all(locale.join("LC_MESSAGES")).unwrap();
        fs::write(libc_dir.join("libc.so.6"), "").unwrap();
        fs::write(locale.join("LC_CTYPE"), "ctype").unwrap();
        fs::write(locale.join("LC_MESSAGES/SYS_LC_MESSAGES"), "msg").unwrap();
        let dest = work.join("tree/lib");

        let copied = bundle_c_utf8(&libc_dir.join("libc.so.6"), &dest).unwrap();

        assert_eq!(
            copied,
            [
                "locale/C.utf8/LC_CTYPE",
                "locale/C.utf8/LC_MESSAGES/SYS_LC_MESSAGES"
            ]
        );
        assert_eq!(
            fs::read_to_string(dest.join("locale/C.utf8/LC_CTYPE")).unwrap(),
            "ctype"
        );
        let _ = fs::remove_dir_all(&work);
    }
}

#[cfg(test)]
mod catalog_coverage_tests {
    use std::collections::BTreeSet;

    use onelf_format::catalog;
    use onelf_format::rules::{Inheritance, PathSelection, RuntimeHookId, SCHEMA_VERSION};

    use super::*;

    /// One framework family's declared surfaces: the descriptors it is
    /// carried through, the resource groups and their destinations and
    /// requirement classes, the runtime mappings and their policies, the
    /// registered hooks, and the sonames that activate it.
    struct Family {
        name: &'static str,
        descriptors: &'static [&'static str],
        /// (rule, group, destination, requirement)
        plugins: &'static [(&'static str, &'static str, &'static str, Requirement)],
        data: &'static [(&'static str, &'static str, &'static str, Requirement)],
        helpers: &'static [(&'static str, &'static str, &'static str, Requirement)],
        /// (variable, selection, inheritance)
        paths: &'static [(&'static str, PathSelection, Inheritance)],
        runtime_hooks: &'static [RuntimeHookId],
        build_hooks: &'static [BuildHookId],
        sonames: &'static [&'static str],
    }

    const FAMILIES: &[Family] = &[
        Family {
            name: "alsa",
            descriptors: &["alsa"],
            plugins: &[],
            data: &[("alsa", "configuration", "share/alsa", Requirement::Required)],
            helpers: &[],
            paths: &[
                (
                    "ALSA_PLUGIN_DIR",
                    PathSelection::First,
                    Inheritance::Replace,
                ),
                (
                    "ALSA_CONFIG_DIR",
                    PathSelection::First,
                    Inheritance::Replace,
                ),
            ],
            runtime_hooks: &[RuntimeHookId::AlsaConfigPathNaturalSort],
            build_hooks: &[BuildHookId::AlsaConfPatch],
            sonames: &["libasound.so.2"],
        },
        Family {
            name: "gstreamer",
            descriptors: &["gstreamer"],
            plugins: &[(
                "gstreamer",
                "plugins",
                "lib/gstreamer-1.0",
                Requirement::Optional,
            )],
            data: &[],
            helpers: &[(
                "gstreamer",
                "scanner",
                "libexec/gstreamer-1.0/gst-plugin-scanner",
                Requirement::Required,
            )],
            paths: &[
                (
                    "GST_PLUGIN_SYSTEM_PATH_1_0",
                    PathSelection::All,
                    Inheritance::Replace,
                ),
                (
                    "GST_PLUGIN_SYSTEM_PATH",
                    PathSelection::All,
                    Inheritance::Replace,
                ),
                (
                    "GST_PLUGIN_SCANNER_1_0",
                    PathSelection::First,
                    Inheritance::Replace,
                ),
                (
                    "GST_PLUGIN_SCANNER",
                    PathSelection::First,
                    Inheritance::Replace,
                ),
            ],
            runtime_hooks: &[],
            build_hooks: &[],
            sonames: &["libgstreamer-1.0.so.0"],
        },
        Family {
            name: "pipewire-spa",
            descriptors: &["pipewire", "spa"],
            plugins: &[
                (
                    "pipewire",
                    "plugins",
                    "lib/pipewire-0.3",
                    Requirement::Optional,
                ),
                ("spa", "plugins", "lib/spa-0.2", Requirement::Optional),
            ],
            data: &[
                (
                    "pipewire",
                    "configuration",
                    "share/pipewire/client.conf",
                    Requirement::Optional,
                ),
                (
                    "pipewire",
                    "configuration-rt",
                    "share/pipewire/client-rt.conf",
                    Requirement::Optional,
                ),
            ],
            helpers: &[],
            paths: &[
                (
                    "PIPEWIRE_MODULE_DIR",
                    PathSelection::All,
                    Inheritance::Replace,
                ),
                (
                    "PIPEWIRE_CONFIG_DIR",
                    PathSelection::First,
                    Inheritance::Preserve,
                ),
                ("SPA_PLUGIN_DIR", PathSelection::All, Inheritance::Replace),
            ],
            runtime_hooks: &[],
            build_hooks: &[],
            sonames: &["libpipewire-0.3.so.0", "libspa-0.2.so.0"],
        },
        Family {
            name: "gio-gi",
            descriptors: &["gio", "gi"],
            plugins: &[("gio", "modules", "lib/gio/modules", Requirement::Optional)],
            data: &[(
                "gi",
                "typelibs",
                "lib/girepository-1.0",
                Requirement::Optional,
            )],
            helpers: &[],
            paths: &[("GI_TYPELIB_PATH", PathSelection::All, Inheritance::Replace)],
            runtime_hooks: &[RuntimeHookId::GioModulePaths],
            build_hooks: &[],
            sonames: &["libgio-2.0.so.0", "libgirepository-1.0.so.1"],
        },
        Family {
            name: "xkb-x11",
            descriptors: &["xkb", "x11"],
            plugins: &[],
            data: &[
                ("xkb", "data", "share/X11/xkb", Requirement::Required),
                ("x11", "data", "share/X11/locale", Requirement::Required),
            ],
            helpers: &[],
            paths: &[
                (
                    "XKB_CONFIG_ROOT",
                    PathSelection::First,
                    Inheritance::Preserve,
                ),
                ("XLOCALEDIR", PathSelection::First, Inheritance::Preserve),
            ],
            runtime_hooks: &[],
            build_hooks: &[],
            sonames: &["libxkbcommon.so.0", "libX11.so.6"],
        },
        Family {
            name: "magic",
            descriptors: &["magic"],
            plugins: &[],
            data: &[(
                "magic",
                "database",
                "share/misc/magic.mgc",
                Requirement::Required,
            )],
            helpers: &[],
            paths: &[("MAGIC", PathSelection::First, Inheritance::Preserve)],
            runtime_hooks: &[],
            build_hooks: &[],
            sonames: &["libmagic.so.1"],
        },
        Family {
            name: "heif",
            descriptors: &["heif"],
            plugins: &[],
            data: &[],
            helpers: &[],
            paths: &[(
                "LIBHEIF_PLUGIN_PATH",
                PathSelection::FirstPerLibraryRoot,
                Inheritance::Replace,
            )],
            runtime_hooks: &[],
            build_hooks: &[],
            sonames: &["libheif.so.1"],
        },
        Family {
            name: "babl-gegl",
            descriptors: &["babl", "gegl"],
            plugins: &[
                ("babl", "plugins", "lib/babl-0.1", Requirement::Optional),
                ("gegl", "plugins", "lib/gegl-0.4", Requirement::Optional),
            ],
            data: &[],
            helpers: &[],
            paths: &[
                ("BABL_PATH", PathSelection::All, Inheritance::Replace),
                ("GEGL_PATH", PathSelection::All, Inheritance::Replace),
            ],
            runtime_hooks: &[],
            build_hooks: &[],
            sonames: &["libbabl-0.1.so.0", "libgegl-0.4.so.0"],
        },
        Family {
            name: "tcl-tk",
            descriptors: &["tcl-8.6", "tcl-9.0", "tk-8.6", "tk-9.0", "tcl-tk"],
            plugins: &[],
            data: &[
                ("tcl-8.6", "scripts", "lib/tcl8.6", Requirement::Required),
                ("tcl-9.0", "scripts", "lib/tcl9.0", Requirement::Required),
                ("tk-8.6", "scripts", "lib/tk8.6", Requirement::Required),
                ("tk-9.0", "scripts", "lib/tk9.0", Requirement::Required),
            ],
            helpers: &[],
            paths: &[],
            runtime_hooks: &[RuntimeHookId::TclTkLibraries],
            build_hooks: &[],
            sonames: &[
                "libtcl8.6.so",
                "libtcl9.0.so",
                "libtk8.6.so",
                "libtcl9tk9.0.so",
            ],
        },
        Family {
            name: "translations",
            descriptors: &["translations"],
            plugins: &[],
            data: &[],
            helpers: &[],
            paths: &[("TEXTDOMAINDIR", PathSelection::First, Inheritance::Preserve)],
            runtime_hooks: &[],
            build_hooks: &[],
            sonames: &[],
        },
        Family {
            name: "qt-gdk-gtk",
            descriptors: &["qt", "qt5", "qt6", "gdk-pixbuf", "gtk3"],
            plugins: &[
                ("qt5", "plugins", "lib/qt5/plugins", Requirement::Optional),
                ("qt6", "plugins", "lib/qt6/plugins", Requirement::Optional),
                (
                    "gdk-pixbuf",
                    "loaders",
                    "lib/gdk-pixbuf-2.0",
                    Requirement::Optional,
                ),
                ("gtk3", "modules", "lib/gtk-3.0", Requirement::Optional),
            ],
            data: &[],
            helpers: &[],
            paths: &[
                ("QT_PLUGIN_PATH", PathSelection::All, Inheritance::Replace),
                ("GTK_PATH", PathSelection::All, Inheritance::Replace),
            ],
            runtime_hooks: &[RuntimeHookId::GdkCacheAbsolutise],
            build_hooks: &[],
            sonames: &[
                "libQt5Core.so.5",
                "libQt6Core.so.6",
                "libgdk_pixbuf-2.0.so.0",
                "libgtk-3.so.0",
            ],
        },
        Family {
            name: "fontconfig",
            descriptors: &["fontconfig"],
            plugins: &[],
            data: &[],
            helpers: &[],
            paths: &[],
            runtime_hooks: &[RuntimeHookId::FontconfigGenerate],
            build_hooks: &[],
            sonames: &["libfontconfig.so.1"],
        },
    ];

    fn descriptor(id: &str) -> &'static Descriptor {
        catalog::builtins()
            .iter()
            .find(|d| d.id.as_str() == id)
            .unwrap_or_else(|| panic!("descriptor {id} is missing from the catalog"))
    }

    /// Every schema 1 descriptor the twelve families are carried
    /// through exists, is valid, and the catalog holds nothing else.
    #[test]
    fn the_catalog_is_exactly_the_families_descriptors() {
        let expected: BTreeSet<&str> = FAMILIES
            .iter()
            .flat_map(|family| family.descriptors.iter().copied())
            .collect();
        let actual: BTreeSet<&str> = catalog::builtins().iter().map(|d| d.id.as_str()).collect();
        assert_eq!(actual, expected, "catalog drift against the families");
        for descriptor in catalog::builtins() {
            assert_eq!(
                descriptor.schema,
                SCHEMA_VERSION,
                "{}",
                descriptor.id.as_str()
            );
            descriptor.validate().expect("built-ins validate");
        }
    }

    /// Every family's descriptors declare exactly the groups, runtime
    /// mappings, policies and hooks the family is carried through, and
    /// each activating soname is a real prefix of the declaring rule.
    #[test]
    fn every_family_declares_its_groups_mappings_and_hooks() {
        for family in FAMILIES {
            let mut plugins: Vec<String> = Vec::new();
            let mut data: Vec<String> = Vec::new();
            let mut helpers: Vec<String> = Vec::new();
            let mut paths: Vec<(String, PathSelection, Inheritance)> = Vec::new();
            let mut runtime_hooks: Vec<RuntimeHookId> = Vec::new();
            let mut build_hooks: Vec<BuildHookId> = Vec::new();
            for id in family.descriptors {
                let descriptor = descriptor(id);
                for group in &descriptor.resources.plugins {
                    plugins.push(format!(
                        "{}/{}/{} {:?}",
                        id,
                        group.id.as_str(),
                        group.destination.as_str(),
                        group.requirement
                    ));
                }
                for group in &descriptor.resources.data {
                    data.push(format!(
                        "{}/{}/{} {:?}",
                        id,
                        group.id.as_str(),
                        group.destination.as_str(),
                        group.requirement
                    ));
                }
                for helper in &descriptor.resources.helpers {
                    helpers.push(format!(
                        "{}/{}/{} {:?}",
                        id,
                        helper.id.as_str(),
                        helper.destination.as_str(),
                        helper.requirement
                    ));
                }
                for path in &descriptor.runtime.paths {
                    for variable in &path.variables {
                        paths.push((
                            variable.as_str().to_string(),
                            path.selection,
                            path.inheritance,
                        ));
                    }
                }
                runtime_hooks.extend(descriptor.runtime.hooks.iter().copied());
                build_hooks.extend(descriptor.build.hooks.iter().copied());
            }
            let want_plugins = family
                .plugins
                .iter()
                .map(|(rule, group, dest, requirement)| {
                    format!("{rule}/{group}/{dest} {requirement:?}")
                })
                .collect::<Vec<_>>();
            let want_data = family
                .data
                .iter()
                .map(|(rule, group, dest, requirement)| {
                    format!("{rule}/{group}/{dest} {requirement:?}")
                })
                .collect::<Vec<_>>();
            let want_helpers = family
                .helpers
                .iter()
                .map(|(rule, group, dest, requirement)| {
                    format!("{rule}/{group}/{dest} {requirement:?}")
                })
                .collect::<Vec<_>>();
            let want_paths = family
                .paths
                .iter()
                .map(|(v, s, i)| ((*v).to_string(), *s, *i))
                .collect::<Vec<_>>();
            assert_eq!(plugins, want_plugins, "{} plugin groups", family.name);
            assert_eq!(data, want_data, "{} data groups", family.name);
            assert_eq!(helpers, want_helpers, "{} helper groups", family.name);
            assert_eq!(paths, want_paths, "{} runtime mappings", family.name);
            assert_eq!(runtime_hooks, family.runtime_hooks, "{}", family.name);
            assert_eq!(build_hooks, family.build_hooks, "{}", family.name);
        }
    }

    /// Every triggered descriptor of every family activates from one of
    /// the family's sonames and queues exactly its declared groups, and
    /// the prefix predicate is a real prefix of that soname.
    #[test]
    fn every_family_activates_from_its_sonames_and_queues_its_groups() {
        for family in FAMILIES {
            let mut discovery = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
            for soname in family.sonames {
                discovery.observe(soname).unwrap();
            }
            let activated: Vec<RuleActivation> = discovery
                .activations()
                .into_iter()
                .filter(|a| family.descriptors.contains(&a.rule.as_str()))
                .collect();
            let mut want: Vec<&str> = family
                .descriptors
                .iter()
                .copied()
                .filter(|id| !descriptor(id).triggers.is_empty())
                .collect();
            let mut got: Vec<&str> = activated
                .iter()
                .map(|a| a.rule.as_str())
                .collect::<Vec<_>>();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "{} activation", family.name);
            for activation in &activated {
                let d = descriptor(&activation.rule);
                let declared: Vec<String> = d
                    .resources
                    .plugins
                    .iter()
                    .map(|g| g.id.as_str().to_string())
                    .chain(d.resources.data.iter().map(|g| g.id.as_str().to_string()))
                    .chain(
                        d.resources
                            .helpers
                            .iter()
                            .map(|g| g.id.as_str().to_string()),
                    )
                    .collect();
                assert_eq!(activation.groups, declared, "{}", activation.rule);
                assert!(
                    d.triggers
                        .iter()
                        .any(|t| t.matches_soname(&activation.reason)),
                    "{} activated by {} which matches no trigger",
                    activation.rule,
                    activation.reason
                );
            }
        }
    }

    /// The registered build hooks are exactly the identities the
    /// catalog declares, and the declared identity resolves.
    #[test]
    fn the_registry_holds_exactly_the_catalog_declared_build_hooks() {
        let declared: BTreeSet<BuildHookId> = catalog::builtins()
            .iter()
            .flat_map(|d| d.build.hooks.iter().copied())
            .collect();
        assert_eq!(declared, BTreeSet::from([BuildHookId::AlsaConfPatch]));
        let _ = staged_hook(BuildHookId::AlsaConfPatch);
    }

    /// Dispatching a declared hook through the registry runs its
    /// procedure: a dry run writes nothing, a real run anchors the
    /// carried snippets in a carried `alsa.conf`.
    #[test]
    fn every_declared_build_hook_dispatches_and_honors_a_dry_run() {
        let work = std::env::temp_dir().join(format!("onelf-hook-dispatch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        let tree = work.join("tree");
        let conf = tree.join("share/alsa/alsa.conf");
        fs::create_dir_all(conf.parent().unwrap()).unwrap();
        fs::write(
            &conf,
            "@hooks [\n\t{\n\t\tfiles [\n\t\t\t\"/etc/alsa/conf.d\"\n\t\t]\n\t}\n]\n",
        )
        .unwrap();

        run_staged_hooks(&[BuildHookId::AlsaConfPatch], &tree, true);
        assert_eq!(
            fs::read_to_string(&conf).unwrap(),
            "@hooks [\n\t{\n\t\tfiles [\n\t\t\t\"/etc/alsa/conf.d\"\n\t\t]\n\t}\n]\n",
            "a dry run writes nothing"
        );

        run_staged_hooks(&[BuildHookId::AlsaConfPatch], &tree, false);
        let patched = fs::read_to_string(&conf).unwrap();
        let host = patched.find("\"/etc/alsa/conf.d\"").unwrap();
        let carried = patched
            .find("{ @func datadir } \"/alsa.conf.d\"")
            .expect("the carried snippet directory is anchored");
        assert!(host < carried, "{patched}");
        let _ = fs::remove_dir_all(&work);
    }

    /// The frameworks whose libraries never compile in the whole path of
    /// their resource directories, so inference cannot see them and their
    /// descriptors stay: GLib, GTK, gdk-pixbuf, GObject introspection,
    /// babl and gegl assemble theirs at run time from pieces, and Qt
    /// compiles only paths relative to its install prefix.
    const RUNTIME_ASSEMBLED: &[&str] = &[
        "gio",
        "gtk3",
        "gdk-pixbuf",
        "gi",
        "babl",
        "gegl",
        "qt5",
        "qt6",
    ];

    /// Every resource section a built-in descriptor still declares is
    /// one inference cannot reproduce: a file, a required group whose
    /// absence must fail the build, a member allowlist that selects a
    /// subset, a runtime-assembled framework, or a group hosting one
    /// of the same descriptor's helpers. A section inference
    /// reproduces belongs to the fixture-verified reduction, not the
    /// catalog.
    #[test]
    fn every_remaining_resource_section_is_beyond_inference() {
        for descriptor in catalog::builtins() {
            let id = descriptor.id.as_str();
            let assembled = RUNTIME_ASSEMBLED.contains(&id);
            for (group, kind) in descriptor
                .resources
                .plugins
                .iter()
                .map(|g| (g, "plugins"))
                .chain(descriptor.resources.data.iter().map(|g| (g, "data")))
            {
                let hosts_helper = descriptor
                    .resources
                    .helpers
                    .iter()
                    .any(|helper| helper.group == group.id);
                let beyond = group.kind == ResourceKind::File
                    || group.requirement == Requirement::Required
                    || !group.members.is_empty()
                    || assembled
                    || hosts_helper;
                assert!(
                    beyond,
                    "{id}/{} {}: inference reproduces this section, so the \
                     descriptor must not carry it",
                    kind,
                    group.id.as_str()
                );
            }
            for helper in &descriptor.resources.helpers {
                assert!(
                    helper.kind == ResourceKind::File,
                    "{id}/{}: a helper is a file, which inference never carries",
                    helper.id.as_str()
                );
            }
        }
    }
}

#[cfg(test)]
mod provider_audit_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("onelf-audit-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Minimal ELF64 shared object carrying the `DT_NEEDED` entries
    /// `needed` names and an optional `DT_SONAME`, so the audit walks
    /// real objects without a toolchain.
    fn dyn_object(needed: &[&str], soname: Option<&str>) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let offset = |name: &str, strtab: &mut Vec<u8>| {
            let at = strtab.len();
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            at as u64
        };
        let mut dyns: Vec<(u64, u64)> = Vec::new();
        dyns.push((5, 0));
        dyns.push((10, 0));
        for name in needed {
            let at = offset(name, &mut strtab);
            dyns.push((1, at));
        }
        if let Some(name) = soname {
            let at = offset(name, &mut strtab);
            dyns.push((14, at));
        }
        dyns.push((0, 0));
        let dyn_off = 64 + 2 * 56;
        let dyn_size = dyns.len() * 16;
        let str_off = dyn_off + dyn_size;
        let total = str_off + strtab.len();
        dyns[0].1 = str_off as u64;
        dyns[1].1 = strtab.len() as u64;

        let mut elf = Vec::new();
        elf.extend_from_slice(b"\x7fELF");
        elf.extend_from_slice(&[2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        elf.extend_from_slice(&3u16.to_le_bytes());
        elf.extend_from_slice(&62u16.to_le_bytes());
        elf.extend_from_slice(&1u32.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&64u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&0u32.to_le_bytes());
        elf.extend_from_slice(&64u16.to_le_bytes());
        elf.extend_from_slice(&56u16.to_le_bytes());
        elf.extend_from_slice(&(2u16.to_le_bytes()));
        elf.extend_from_slice(&[0u8; 6]);
        elf.extend_from_slice(&1u32.to_le_bytes());
        elf.extend_from_slice(&6u32.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&(total as u64).to_le_bytes());
        elf.extend_from_slice(&(total as u64).to_le_bytes());
        elf.extend_from_slice(&0x1000u64.to_le_bytes());
        elf.extend_from_slice(&2u32.to_le_bytes());
        elf.extend_from_slice(&6u32.to_le_bytes());
        for _ in 0..3 {
            elf.extend_from_slice(&(dyn_off as u64).to_le_bytes());
        }
        elf.extend_from_slice(&(dyn_size as u64).to_le_bytes());
        elf.extend_from_slice(&(dyn_size as u64).to_le_bytes());
        elf.extend_from_slice(&8u64.to_le_bytes());
        for (tag, value) in &dyns {
            elf.extend_from_slice(&tag.to_le_bytes());
            elf.extend_from_slice(&value.to_le_bytes());
        }
        elf.extend_from_slice(&strtab);
        assert_eq!(elf.len(), total);
        assert_eq!(parse_needed_bytes(&elf).unwrap(), needed);
        let parsed = goblin::elf::Elf::parse(&elf).unwrap();
        assert_eq!(parsed.soname, soname);
        elf
    }

    /// The content hash a test export carries: the id in the first
    /// byte, so two exports with the same id are "the same bytes" and
    /// two with different ids are not.
    fn content(id: u8) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = id;
        hash
    }

    fn provider(set: &str, pin: u64, exports: &[(&str, u8)]) -> sysroot::SetProvider {
        sysroot::SetProvider {
            set: set.to_string(),
            pin: format!("{pin:064x}"),
            provides: exports.iter().map(|(s, _)| s.to_string()).collect(),
            exports: exports
                .iter()
                .map(|(soname, id)| sysroot::SetExport {
                    soname: soname.to_string(),
                    blake3: content(*id),
                })
                .collect(),
        }
    }

    fn entrypoint(
        name: &str,
        path: Option<&str>,
        selection: recipe::SetSelection,
    ) -> AuditedEntrypoint {
        AuditedEntrypoint {
            name: name.to_string(),
            path: path.map(PathBuf::from),
            selection,
        }
    }

    fn names(list: &[&str]) -> recipe::SetSelection {
        recipe::SetSelection::Names(list.iter().map(|s| s.to_string()).collect())
    }

    fn no_host(_soname: &str) -> bool {
        false
    }

    /// The sonames the ELF objects under `root` export, each with the
    /// content hash of the object exporting it: the same derivation the
    /// set artifact's provider index and the audit's hashes follow,
    /// against a plain tree.
    fn tree_exports(root: &Path) -> Vec<sysroot::SetExport> {
        let mut found: Vec<sysroot::SetExport> = jwalk::WalkDir::new(root)
            .skip_hidden(false)
            .sort(true)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                let data = fs::read(e.path()).ok()?;
                let elf = goblin::elf::Elf::parse(&data).ok()?;
                let soname = elf.soname?;
                Some(sysroot::SetExport {
                    soname: soname.to_string(),
                    blake3: *blake3::hash(&data).as_bytes(),
                })
            })
            .collect();
        found.sort_by(|a, b| (&a.soname, &a.blake3).cmp(&(&b.soname, &b.blake3)));
        found.dedup();
        found
    }

    /// `bin/cli` needs a soname only the `gui` set exports, and gets its
    /// own selection only: the audit fails for the CLI naming the GUI
    /// set, and never for the GUI, whose own selection supplies its own
    /// need. Naming both sets supplies the CLI.
    #[test]
    fn a_gui_only_set_cannot_satisfy_the_cli_needs() {
        let d = tmp("gui-cli");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/cli"), dyn_object(&["libshared.so.1"], None)).unwrap();
        fs::write(d.join("bin/gui"), dyn_object(&["libshared.so.1"], None)).unwrap();
        let providers = vec![
            provider("cli", 1, &[("libcli.so.1", 1)]),
            provider("gui", 2, &[("libshared.so.1", 1)]),
        ];
        let entrypoints = vec![
            entrypoint("cli", Some("bin/cli"), names(&["cli"])),
            entrypoint("gui", Some("bin/gui"), names(&["gui"])),
        ];
        let err =
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("entrypoint 'cli'"), "{text}");
        assert!(text.contains("libshared.so.1"), "{text}");
        assert!(text.contains("which gui exports"), "{text}");
        assert!(!text.contains("entrypoint 'gui'"), "{text}");

        // The same two entrypoints with the CLI naming both sets: its
        // need is satisfied by its own selection, and nothing else
        // stands in the way.
        let entrypoints = vec![
            entrypoint("cli", Some("bin/cli"), names(&["cli", "gui"])),
            entrypoint("gui", Some("bin/gui"), names(&["gui"])),
        ];
        audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
            .unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    /// The providers a build's sets actually produce when their
    /// sonames never overlap: names without content hashes, because a
    /// hash is what the collision rule alone weighs. Such a provider
    /// still answers its own selection, and an unselected one satisfies
    /// nothing, with the failure naming the set that was left out.
    #[test]
    fn an_exports_free_provider_answers_its_own_selection_alone() {
        let d = tmp("exports-free");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/cli"), dyn_object(&["libshared.so.1"], None)).unwrap();
        let providers = vec![
            sysroot::SetProvider {
                set: "alpha".to_string(),
                pin: format!("{:064x}", 1u64),
                provides: vec!["libshared.so.1".to_string()],
                exports: Vec::new(),
            },
            sysroot::SetProvider {
                set: "beta".to_string(),
                pin: format!("{:064x}", 2u64),
                provides: vec!["libbeta.so.1".to_string()],
                exports: Vec::new(),
            },
        ];
        let entrypoints = vec![entrypoint("cli", Some("bin/cli"), names(&["beta"]))];
        let err =
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("entrypoint 'cli'"), "{text}");
        assert!(text.contains("bin/cli needs libshared.so.1"), "{text}");
        assert!(text.contains("which alpha exports"), "{text}");
        assert!(!text.contains("entrypoint 'beta'"), "{text}");

        // Naming the exporting set too supplies the need: the
        // selection, not the audit, decides what counts.
        let entrypoints = vec![entrypoint(
            "cli",
            Some("bin/cli"),
            names(&["alpha", "beta"]),
        )];
        audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
            .unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    /// A selected set that claims nothing, the shape an unchecked build
    /// leaves, satisfies nothing: the need fails for its own reason
    /// instead of hiding behind an unverifiable claim.
    #[test]
    fn a_selected_provider_that_claims_nothing_satisfies_nothing() {
        let d = tmp("claims-nothing");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&["libshared.so.1"], None)).unwrap();
        let providers = vec![sysroot::SetProvider {
            set: "opaque".to_string(),
            pin: format!("{:064x}", 1u64),
            provides: Vec::new(),
            exports: Vec::new(),
        }];
        let entrypoints = vec![entrypoint("app", Some("bin/app"), names(&["opaque"]))];
        let err =
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("bin/app needs libshared.so.1"), "{text}");
        assert!(text.contains("nothing in the application"), "{text}");
        let _ = fs::remove_dir_all(&d);
    }

    /// Two selected declarations pinning the same build are one
    /// provider however they are named: no conflict, and the need they
    /// both export is satisfied.
    #[test]
    fn identical_exports_deduplicate() {
        let d = tmp("dedup");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&["libshared.so.1"], None)).unwrap();
        let providers = vec![
            provider("alpha", 1, &[("libshared.so.1", 1)]),
            provider("beta", 1, &[("libshared.so.1", 1)]),
        ];
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
            .unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    /// Two selected declarations pinning different builds but exporting
    /// one soname is refused, naming both sets, whatever order the
    /// selection read.
    #[test]
    fn differing_simultaneous_exports_fail_naming_both_sets() {
        let d = tmp("diff");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&["libshared.so.1"], None)).unwrap();
        let providers = vec![
            provider("beta", 2, &[("libshared.so.1", 2)]),
            provider("alpha", 1, &[("libshared.so.1", 1)]),
        ];
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        let err =
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("entrypoint 'app'"), "{text}");
        assert!(text.contains("libshared.so.1"), "{text}");
        assert!(text.contains("alpha"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(text.contains("exported with different bytes"), "{text}");
        assert!(text.contains("cannot be selected together"), "{text}");
        let _ = fs::remove_dir_all(&d);
    }

    /// The drop pass never omits what the assembly requires: a named
    /// manifest entrypoint and an object a required descriptor group
    /// placed stay as findings, while a generated auto-collected plugin
    /// with the same unmet need is omitted.
    #[test]
    fn the_drop_pass_keeps_named_entrypoints_and_required_objects() {
        let d = tmp("protect");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::create_dir_all(d.join("lib/plugins")).unwrap();
        fs::create_dir_all(d.join("libexec")).unwrap();
        let unmet = dyn_object(&["libgone.so.1"], None);
        for rel in ["bin/gui", "lib/plugins/auto.so", "libexec/helper"] {
            fs::write(d.join(rel), &unmet).unwrap();
        }
        sysroot::record_generated(
            &d,
            vec![
                "bin/gui".to_string(),
                "lib/plugins/auto.so".to_string(),
                "libexec/helper".to_string(),
            ],
        )
        .unwrap();
        let opts = BundleOptions {
            directory: d.clone(),
            lib_dir: PathBuf::from("lib"),
            primary: Some(PathBuf::from("bin/cli")),
            entrypoint_selections: vec![entrypoint(
                "gui",
                Some("bin/gui"),
                recipe::SetSelection::All,
            )],
            strict: true,
            ..Default::default()
        };
        let required: HashSet<PathBuf> = HashSet::from([PathBuf::from("libexec/helper")]);
        let audit = || audit_unbundled_needs(&opts.directory, &opts.lib_dir, &[]);
        let mut findings = audit();
        let dropped = drop_unloadable(
            &opts,
            &mut findings,
            audit,
            &SourceClassifier::host_only(),
            &required,
            &[],
        )
        .unwrap();
        let gone: Vec<&Path> = dropped.iter().map(|x| x.object.as_path()).collect();
        assert_eq!(gone, vec![Path::new("lib/plugins/auto.so")]);
        assert!(d.join("bin/gui").is_file());
        assert!(d.join("libexec/helper").is_file());
        let left: Vec<PathBuf> = findings
            .iter()
            .map(|(o, _)| o.strip_prefix(&d).unwrap().to_path_buf())
            .collect();
        assert!(left.contains(&PathBuf::from("bin/gui")), "{left:?}");
        assert!(left.contains(&PathBuf::from("libexec/helper")), "{left:?}");
        let _ = fs::remove_dir_all(&d);
    }

    /// Under the restricted omission pass, a link beside a removed
    /// member goes only through the sweep: a link the pass did not
    /// generate stays, even though the removal left it dangling, and
    /// a generated link that points outside the tree names nothing
    /// the pass removed, so it stays too.
    #[test]
    fn a_publisher_sibling_link_beside_an_omitted_member_stays() {
        let d = tmp("restricted-sibling");
        fs::create_dir_all(d.join("lib/libfw/plugins")).unwrap();
        fs::write(
            d.join("lib/libfw/plugins/a.so"),
            dyn_object(&["libgone.so.1"], None),
        )
        .unwrap();
        std::os::unix::fs::symlink("a.so", d.join("lib/libfw/plugins/b.so")).unwrap();
        std::os::unix::fs::symlink(
            "../../../../escape/lib.so",
            d.join("lib/libfw/plugins/out.so"),
        )
        .unwrap();
        sysroot::record_generated(
            &d,
            vec![
                "lib/libfw/plugins/a.so".to_string(),
                "lib/libfw/plugins/out.so".to_string(),
            ],
        )
        .unwrap();
        let opts = BundleOptions {
            directory: d.clone(),
            lib_dir: PathBuf::from("lib"),
            ..Default::default()
        };
        let follow = [d.join("lib/libfw/plugins/b.so")];
        let audit = || audit_unbundled_needs(&opts.directory, &opts.lib_dir, &follow);
        let mut findings = audit();
        let dropped = drop_unloadable(
            &opts,
            &mut findings,
            audit,
            &SourceClassifier::host_only(),
            &HashSet::new(),
            &[PathBuf::from("lib/libfw/plugins")],
        )
        .unwrap();
        let gone: Vec<&Path> = dropped.iter().map(|x| x.object.as_path()).collect();
        assert_eq!(gone, vec![Path::new("lib/libfw/plugins/a.so")]);
        assert!(
            d.join("lib/libfw/plugins/b.so").is_symlink(),
            "the publisher's link stays"
        );
        assert!(
            d.join("lib/libfw/plugins/out.so").is_symlink(),
            "the outside-tree link stays"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// Providers that agree on the bytes form one side of a collision,
    /// so a finding names one set per distinct content, never two sets
    /// that ship the same export.
    #[test]
    fn a_collision_names_one_set_per_distinct_content() {
        let providers = [
            provider("alpha", 1, &[("libshared.so.1", 1)]),
            provider("beta", 2, &[("libshared.so.1", 2)]),
            provider("gamma", 3, &[("libshared.so.1", 2)]),
        ];
        let selected: Vec<&sysroot::SetProvider> = providers.iter().collect();
        let findings = provider_collisions("app", &selected);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let text = &findings[0];
        assert!(text.contains("alpha"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(!text.contains("gamma"), "{text}");
    }

    /// Two entrypoints whose selections never name the same set use
    /// different providers of one soname without a conflict: each is
    /// audited against its own selection alone.
    #[test]
    fn disjoint_selections_use_different_providers() {
        let d = tmp("disjoint");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/cli"), dyn_object(&["libshared.so.1"], None)).unwrap();
        fs::write(d.join("bin/gui"), dyn_object(&["libshared.so.1"], None)).unwrap();
        let providers = vec![
            provider("alpha", 1, &[("libshared.so.1", 1)]),
            provider("beta", 2, &[("libshared.so.1", 2)]),
        ];
        let entrypoints = vec![
            entrypoint("cli", Some("bin/cli"), names(&["alpha"])),
            entrypoint("gui", Some("bin/gui"), names(&["beta"])),
        ];
        audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
            .unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    /// Two set trees whose private plugins share a basename export only
    /// their own sonames, so the basename adds nothing to the exports
    /// and the two builds, pinning different bytes, still never
    /// collide on it.
    #[test]
    fn private_plugin_basenames_are_not_collisions() {
        let d = tmp("plugins");
        let alpha = d.join("set-alpha");
        let beta = d.join("set-beta");
        for (set, soname) in [(&alpha, "liba.so.1"), (&beta, "libb.so.1")] {
            fs::create_dir_all(set.join("lib/plugins")).unwrap();
            fs::write(
                set.join("lib/libcarried.so.1"),
                dyn_object(&[], Some(soname)),
            )
            .unwrap();
            fs::write(set.join("lib/plugins/helper.so"), dyn_object(&[], None)).unwrap();
        }
        let alpha_exports = tree_exports(&alpha);
        let beta_exports = tree_exports(&beta);
        assert_eq!(
            alpha_exports
                .iter()
                .map(|e| e.soname.as_str())
                .collect::<Vec<_>>(),
            ["liba.so.1"]
        );
        assert_eq!(
            beta_exports
                .iter()
                .map(|e| e.soname.as_str())
                .collect::<Vec<_>>(),
            ["libb.so.1"]
        );

        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(
            d.join("bin/app"),
            dyn_object(&["liba.so.1", "libb.so.1"], None),
        )
        .unwrap();
        let providers = vec![
            sysroot::SetProvider {
                set: "alpha".to_string(),
                pin: format!("{:064x}", 1u64),
                provides: alpha_exports.iter().map(|e| e.soname.clone()).collect(),
                exports: alpha_exports,
            },
            sysroot::SetProvider {
                set: "beta".to_string(),
                pin: format!("{:064x}", 2u64),
                provides: beta_exports.iter().map(|e| e.soname.clone()).collect(),
                exports: beta_exports,
            },
        ];
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
            .unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    /// A library the tree itself carries satisfies the need, whether or
    /// not a selected set also exports the soname: the application
    /// outranks its sets. Take the copy away and the same need fails,
    /// so the audit's answer came from the tree and nothing else.
    #[test]
    fn an_app_owned_provider_satisfies_the_requirement() {
        let d = tmp("app-owned");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&["libshared.so.1"], None)).unwrap();
        fs::write(
            d.join("lib/libshared.so.1"),
            dyn_object(&[], Some("libshared.so.1")),
        )
        .unwrap();
        let providers = vec![provider("alpha", 1, &[("libshared.so.1", 1)])];
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
            .unwrap();

        // The audit's answer came from the tree: take the app's copy
        // away and a set that exports nothing cannot answer the need.
        let providers = vec![provider("alpha", 1, &[])];
        fs::remove_file(d.join("lib/libshared.so.1")).unwrap();
        let err =
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
                .unwrap_err();
        assert!(err.to_string().contains("libshared.so.1"), "{err}");
        let _ = fs::remove_dir_all(&d);
    }

    /// A selected set exporting `libshared.so.1` does not satisfy a
    /// need for `libshared.so.2`: the version a need names is the one
    /// that counts, and the failure names the entrypoint, the need, and
    /// the set that holds the version the selection left out.
    #[test]
    fn an_insufficient_provider_version_fails_naming_what_was_found() {
        let d = tmp("version");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/cli"), dyn_object(&["libshared.so.2"], None)).unwrap();
        let providers = vec![
            provider("cli", 1, &[("libshared.so.1", 1)]),
            provider("gui", 2, &[("libshared.so.2", 2)]),
        ];
        let entrypoints = vec![entrypoint("cli", Some("bin/cli"), names(&["cli"]))];
        let err =
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("entrypoint 'cli'"), "{text}");
        assert!(text.contains("libshared.so.2"), "{text}");
        assert!(text.contains("which gui exports"), "{text}");
        let _ = fs::remove_dir_all(&d);
    }

    /// A need nothing declared exports fails naming the entrypoint, the
    /// requesting object, and the soname, without inventing a provider.
    #[test]
    fn an_absent_provider_names_the_need_and_the_object() {
        let d = tmp("absent");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&["libgone.so.1"], None)).unwrap();
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        let err = audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &[], &no_host)
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("entrypoint 'app'"), "{text}");
        assert!(text.contains("bin/app needs libgone.so.1"), "{text}");
        assert!(text.contains("nothing in the application"), "{text}");
        let _ = fs::remove_dir_all(&d);
    }

    /// A resolved need is followed into the object it named, so the
    /// closure is the entrypoint's own: the failure names the carried
    /// library that asked, not the executable, and a need another
    /// entrypoint's objects alone reach never shows up here.
    #[test]
    fn the_requirements_walk_follows_the_entrypoints_own_closure() {
        let d = tmp("closure");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&["libcarried.so.1"], None)).unwrap();
        fs::write(
            d.join("lib/libcarried.so.1"),
            dyn_object(&["libgone.so.1"], Some("libcarried.so.1")),
        )
        .unwrap();
        // Another entrypoint's objects need something else; its
        // selection must not answer for this one's audit.
        fs::write(d.join("bin/other"), dyn_object(&["libgui.so.1"], None)).unwrap();
        let providers = vec![provider("gui", 1, &[("libgui.so.1", 1)])];
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        let err =
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &providers, &no_host)
                .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("lib/libcarried.so.1 needs libgone.so.1"),
            "{text}"
        );
        assert!(!text.contains("libgui.so.1"), "{text}");
        let _ = fs::remove_dir_all(&d);
    }

    /// An entrypoint audited for its selection alone contributes no
    /// rooted needs, so a tree the audit cannot root, even one that does
    /// not exist, cannot fail it; the collision rule still applies to
    /// the providers the selection names.
    #[test]
    fn a_selection_only_entrypoint_needs_no_tree() {
        let providers = vec![
            provider("alpha", 1, &[("liba.so.1", 1)]),
            provider("beta", 2, &[("libb.so.1", 2)]),
        ];
        let entrypoints = vec![entrypoint("app", None, recipe::SetSelection::All)];
        audit_entrypoint_providers(
            Path::new("/onelf-audit-no-such-tree"),
            Path::new("lib"),
            &entrypoints,
            &providers,
            &no_host,
        )
        .unwrap();
        let colliding = vec![
            provider("alpha", 1, &[("liba.so.1", 1)]),
            provider("beta", 2, &[("liba.so.1", 2)]),
        ];
        let err = audit_entrypoint_providers(
            Path::new("/onelf-audit-no-such-tree"),
            Path::new("lib"),
            &entrypoints,
            &colliding,
            &no_host,
        )
        .unwrap_err();
        assert!(err.to_string().contains("liba.so.1"), "{err}");
    }

    /// A selection naming a set the recipe never declares is refused,
    /// naming the entrypoint and the name.
    #[test]
    fn an_unknown_selection_name_is_refused() {
        let d = tmp("ghost");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&[], None)).unwrap();
        let entrypoints = vec![entrypoint("app", Some("bin/app"), names(&["ghost"]))];
        let err = audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &[], &no_host)
            .unwrap_err();
        assert!(err.to_string().contains("set ghost"), "{err}");
        let _ = fs::remove_dir_all(&d);
    }

    /// An entrypoint whose executable is missing from the tree is a
    /// finding, since nothing roots its requirements.
    #[test]
    fn a_missing_entrypoint_executable_is_a_finding() {
        let d = tmp("missing");
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        let err = audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &[], &no_host)
            .unwrap_err();
        assert!(
            err.to_string().contains("bin/app is not in the tree"),
            "{err}"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// A failing audit leaves the tree byte for byte as it found it:
    /// the caller may run it before the assembly writes and trust that
    /// a failure wrote nothing.
    #[test]
    fn a_failing_audit_writes_nothing() {
        let d = tmp("readonly");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::write(d.join("bin/app"), dyn_object(&["libgone.so.1"], None)).unwrap();
        fs::write(
            d.join("lib/libcarried.so.1"),
            dyn_object(&[], Some("libcarried.so.1")),
        )
        .unwrap();
        let before = snapshot(&d);
        let entrypoints = vec![entrypoint(
            "app",
            Some("bin/app"),
            recipe::SetSelection::All,
        )];
        assert!(
            audit_entrypoint_providers(&d, Path::new("lib"), &entrypoints, &[], &no_host).is_err()
        );
        assert_eq!(before, snapshot(&d), "the audit must not write");
        let _ = fs::remove_dir_all(&d);
    }

    /// Every file under `root`, by relative path and bytes.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files: Vec<(PathBuf, Vec<u8>)> = jwalk::WalkDir::new(root)
            .skip_hidden(false)
            .sort(true)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let rel = e.path().strip_prefix(root).unwrap().to_path_buf();
                (rel, fs::read(e.path()).unwrap())
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }
}

/// Set preparation into per-pin subtrees: the verified reads out of a
/// pinned build, the pin keyed destinations, the helper execution
/// rules, and the selection scoping over what was prepared.
#[cfg(test)]
mod set_preparation_tests {
    use super::*;
    use onelf_format::rules::RelativePath;
    use onelf_format::{Block, Entry, EntryKind, Flags, Footer, Manifest, ManifestHeader};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("onelf-prep-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A STORED-mode package carrying `files` at their literal paths.
    /// STORED blocks keep the bytes verbatim, so a test can tamper with
    /// one payload byte and the container's verified read must refuse
    /// what the pin no longer names.
    fn stored_package(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut string_table: Vec<u8> = vec![0];
        let mut entries = Vec::new();
        let mut payload = Vec::new();
        for (name, content) in files {
            let name_offset = string_table.len() as u32;
            string_table.extend_from_slice(name.as_bytes());
            string_table.push(0);
            let hash = *blake3::hash(content).as_bytes();
            entries.push(Entry {
                kind: EntryKind::File,
                parent: u32::MAX,
                name: name_offset,
                mode: 0o644,
                mtime_secs: 0,
                mtime_nsec: 0,
                content_hash: hash,
                blocks: vec![Block {
                    payload_offset: payload.len() as u64,
                    compressed_size: content.len() as u64,
                    original_size: content.len() as u64,
                    content_hash: hash,
                }],
                symlink_target: 0,
            });
            payload.extend_from_slice(content);
        }
        let manifest = Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: entries.len() as u32,
                string_table_size: string_table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: *blake3::hash(&[]).as_bytes(),
                footer_hash: [0; 32],
            },
            entrypoints: Vec::new(),
            entries,
            lib_dir_offsets: Vec::new(),
            string_table,
        };
        let raw = manifest.serialize().unwrap();
        let compressed = zstd::bulk::compress(&raw, 1).unwrap();
        let footer = Footer {
            format_version: onelf_format::footer::FORMAT_VERSION,
            flags: Flags::STORED,
            manifest_offset: 0,
            manifest_compressed: compressed.len() as u64,
            manifest_original: raw.len() as u64,
            payload_offset: compressed.len() as u64,
            payload_size: payload.len() as u64,
            dict_offset: 0,
            dict_size: 0,
            manifest_checksum: onelf_format::manifest::manifest_checksum(&raw).unwrap(),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(&payload);
        footer.write_to(&mut bytes).unwrap();
        bytes
    }

    /// Write one pinned build under a fresh directory and the set
    /// declaration pinning it by its whole-file digest.
    fn write_build(tag: &str, files: &[(&str, &[u8])]) -> (PathBuf, sysroot::SharedSet) {
        let dir = tmp(tag);
        let bytes = stored_package(files);
        let path = dir.join("set.onelf");
        fs::write(&path, &bytes).unwrap();
        let set = sysroot::SharedSet {
            name: "audio".to_string(),
            packages: vec!["alsa-lib".to_string()],
            url: format!("file://{}", path.display()),
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        };
        (dir, set)
    }

    fn at(base: SourceBase, path: &str) -> SourceCandidate {
        SourceCandidate {
            base,
            path: RelativePath::new(path).unwrap(),
        }
    }

    fn data_query<'a>(candidates: &'a [SourceCandidate]) -> SupplyQuery<'a> {
        SupplyQuery {
            rule: "alsa",
            group: "configuration",
            kind: ResourceKind::Directory,
            destination: "share/alsa",
            candidates,
            executable: false,
            hooks: &[BuildHookId::AlsaConfPatch],
            runtime: builtin_runtime("alsa"),
        }
    }

    /// The runtime path groups of the built-in rule `id`.
    fn builtin_runtime(id: &str) -> &'static [RuntimePathGroup] {
        &onelf_format::catalog::builtins()
            .iter()
            .find(|d| d.id.as_str() == id)
            .unwrap()
            .runtime
            .paths
    }

    fn helper_query<'a>(candidates: &'a [SourceCandidate]) -> SupplyQuery<'a> {
        SupplyQuery {
            rule: "gstreamer",
            group: "scanner",
            kind: ResourceKind::File,
            destination: "libexec/gstreamer-1.0/gst-plugin-scanner",
            candidates,
            executable: true,
            hooks: &[],
            runtime: builtin_runtime("gstreamer"),
        }
    }

    /// Supply one group from `sets` into an empty plan and walk the
    /// queued writes into `appdir`, the way a checked build does.
    fn prepare_and_walk(
        appdir: &Path,
        sets: &[sysroot::SharedSet],
        query: &SupplyQuery,
    ) -> io::Result<Vec<PreparedPin>> {
        let mut plan = DestinationPlan::load(appdir)?;
        let mut preparation = SetPreparation::new(sets, &[], Path::new("lib"), false);
        assert!(
            preparation.supply_group(&mut plan, query)?,
            "the declared set was expected to hold the resource"
        );
        assert!(!plan.has_findings());
        let mut actions = Vec::new();
        let prepared = preparation.queue_actions(&mut actions);
        execute_walk(
            appdir,
            false,
            false,
            &appdir.join("lib"),
            &actions,
            &plan,
            &mut WalkOutcome::default(),
        )?;
        Ok(prepared)
    }

    /// The files under `root`, relative and sorted.
    fn relative_files(root: &Path) -> Vec<String> {
        let mut got: Vec<String> = jwalk::WalkDir::new(root)
            .skip_hidden(false)
            .sort(true)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                e.path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        got.sort();
        got
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A set-supplied configuration directory is staged into the
    /// supplying pin's own subtree, layout preserved, and the
    /// registered hook transforms it there. Nothing outside the
    /// declared files is created, and no helper ran to do it.
    /// A resource is prepared only where the rule's runtime mapping
    /// would find it in the pin's subtree: a destination no runtime
    /// candidate names is left in the set, so a required group it would
    /// satisfy stays a miss. The prepared pin names the rule it binds.
    #[test]
    fn only_a_resource_a_runtime_mapping_reaches_is_prepared() {
        let appdir = tmp("unmapped-subtree");
        let (builddir, set) = write_build(
            "unmapped-set",
            &[("share/alsa/alsa.conf", b"# conf\n".as_slice())],
        );
        let candidates = [at(SourceBase::InstallationPrefix, "share/alsa")];
        let unmapped = SupplyQuery {
            runtime: &[],
            ..data_query(&candidates)
        };
        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(std::slice::from_ref(&set), &[], Path::new("lib"), false);
        assert!(!preparation.supply_group(&mut plan, &unmapped).unwrap());
        assert!(preparation.queue_actions(&mut Vec::new()).is_empty());

        let prepared = prepare_and_walk(
            &appdir,
            std::slice::from_ref(&set),
            &data_query(&candidates),
        )
        .unwrap();
        assert_eq!(prepared[0].rules, ["alsa"]);
        assert!(runtime_maps(
            builtin_runtime("gstreamer"),
            "lib/gstreamer-1.0/gst-plugin-scanner",
            Path::new("lib")
        ));
        let _ = fs::remove_dir_all(&appdir);
        let _ = fs::remove_dir_all(&builddir);
    }

    #[test]
    fn a_set_supplied_configuration_prepares_into_its_pins_subtree() {
        let appdir = tmp("alsa-subtree");
        let (builddir, set) = write_build(
            "alsa-set",
            &[
                (
                    "share/alsa/alsa.conf",
                    b"@hooks [\n\t{\n\t\tfunc load\n\t\tfiles [\n\t\t\t\"/etc/alsa/conf.d\"\n\t\t]\n\t}\n]\n",
                ),
                (
                    "share/alsa/alsa.conf.d/99-fixture.conf",
                    b"pcm.fixer type plug\n",
                ),
            ],
        );
        let candidates = [at(SourceBase::InstallationPrefix, "share/alsa")];
        let prepared = prepare_and_walk(
            &appdir,
            std::slice::from_ref(&set),
            &data_query(&candidates),
        )
        .expect("the supply");
        assert_eq!(prepared.len(), 1);
        let pin = &prepared[0];
        assert_eq!(pin.set, "audio");
        assert_eq!(pin.pin, set.blake3);
        assert_eq!(
            pin.root,
            Path::new(".onelf-prepared/audio").join(set.blake3.as_str())
        );

        let root = appdir.join(&pin.root);
        assert_eq!(
            relative_files(&root),
            [
                "share/alsa/alsa.conf",
                "share/alsa/alsa.conf.d/99-fixture.conf"
            ],
            "the subtree holds exactly the declared files"
        );
        let conf = fs::read_to_string(root.join("share/alsa/alsa.conf")).unwrap();
        assert!(
            conf.contains("{ @func datadir } \"/alsa.conf.d\""),
            "the registered hook anchored the subtree's own snippets: {conf}"
        );
        assert_eq!(
            fs::read_to_string(root.join("share/alsa/alsa.conf.d/99-fixture.conf")).unwrap(),
            "pcm.fixer type plug\n"
        );
        assert_eq!(mode_of(&root.join("share/alsa/alsa.conf")), 0o644);
        assert_eq!(pin.files.len(), 2);
        let _ = fs::remove_dir_all(&appdir);
        let _ = fs::remove_dir_all(&builddir);
    }

    /// A set-supplied helper that would need an interpreter from the
    /// packer's tree or the set's host is refused with guidance, a
    /// /bin/sh script and an ELF binary prepare with the executable
    /// mode, and the refused build left nothing behind.
    #[test]
    fn helpers_prepare_only_without_a_foreign_interpreter() {
        let appdir = tmp("helper-safety");
        let candidates = [
            at(SourceBase::LibraryDir, "gstreamer-1.0/gst-plugin-scanner"),
            at(
                SourceBase::InstallationPrefix,
                "libexec/gstreamer-1.0/gst-plugin-scanner",
            ),
        ];

        let (envdir, env_set) = write_build(
            "helper-env",
            &[(
                "lib/gstreamer-1.0/gst-plugin-scanner",
                b"#!/usr/bin/env python3\nimport sys\n",
            )],
        );
        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(std::slice::from_ref(&env_set), &[], Path::new("lib"), false);
        let err = preparation
            .supply_group(&mut plan, &helper_query(&candidates))
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("/usr/bin/env"), "{text}");
        assert!(text.contains("/bin/sh"), "{text}");
        assert!(
            text.contains("libexec/gstreamer-1.0/gst-plugin-scanner"),
            "{text}"
        );
        drop(preparation);
        drop(plan);
        assert!(
            !appdir.join(PREPARED_DIR).exists(),
            "a refused helper prepared nothing"
        );

        let (shdir, sh_set) = write_build(
            "helper-sh",
            &[(
                "lib/gstreamer-1.0/gst-plugin-scanner",
                b"#!/bin/sh\nexec \"$@\"\n",
            )],
        );
        let prepared = prepare_and_walk(
            &appdir,
            std::slice::from_ref(&sh_set),
            &helper_query(&candidates),
        )
        .expect("the /bin/sh helper");
        let dest = appdir
            .join(&prepared[0].root)
            .join("libexec/gstreamer-1.0/gst-plugin-scanner");
        assert_eq!(
            fs::read(&dest).unwrap(),
            b"#!/bin/sh\nexec \"$@\"\n",
            "the helper's bytes are the pinned build's bytes"
        );
        assert_eq!(mode_of(&dest), 0o755);

        let (elfdir, elf_set) = write_build(
            "helper-elf",
            &[(
                "lib/gstreamer-1.0/gst-plugin-scanner",
                b"\x7fELFfixture-scanner",
            )],
        );
        let prepared = prepare_and_walk(
            &appdir,
            std::slice::from_ref(&elf_set),
            &helper_query(&candidates),
        )
        .expect("the ELF helper");
        let dest = appdir
            .join(&prepared[0].root)
            .join("libexec/gstreamer-1.0/gst-plugin-scanner");
        assert_eq!(fs::read(&dest).unwrap(), b"\x7fELFfixture-scanner");
        assert_eq!(mode_of(&dest), 0o755);
        for dir in [&envdir, &shdir, &elfdir] {
            let _ = fs::remove_dir_all(dir);
        }
        let _ = fs::remove_dir_all(&appdir);
    }

    /// Bytes that are not the pin supply nothing, and a build re-pinned
    /// to tampered bytes passes the pin check but fails the verified
    /// read, naming the set and the path.
    #[test]
    fn tampered_set_bytes_are_refused_at_the_pin_or_the_read() {
        let appdir = tmp("tamper");
        let candidates = [at(SourceBase::InstallationPrefix, "share/alsa")];
        let (builddir, set) = write_build("tamper-set", &[("share/alsa/alsa.conf", b"conf\n")]);
        let build = builddir.join("set.onelf");
        let original = fs::read(&build).unwrap();

        let mut flipped = original.clone();
        let at = flipped.len() - onelf_format::FOOTER_SIZE - 1;
        flipped[at] ^= 0xff;
        fs::write(&build, &flipped).unwrap();

        let err = sysroot::VerifiedSet::open(&set)
            .err()
            .expect("a build that is not the pin must be refused");
        assert!(err.contains("not the pinned build"), "{err}");
        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(std::slice::from_ref(&set), &[], Path::new("lib"), false);
        assert!(
            !preparation
                .supply_group(&mut plan, &data_query(&candidates))
                .unwrap(),
            "an unpinned build supplies nothing"
        );
        drop(preparation);
        drop(plan);
        assert!(!appdir.join(PREPARED_DIR).exists());

        let corrupt = sysroot::SharedSet {
            blake3: blake3::hash(&flipped).to_hex().to_string(),
            ..set
        };
        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(std::slice::from_ref(&corrupt), &[], Path::new("lib"), false);
        let err = preparation
            .supply_group(&mut plan, &data_query(&candidates))
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("tampered or corrupt package"), "{text}");
        assert!(text.contains("audio"), "{text}");
        assert!(text.contains("alsa.conf"), "{text}");
        drop(preparation);
        drop(plan);
        assert!(!appdir.join(PREPARED_DIR).exists());
        let _ = fs::remove_dir_all(&appdir);
        let _ = fs::remove_dir_all(&builddir);
    }

    /// Two builds of one set name prepare side by side: each pin's
    /// subtree keeps its own version, neither overwrites the other,
    /// and the build's ledger lists both pins' files.
    #[test]
    fn two_versions_of_one_set_prepare_side_by_side() {
        let appdir = tmp("versions");
        let candidates = [at(SourceBase::InstallationPrefix, "share/alsa")];
        let (dir1, set1) = write_build("v1", &[("share/alsa/alsa.conf", b"conf v1\n")]);
        let (dir2, set2) = write_build("v2", &[("share/alsa/alsa.conf", b"conf v2\n")]);
        assert_ne!(set1.blake3, set2.blake3);

        let first = prepare_and_walk(&appdir, &[set1], &data_query(&candidates))
            .expect("the first version");
        sysroot::record_generated(
            &appdir,
            first.iter().flat_map(|p| p.files.iter().cloned()).collect(),
        )
        .unwrap();
        let second = prepare_and_walk(&appdir, &[set2], &data_query(&candidates))
            .expect("the second version");
        sysroot::record_generated(
            &appdir,
            second
                .iter()
                .flat_map(|p| p.files.iter().cloned())
                .collect(),
        )
        .unwrap();

        assert_ne!(first[0].root, second[0].root);
        assert_eq!(
            fs::read_to_string(appdir.join(&first[0].root).join("share/alsa/alsa.conf")).unwrap(),
            "conf v1\n",
            "the first pin's subtree keeps its version"
        );
        assert_eq!(
            fs::read_to_string(appdir.join(&second[0].root).join("share/alsa/alsa.conf")).unwrap(),
            "conf v2\n"
        );
        let ledger: Vec<String> = sysroot::read_generated(&appdir)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        for file in first[0].files.iter().chain(second[0].files.iter()) {
            assert!(ledger.contains(file), "{file} missing from {ledger:?}");
        }
        let _ = fs::remove_dir_all(&appdir);
        let _ = fs::remove_dir_all(&dir1);
        let _ = fs::remove_dir_all(&dir2);
    }

    /// A pin's subtree is keyed by the set name and the lowercase hex
    /// of the pin, and a bad set name or a pin that is no digest is
    /// refused.
    #[test]
    fn the_prepared_subtree_is_keyed_by_set_and_pin() {
        let pin = "AB12CD34".to_string() + &"0".repeat(56);
        let root = prepared_subtree("audio", &pin).unwrap();
        assert_eq!(
            root,
            Path::new(".onelf-prepared/audio").join(pin.to_ascii_lowercase())
        );
        assert!(prepared_subtree("bad name", &pin).is_err());
        assert!(prepared_subtree("audio", "not-a-digest").is_err());
    }

    /// A dry run consults no build even when one sits at the pin, and a
    /// build that cannot be had on pinned terms is a soft miss: the
    /// group stays unsupplied and nothing is written.
    #[test]
    fn a_dry_run_and_an_unreachable_build_prepare_nothing() {
        let appdir = tmp("no-fetch");
        let candidates = [at(SourceBase::InstallationPrefix, "share/alsa")];
        let (builddir, set) = write_build("dry", &[("share/alsa/alsa.conf", b"conf\n")]);

        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(std::slice::from_ref(&set), &[], Path::new("lib"), true);
        assert!(!preparation.active());
        assert!(
            !preparation
                .supply_group(&mut plan, &data_query(&candidates))
                .unwrap()
        );
        drop(preparation);
        drop(plan);
        assert!(!appdir.join(PREPARED_DIR).exists());

        let ghost = sysroot::SharedSet {
            url: format!("file://{}", builddir.join("absent.onelf").display()),
            ..set
        };
        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(std::slice::from_ref(&ghost), &[], Path::new("lib"), false);
        assert!(
            !preparation
                .supply_group(&mut plan, &data_query(&candidates))
                .unwrap()
        );
        drop(preparation);
        drop(plan);
        assert!(!appdir.join(PREPARED_DIR).exists());
        let _ = fs::remove_dir_all(&appdir);
        let _ = fs::remove_dir_all(&builddir);
    }

    /// A set a database-backed sysroot declares feeds the same
    /// preparation: its helper prepares into the pin's subtree with the
    /// executable mode, and a dry run over the sysroot list consults no
    /// build, the way a dry run over the bundle-level list does.
    #[test]
    fn a_sysroot_declared_set_supplies_through_the_same_preparation() {
        let appdir = tmp("sysroot-supply");
        let candidates = [at(
            SourceBase::LibraryDir,
            "gstreamer-1.0/gst-plugin-scanner",
        )];
        let (builddir, set) = write_build(
            "sysroot-set",
            &[(
                "lib/gstreamer-1.0/gst-plugin-scanner",
                b"\x7fELFfixture-scanner",
            )],
        );

        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(&[], std::slice::from_ref(&set), Path::new("lib"), true);
        assert!(!preparation.active());
        assert!(
            !preparation
                .supply_group(&mut plan, &helper_query(&candidates))
                .unwrap()
        );
        drop(preparation);
        drop(plan);
        assert!(!appdir.join(PREPARED_DIR).exists());

        let mut plan = DestinationPlan::load(&appdir).unwrap();
        let mut preparation =
            SetPreparation::new(&[], std::slice::from_ref(&set), Path::new("lib"), false);
        assert!(
            preparation
                .supply_group(&mut plan, &helper_query(&candidates))
                .unwrap(),
            "the sysroot's declared set holds the helper"
        );
        assert!(!plan.has_findings());
        let mut actions = Vec::new();
        let prepared = preparation.queue_actions(&mut actions);
        execute_walk(
            &appdir,
            false,
            false,
            &appdir.join("lib"),
            &actions,
            &plan,
            &mut WalkOutcome::default(),
        )
        .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].set, "audio");
        assert_eq!(prepared[0].pin, set.blake3);
        let dest = appdir
            .join(&prepared[0].root)
            .join("libexec/gstreamer-1.0/gst-plugin-scanner");
        assert_eq!(fs::read(&dest).unwrap(), b"\x7fELFfixture-scanner");
        assert_eq!(mode_of(&dest), 0o755);
        let _ = fs::remove_dir_all(&appdir);
        let _ = fs::remove_dir_all(&builddir);
    }
}

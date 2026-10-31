//! Populating an AppDir from a pinned sysroot.
//!
//! The application's package, everything it depends on, and the optional
//! dependencies the recipe names are copied out of the sysroot with
//! `usr/` flattened into the conventional AppDir layout. The platform
//! line, the policy and an optional trace decide what stays out. Nothing
//! from the packer's own machine is consulted.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use onelf_sysroot::path::{entry_in_root, in_root};
use onelf_sysroot::platform::{self, Pin};
use onelf_sysroot::prune::prune;
use onelf_sysroot::{Database, PlatformLine, Policy, Trace};

use super::elf::parse_needed;
use super::ui::color;

/// Where the bundle's contents come from and what is left out.
#[derive(Debug, Clone, Default)]
pub struct SysrootOptions {
    /// A materialized rootfs with its package database.
    pub root: PathBuf,
    /// The entrypoint relative to the AppDir, as in the recipe.
    pub command: String,
    /// The label recorded as the package's platform in its provenance.
    pub platform: String,
    /// Optional dependencies to include, by package name.
    pub optional: Vec<String>,
    /// Sonames the host provides, one prefix per line. Without a file the
    /// GPU driver families are the line, so Mesa stays out of the bundle
    /// unless the publisher writes a line that leaves it in.
    pub platform_line: Option<PathBuf>,
    /// Paths that never ship, one glob per line.
    pub policy: Option<PathBuf>,
    /// Paths a test run opened, one per line.
    pub trace: Option<PathBuf>,
    /// Globs the trace may not prune.
    pub keep: Option<PathBuf>,
    /// The GL build's URL, overriding the sysroot's own pin.
    pub platform_url: Option<String>,
    /// The GL build's BLAKE3 hash, overriding the sysroot's own pin.
    pub platform_hash: Option<String>,
    /// Dependency sets a pinned build supplies instead of the bundle.
    pub sets: Vec<SharedSet>,
}

/// A dependency set shared between packages: the closure of `packages`
/// stays out of the bundle, and the build at `url`, verified by `blake3`,
/// is what supplies it at launch.
#[derive(Debug, Clone)]
pub struct SharedSet {
    pub name: String,
    pub packages: Vec<String>,
    pub url: String,
    pub blake3: String,
}

/// What populating did, for the report.
#[derive(Debug, Default)]
pub struct SysrootReport {
    pub packages: Vec<(String, String)>,
    pub unsatisfied: Vec<(String, String)>,
    pub host_provided: Vec<String>,
    pub removed_platform: usize,
    pub removed_policy: usize,
    pub removed_trace: usize,
    pub copied: usize,
    /// Files the database lists that the sysroot does not hold.
    pub absent: usize,
    /// Packages owning a library on the platform line, left to the host
    /// whole along with whatever only they depended on.
    pub host_packages: Vec<String>,
    /// Every file of those packages, relative to the sysroot root, so the
    /// dependency walk leaves them alone as well.
    pub host_files: HashSet<String>,
    /// The GL build the package pins, when the sysroot or recipe names one.
    pub pin: Option<Pin>,
    /// Each shared set with the packages of its closure left to it.
    pub sets: Vec<(String, Vec<String>)>,
    /// Sonames a shared set provides, which the verifier accepts.
    pub set_sonames: HashSet<String>,
}

/// Copy the closure of the entrypoint's package into `appdir`. Returns
/// the report and the platform line, which the dependency walk needs too.
pub fn populate(appdir: &Path, opts: &SysrootOptions) -> io::Result<(SysrootReport, PlatformLine)> {
    let root = &opts.root;
    refuse_inside(root, appdir)?;
    let db = Database::read(root)?;
    let command = opts
        .command
        .trim_start_matches("./")
        .trim_start_matches('/');
    let candidates = [format!("usr/{command}"), command.to_string()];
    let owner = candidates.iter().find_map(|c| db.owner(c)).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{}: no package in the sysroot owns usr/{command} or {command}",
                root.display()
            ),
        )
    })?;

    // A name the recipe asks for that nothing provides is a typo or a
    // package the sysroot lacks, and either way the bundle would ship
    // without it.
    if let Some(name) = opts.optional.iter().find(|n| db.satisfier(n).is_none()) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{}: optional {name}: no package in the sysroot provides it",
                root.display()
            ),
        ));
    }

    let platform = match &opts.platform_line {
        Some(p) => PlatformLine::load(p)?,
        None => PlatformLine::from_prefixes(
            onelf_format::drivers::DRIVER_FAMILIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
    };
    // A package whose library the host supplies is the host's package: its
    // other files, Mesa's gallium and DRI drivers say, serve that library
    // and nothing in the bundle, and so does what only it depended on.
    let full = db.closure(&owner.name, &opts.optional);
    let host_packages: std::collections::BTreeSet<String> = full
        .packages
        .iter()
        .filter(|n| *n != &owner.name)
        .filter_map(|n| db.package(n))
        .filter(|p| {
            p.files
                .iter()
                .any(|f| onelf_sysroot::is_top_level_lib_object(f) && platform.matches(f))
        })
        .map(|p| p.name.clone())
        .collect();
    // A shared set's closure is left out the same way: its packages and
    // what only they depended on come from the set's build at launch.
    let mut set_packages: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut set_report: Vec<(String, Vec<String>)> = Vec::new();
    for set in &opts.sets {
        check_set_name(&set.name)?;
        let Some((first, rest)) = set.packages.split_first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("set {}: names no packages", set.name),
            ));
        };
        for name in &set.packages {
            if db.satisfier(name).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "set {}: no package in the sysroot provides {name}",
                        set.name
                    ),
                ));
            }
        }
        if set.packages.iter().any(|p| p == &owner.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "set {}: names the application's own package {}",
                    set.name, owner.name
                ),
            ));
        }
        platform::check_url(&set.url)
            .and_then(|()| platform::check_hash(&set.blake3))
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("set {}: {e}", set.name),
                )
            })?;
        // The set's build leaves out whatever the platform line hands
        // to the host, so a package reached only through one of those is
        // not in the set either. Reading the closure without that
        // exclusion promises libraries the set never carries, and the
        // bundle then omits them on the strength of the promise.
        let names: Vec<String> = db
            .closure_excluding(first, rest, &host_packages)
            .packages
            .into_iter()
            .filter(|n| n != "glibc" && n != &owner.name)
            .collect();
        set_packages.extend(names.iter().cloned());
        set_report.push((set.name.clone(), names));
    }
    let left_out: std::collections::BTreeSet<String> = host_packages
        .iter()
        .chain(set_packages.iter())
        .cloned()
        .collect();
    let host_files: HashSet<String> = left_out
        .iter()
        .filter_map(|n| db.package(n))
        .flat_map(|p| p.files.iter().cloned())
        .collect();
    // What the sets actually ship, read from the builds themselves where
    // they can be found. Inferring it from the sysroot a second time is
    // only close: the build drops what it cannot load, and the sysroot
    // it was made from may not be the one in hand.
    let mut set_sonames: HashSet<String> = HashSet::new();
    for set in &opts.sets {
        match shipped_sonames(set) {
            Ok(shipped) => set_sonames.extend(shipped),
            Err(why) => {
                eprintln!(
                    "  {} set {}: {why}, so what it holds is inferred from the sysroot",
                    color::bold("note:"),
                    set.name
                );
                set_sonames.extend(
                    db.closure_excluding(&set.packages[0], &set.packages[1..], &host_packages)
                        .packages
                        .iter()
                        .filter_map(|n| db.package(n))
                        .flat_map(|p| p.files.iter())
                        .filter(|f| onelf_sysroot::is_top_level_lib_object(f))
                        .filter_map(|f| f.rsplit('/').next().map(String::from)),
                );
            }
        }
    }
    let closure = db.closure_excluding(&owner.name, &opts.optional, &left_out);
    let files = db.files_of(&closure);
    let policy = opts.policy.as_deref().map(Policy::load).transpose()?;
    let trace = opts.trace.as_deref().map(Trace::load).transpose()?;
    let keep = opts.keep.as_deref().map(Policy::load).transpose()?;

    // Under a trace, a soname some surviving object needs survives too,
    // even when the test run never mapped it, and so on transitively:
    // what the survivors need is found by pruning, adding what they
    // name, and pruning again until nothing new appears.
    let mut keep_names: HashSet<String> = HashSet::new();
    let pruned = loop {
        let pruned = prune(
            &files,
            Some(&platform),
            policy.as_ref(),
            trace.as_ref(),
            &keep_names,
            keep.as_ref(),
        );
        if trace.is_none() {
            break pruned;
        }
        let before = keep_names.len();
        for rel in &pruned.kept {
            // A kept link keeps what it points to: `libtbb.so.12` is what
            // the loader asks for, `libtbb.so.12.14` is what it gets. By
            // path, so a link to one `index.theme` keeps no other.
            if let Some(target) = link_target(root, rel) {
                keep_names.insert(target);
            }
            let Ok(path) = in_root(root, Path::new(rel)) else {
                continue;
            };
            if is_elf(&path)
                && let Ok(needed) = parse_needed(&path)
            {
                keep_names.extend(needed);
            }
        }
        // And a link to a kept file is kept: the app asks the loader for
        // `libX11.so.6` and the run records `libX11.so.6.4.0`, since the
        // runtime followed the link before the app started and the
        // kernel does not ask again. So is a link to a directory holding
        // one, such as `X11/xkb -> /usr/share/xkeyboard-config-2`.
        let kept: HashSet<&str> = pruned.kept.iter().map(String::as_str).collect();
        let kept_dirs: HashSet<&str> = pruned
            .kept
            .iter()
            .flat_map(|rel| Path::new(rel).ancestors().skip(1))
            .filter_map(Path::to_str)
            .collect();
        for rel in &files {
            if !kept.contains(rel.as_str())
                && link_target(root, rel)
                    .is_some_and(|t| kept.contains(t.as_str()) || kept_dirs.contains(t.as_str()))
            {
                keep_names.insert(rel.clone());
            }
        }
        if keep_names.len() == before {
            break pruned;
        }
    };

    remove_generated(appdir)?;
    let mut generated: Vec<String> = Vec::new();
    // The host packages' own libraries on the line are reported as
    // host-provided like any other, so the report reads the same whether
    // a library left alone or with its package.
    let mut host_provided = pruned.host_provided.clone();
    let mut removed_platform = pruned.removed_platform;
    for rel in host_packages
        .iter()
        .filter_map(|n| db.package(n))
        .flat_map(|p| p.files.iter())
        .filter(|f| platform.matches(f))
    {
        removed_platform += 1;
        host_provided.push(rel.rsplit('/').next().unwrap_or(rel).to_string());
    }
    host_provided.sort();
    host_provided.dedup();

    let mut report = SysrootReport {
        packages: closure
            .packages
            .iter()
            .filter_map(|n| db.package(n))
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect(),
        unsatisfied: closure.unsatisfied.clone(),
        host_provided,
        removed_platform,
        removed_policy: pruned.removed_policy,
        removed_trace: pruned.removed_trace,
        host_packages: host_packages.into_iter().collect(),
        host_files,
        sets: set_report,
        set_sonames,
        ..Default::default()
    };
    for rel in &pruned.kept {
        match copy_entry(root, rel, appdir)? {
            Some(dest) => {
                report.copied += 1;
                generated.push(dest.to_string_lossy().into_owned());
            }
            None => report.absent += 1,
        }
    }
    let caches = copy_generated_caches(root, appdir)?;
    report.copied += caches.len();
    generated.extend(caches);
    let links = link_sonames(appdir)?;
    report.copied += links.len();
    generated.extend(links);
    record_generated(appdir, generated)?;
    let record = appdir.join(PROVENANCE_FILE);
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&record, render_provenance(&opts.platform, &report.packages))?;
    super::normalize_mtime(&record);

    report.pin = resolve_pin(opts)?;
    let pin_path = appdir.join(PLATFORM_FILE);
    match &report.pin {
        Some(pin) => {
            fs::write(&pin_path, render_pin(pin))?;
            super::normalize_mtime(&pin_path);
        }
        None => {
            let _ = fs::remove_file(&pin_path);
        }
    }
    let sets_path = appdir.join(SETS_FILE);
    if opts.sets.is_empty() {
        let _ = fs::remove_file(&sets_path);
    } else {
        fs::write(&sets_path, render_sets(&opts.sets, &report.sets))?;
        super::normalize_mtime(&sets_path);
    }
    Ok((report, platform))
}

/// Record what a `[sysroot]` without `path` pins, in an AppDir no
/// package database populates: the shared sets, and a GL build pin when
/// the recipe names one whole. The AppDir stays as the author assembled
/// it; these records are the only trace of the section, and they are
/// what the runtime fetches at launch. The files and their format are
/// the ones a populated build writes, so the runtime cannot tell the
/// two apart.
pub fn record_sets(appdir: &Path, sets: &[SharedSet], pin: Option<&Pin>) -> io::Result<()> {
    let records = [
        (
            SETS_FILE,
            (!sets.is_empty()).then(|| render_sets(sets, &[])),
        ),
        (PLATFORM_FILE, pin.map(render_pin)),
    ];
    for (name, text) in records {
        let path = appdir.join(name);
        match text {
            Some(text) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, text)?;
                super::normalize_mtime(&path);
            }
            None => {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// The GL build pin a recipe names without a sysroot: both fields are
/// the recipe's, and the label is the plain `sysroot` since no archive
/// or directory names a platform. Packages naming the same build share
/// one store directory under it.
pub fn pin_without_root(url: Option<&String>, hash: Option<&String>) -> io::Result<Option<Pin>> {
    let (Some(url), Some(hash)) = (url, hash) else {
        if url.is_some() || hash.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a GL build pin needs both platform-url and platform-hash",
            ));
        }
        return Ok(None);
    };
    let label = "sysroot".to_string();
    platform::check_label(&label)
        .and_then(|()| platform::check_url(url))
        .and_then(|()| platform::check_hash(hash))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(Some(Pin {
        label,
        url: url.clone(),
        blake3: hash.to_ascii_lowercase(),
    }))
}

/// The sysroot path the link at `rel` points to, with the directories on
/// the way resolved inside the sysroot, or `None` when `rel` is no link.
fn link_target(root: &Path, rel: &str) -> Option<String> {
    let target = fs::read_link(entry_in_root(root, Path::new(rel)).ok()?).ok()?;
    let named = Path::new(rel)
        .parent()
        .unwrap_or(Path::new(""))
        .join(target);
    let resolved = entry_in_root(root, &named).ok()?;
    Some(resolved.strip_prefix(root).ok()?.to_str()?.to_string())
}

/// Refuse a sysroot at `root` inside `appdir`. It would be packed along
/// with the bundle, and its library directories would be found by every
/// walk that looks for `.so` files. The check holds before the sysroot
/// exists, so an archive is never materialized there either.
pub fn refuse_inside(root: &Path, appdir: &Path) -> io::Result<()> {
    if let (Some(root_abs), Ok(appdir_abs)) = (absolute(root), appdir.canonicalize())
        && root_abs.starts_with(&appdir_abs)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: the sysroot must not be inside the AppDir {}",
                root.display(),
                appdir.display()
            ),
        ));
    }
    Ok(())
}

/// Refuse a shared set name a recipe could not refer to: a set is named
/// in `[sysroot.sets.<name>]` and in the paths built from it.
pub fn check_set_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("set {name}: a set name is letters, digits, - and _ only"),
        ));
    }
    Ok(())
}

/// `path` canonicalized through its deepest existing ancestor, with the
/// rest appended, so a path not created yet can still be compared.
fn absolute(path: &Path) -> Option<PathBuf> {
    let mut base = path.to_path_buf();
    let mut rest = Vec::new();
    let mut abs = loop {
        let probe = if base.as_os_str().is_empty() {
            Path::new(".")
        } else {
            base.as_path()
        };
        if let Ok(abs) = probe.canonicalize() {
            break abs;
        }
        rest.push(base.components().next_back()?.as_os_str().to_os_string());
        base.pop();
    };
    for part in rest.iter().rev() {
        if part == ".." {
            abs.pop();
        } else {
            abs.push(part);
        }
    }
    Some(abs)
}

/// Where the package records the shared sets it needs at launch,
/// relative to the AppDir: one table per set with the build's URL and
/// hash, and the packages left to it.
pub const SETS_FILE: &str = ".onelf/sets";

/// The name every shared object in a dependency build answers to, one
/// per line, written by `pack-set` and read by the packages that leave
/// the build's contents out of their own bundle.
pub const SHIPPED_FILE: &str = ".onelf/sonames";

/// Absolute paths the runtime makes bundle directories answer for, one
/// `ABSOLUTE<tab>RELATIVE` pair per line.
pub const PATHS_FILE: &str = ".onelf/paths";

/// The sets record as TOML.
pub fn render_sets(sets: &[SharedSet], closures: &[(String, Vec<String>)]) -> String {
    let mut out = String::new();
    for set in sets {
        let packages = closures
            .iter()
            .find(|(n, _)| n == &set.name)
            .map(|(_, p)| p.as_slice())
            .unwrap_or(&[]);
        // The name is a bare key: populate() refused anything else.
        out.push_str(&format!(
            "[{}]\nurl = {}\nblake3 = {}\npackages = [{}]\n\n",
            set.name,
            toml_string(&set.url),
            toml_string(&set.blake3),
            packages
                .iter()
                .map(|p| toml_string(p))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// What a GL build takes from a sysroot: the closure of the named
/// packages, kept to shared objects and the driver description
/// directories, with glibc's own files left out since the package that
/// uses the build carries its glibc.
pub struct GlSelection {
    /// The packages the closure holds, by name and version.
    pub packages: Vec<(String, String)>,
    pub copied: usize,
}

/// Directories under `usr/share` a driver stack reads.
const GL_SHARE_DIRS: &[&str] = &[
    "usr/share/vulkan/",
    "usr/share/glvnd/",
    "usr/share/drirc.d/",
    "usr/share/libdrm/",
];

/// Directories under `usr/share` that serve nothing at runtime and are
/// left out of a shared set.
const SET_SKIP_DIRS: &[&str] = &[
    "usr/include/",
    "usr/share/man/",
    "usr/share/doc/",
    "usr/share/info/",
    "usr/share/licenses/",
    "usr/share/gtk-doc/",
    "usr/lib/pkgconfig/",
    "usr/lib/cmake/",
    // Introspection XML, read by binding generators at build time. What
    // a running program reads is the compiled typelib beside it.
    "usr/share/gir-1.0/",
];

/// Materialize a shared set's tree at `tree` from the sysroot at `root`:
/// every file of the closure of `packages` apart from glibc's and the
/// development and documentation directories. A set carries whatever its
/// packages need at runtime, plugins and data included, since nothing
/// else supplies those for it.
///
/// The platform line applies here as it does to a bundle: a package
/// whose top-level library the host provides is the host's, and so is
/// whatever only it depended on. Without that a toolkit set drags in the
/// GPU stack and the compiler behind it, which is most of its size and
/// none of its purpose.
pub fn populate_set(
    tree: &Path,
    root: &Path,
    packages: &[String],
    platform: &PlatformLine,
) -> io::Result<GlSelection> {
    populate_closure(tree, root, packages, Some(platform), |rel| {
        !SET_SKIP_DIRS.iter().any(|d| rel.starts_with(d))
    })
}

/// Materialize a GL build tree at `tree` from the sysroot at `root`:
/// the closure of `packages`, the first of which anchors it.
pub fn populate_gl(tree: &Path, root: &Path, packages: &[String]) -> io::Result<GlSelection> {
    populate_closure(tree, root, packages, None, |rel| {
        let is_lib = rel.starts_with("usr/lib/") || rel.starts_with("lib/");
        let is_object = Path::new(rel)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(".so"));
        (is_lib && is_object) || GL_SHARE_DIRS.iter().any(|d| rel.starts_with(d))
    })
}

/// The closure of `packages` copied into `tree`, glibc's files always left
/// out and `wanted` deciding the rest.
fn populate_closure(
    tree: &Path,
    root: &Path,
    packages: &[String],
    platform: Option<&PlatformLine>,
    wanted: impl Fn(&str) -> bool,
) -> io::Result<GlSelection> {
    let Some((first, rest)) = packages.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "name at least one package to build from",
        ));
    };
    let db = Database::read(root)?;
    for name in packages {
        if db.satisfier(name).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: no package provides {name}", root.display()),
            ));
        }
    }
    let host_packages: std::collections::BTreeSet<String> = match platform {
        Some(platform) => db
            .closure(first, rest)
            .packages
            .iter()
            .filter(|n| !packages.contains(n))
            .filter_map(|n| db.package(n))
            .filter(|p| {
                p.files
                    .iter()
                    .any(|f| onelf_sysroot::is_top_level_lib_object(f) && platform.matches(f))
            })
            .map(|p| p.name.clone())
            .collect(),
        None => std::collections::BTreeSet::new(),
    };
    let closure = db.closure_excluding(first, rest, &host_packages);
    let glibc: HashSet<&str> = db
        .package("glibc")
        .map(|p| p.files.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let mut copied = 0;
    for rel in db.files_of(&closure) {
        if !glibc.contains(rel.as_str()) && wanted(&rel) && copy_entry(root, &rel, tree)?.is_some()
        {
            copied += 1;
        }
    }
    copied += copy_generated_caches(root, tree)?.len();
    copied += link_sonames(tree)?.len();
    Ok(GlSelection {
        packages: closure
            .packages
            .iter()
            .filter_map(|n| db.package(n))
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect(),
        copied,
    })
}

/// The store the runtime fetches set and GL builds into: the same root,
/// so a build the packer fetched is already there for a launch on this
/// machine.
fn store_root() -> Option<PathBuf> {
    match std::env::var_os("ONELF_PLATFORM_STORE").filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => onelf_format::cache_layout::resolve_root(rustix::process::getuid().as_raw(), None)
            .map(|root| onelf_format::cache_layout::platform_dir(&root)),
    }
}

/// A set's pinned build: where it already sits, or fetched over https
/// into the store. A file counts only when its hash is the pinned one,
/// since that is the build the launch will use; a fetch that does not
/// match is discarded. The error says why no build could be had.
fn set_build(set: &SharedSet) -> Result<PathBuf, String> {
    let pinned = |p: &Path| {
        p.is_file()
            && crate::sysroot_cmd::hash_file(p).is_ok_and(|h| h.eq_ignore_ascii_case(&set.blake3))
    };
    if let Some(path) = set.url.strip_prefix("file://") {
        let path = PathBuf::from(path);
        return if pinned(&path) {
            Ok(path)
        } else {
            Err(format!(
                "{} is missing or not the pinned build",
                path.display()
            ))
        };
    }
    if !set.url.starts_with("https://") {
        return Err(format!("{} is neither https:// nor file://", set.url));
    }
    let store = store_root()
        .ok_or("no store to fetch it into (set HOME, XDG_CACHE_HOME or ONELF_PLATFORM_STORE)")?;
    let stored = store.join(&set.name).join(format!("{}.onelf", set.blake3));
    if pinned(&stored) {
        return Ok(stored);
    }
    eprintln!(
        "  {} set {} from {}",
        color::bold("Fetching"),
        color::cyan(&set.name),
        set.url
    );
    if let Some(parent) = stored.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let staged = stored.with_extension(format!("part-{}", std::process::id()));
    let fetched = crate::sysroot_cmd::download(&set.url, &staged)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            if pinned(&staged) {
                fs::rename(&staged, &stored).map_err(|e| format!("{}: {e}", stored.display()))
            } else {
                Err(format!(
                    "{} does not hash to the pinned {}",
                    set.url, set.blake3
                ))
            }
        });
    if fetched.is_err() {
        let _ = fs::remove_file(&staged);
    }
    fetched.map(|()| stored)
}

/// The sonames a set's pinned build records, or why they cannot be read.
fn shipped_sonames(set: &SharedSet) -> Result<Vec<String>, String> {
    let build = set_build(set)?;
    let bytes = crate::metadata::read_packed_file(&build, SHIPPED_FILE)
        .map_err(|e| format!("{}: {e}", build.display()))?
        .ok_or_else(|| format!("{} records no sonames", build.display()))?;
    let text = String::from_utf8(bytes).map_err(|e| format!("{}: {e}", build.display()))?;
    Ok(text
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// The sonames a set-only build's pinned sets supply, split out of the
/// verifier's findings and reported as theirs.
///
/// Each set's pinned build is read for `.onelf/sonames`, fetched first
/// when it is not at hand, so a name is attributed to a set only when
/// that set's build ships it. A set whose build cannot be had is
/// returned with the reason, and claims nothing: what it may supply
/// stays among the findings rather than being taken on the recipe's
/// word.
pub(crate) fn take_set_provides(
    sets: &[SharedSet],
    findings: &mut Vec<(PathBuf, Vec<String>)>,
) -> (Vec<SetProvides>, Vec<(String, String)>) {
    let mut known: Vec<(String, HashSet<String>)> = Vec::new();
    let mut unchecked: Vec<(String, String)> = Vec::new();
    for set in sets {
        match shipped_sonames(set) {
            Ok(names) => known.push((set.name.clone(), names.into_iter().collect())),
            Err(why) => unchecked.push((set.name.clone(), why)),
        }
    }
    (partition_set_provides(&known, findings), unchecked)
}

/// What one set provides, for the report.
#[derive(Debug)]
pub(crate) struct SetProvides {
    pub set: String,
    pub sonames: BTreeSet<String>,
}

/// Remove from `findings` every soname a set's build ships, and return
/// them per set, in declaration order.
fn partition_set_provides(
    known: &[(String, HashSet<String>)],
    findings: &mut Vec<(PathBuf, Vec<String>)>,
) -> Vec<SetProvides> {
    let mut provides: Vec<SetProvides> = known
        .iter()
        .map(|(name, _)| SetProvides {
            set: name.clone(),
            sonames: BTreeSet::new(),
        })
        .collect();
    for (_, libs) in findings.iter_mut() {
        libs.retain(
            |soname| match known.iter().position(|(_, names)| names.contains(soname)) {
                Some(i) => {
                    provides[i].sonames.insert(soname.clone());
                    false
                }
                None => true,
            },
        );
    }
    findings.retain(|(_, libs)| !libs.is_empty());
    provides.retain(|p| !p.sonames.is_empty());
    provides
}

/// Print what the pinned sets supply, and which could not be checked,
/// ahead of the remaining findings.
pub(crate) fn report_set_provides(provides: &[SetProvides], unchecked: &[(String, String)]) {
    for p in provides {
        eprintln!(
            "  {} {}: {}",
            color::bold("Provided by set"),
            color::cyan(&p.set),
            p.sonames.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    for (set, why) in unchecked {
        eprintln!(
            "  {} set {} could not be checked ({why}); what it supplies is \
             listed below as missing",
            color::bold("warning:"),
            color::cyan(set)
        );
    }
}

/// Rewrite the absolute module paths in a copied cache to paths relative
/// to the cache itself, and drop the entries for modules the tree does
/// not carry.
///
/// A distribution generates these caches with the paths its own
/// filesystem uses, `/usr/lib/gdk-pixbuf-2.0/...` and the like. Copied
/// unchanged into a bundle they name the host's modules, and the host's
/// modules are built against the host's libc: on a musl host a GTK
/// application then tries to load a musl module into a glibc process and
/// says so, repeatedly, while quietly losing its image loaders and input
/// methods. Both gdk-pixbuf and GTK resolve a relative path against the
/// directory the cache sits in, which is what makes the bundle's own
/// copies findable wherever the bundle ends up. A module the bundle left
/// out has no copy to name, and its absolute path would load the host's,
/// so its entry goes: each starts with a line holding only the module's
/// quoted path and runs to the next blank line.
fn relocate_cache(cache: &Path, tree: &Path) -> io::Result<()> {
    let Ok(text) = fs::read_to_string(cache) else {
        return Ok(()); // a binary cache has no paths to rewrite
    };
    let Some(dir) = cache.parent() else {
        return Ok(());
    };
    let mut out = String::with_capacity(text.len());
    let mut dropping = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end();
        if dropping {
            dropping = !body.is_empty();
            continue;
        }
        let module = body
            .strip_prefix('"')
            .and_then(|b| b.strip_suffix('"'))
            .filter(|m| m.starts_with('/') && !m.contains('"'));
        if let Some(module) = module {
            match relative_in_tree(module, dir, tree) {
                Some(relative) => {
                    out.push('"');
                    out.push_str(&relative);
                    out.push('"');
                    out.push_str(&line[body.len()..]);
                }
                None => dropping = true,
            }
            continue;
        }
        out.push_str(&relocate_paths(line, dir, tree));
    }
    if out != text {
        fs::write(cache, out)?;
    }
    Ok(())
}

/// `line` with every quoted absolute path the tree holds made relative
/// to `dir`.
fn relocate_paths(line: &str, dir: &Path, tree: &Path) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("\"/") {
        let (before, after) = rest.split_at(start + 1);
        out.push_str(before);
        let Some(end) = after.find('"') else {
            rest = after;
            break;
        };
        let (path, tail) = after.split_at(end);
        match relative_in_tree(path, dir, tree) {
            Some(relative) => out.push_str(&relative),
            None => out.push_str(path),
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// The path `absolute` names, as the cache in `dir` should spell it, when
/// the tree actually holds that file.
fn relative_in_tree(absolute: &str, dir: &Path, tree: &Path) -> Option<String> {
    let mapped = appdir_path(absolute.trim_start_matches('/'));
    let target = tree.join(Path::new(&mapped));
    if !target.is_file() {
        return None;
    }
    let from = dir.strip_prefix(tree).ok()?;
    let to = Path::new(&mapped);
    Some(relative_path(from, to).to_string_lossy().into_owned())
}

/// Recreate the soname links a distribution leaves to `ldconfig`, and
/// return what was made, relative to `dir`.
///
/// A package ships the versioned file and usually a development symlink
/// and stops there: `libfoo.so` and `libfoo.so.1.2.3` are its files, and
/// `libfoo.so.1`, the name every dependant asks for, is made by
/// `ldconfig` when the package is installed. No package database records
/// it, so copying what the database lists loses exactly the name that
/// matters.
fn link_sonames(dir: &Path) -> io::Result<Vec<String>> {
    let mut made = Vec::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false).sort(true) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(soname) = super::elf::parse_soname(&path) else {
            continue;
        };
        if path.file_name().and_then(|n| n.to_str()) == Some(soname.as_str()) {
            continue;
        }
        // A soname naming a directory of its own is not a name the
        // loader resolves beside the file, so there is nothing to make.
        if soname.contains('/') || soname.is_empty() {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        let link = parent.join(&soname);
        if link.exists() || link.is_symlink() {
            continue;
        }
        let Some(target) = path.file_name() else {
            continue;
        };
        std::os::unix::fs::symlink(target, &link)?;
        if let Ok(rel) = link.strip_prefix(dir) {
            made.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(made)
}

/// Caches a distribution generates after installing a package rather
/// than shipping inside it. No package owns them, so a closure never
/// names them, and the library that reads each one treats a missing
/// cache as an empty index rather than an error: a GTK application whose
/// compiled schema cache is absent exits at startup saying its schema is
/// not installed, and one whose icon cache is absent draws no icons.
const GENERATED_CACHES: &[&str] = &[
    "gschemas.compiled",
    "icon-theme.cache",
    "mime.cache",
    "mimeinfo.cache",
    "loaders.cache",
    "immodules.cache",
    "giomodule.cache",
];

/// Copy every generated cache the sysroot holds for a directory the tree
/// already carries, and return what was copied, relative to the tree.
///
/// Tied to the directories the closure produced: a cache is an index of
/// what sits beside it, so one arrives only where its subject already
/// did. The sysroot's own copy is used rather than a freshly built one,
/// since building it would mean running a tool from the packer's machine
/// over the sysroot's data.
fn copy_generated_caches(root: &Path, tree: &Path) -> io::Result<Vec<String>> {
    let mut copied = Vec::new();
    for entry in jwalk::WalkDir::new(tree).skip_hidden(false).sort(true) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let dir = entry.path();
        let Ok(rel) = dir.strip_prefix(tree) else {
            continue;
        };
        for name in GENERATED_CACHES {
            let dest = dir.join(name);
            if dest.exists() {
                continue;
            }
            // The AppDir flattens `usr/`, so a tree directory answers to
            // either shape in the sysroot.
            let src = ["usr", ""]
                .iter()
                .filter_map(|prefix| in_root(root, &Path::new(prefix).join(rel).join(name)).ok())
                .find(|p| p.is_file());
            let Some(src) = src else { continue };
            fs::copy(&src, &dest)?;
            relocate_cache(&dest, tree)?;
            super::normalize_mtime(&dest);
            copied.push(rel.join(name).to_string_lossy().into_owned());
        }
    }
    Ok(copied)
}

/// Where a sysroot build lists what it put into the AppDir, relative to
/// the AppDir. The next build removes those files first, so a changed
/// policy or closure takes effect without touching anything the
/// publisher added by hand. Never packed.
pub const GENERATED_FILE: &str = ".onelf/generated";

/// Remove what the previous sysroot build generated, and the directories
/// that emptied as a result.
pub fn remove_generated(appdir: &Path) -> io::Result<()> {
    let record = appdir.join(GENERATED_FILE);
    let text = match fs::read_to_string(&record) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    // The record sits in the AppDir and anyone can edit it, so a line
    // that is not a plain path inside the AppDir is no file of ours.
    let inside = |line: &&str| {
        !line.is_empty()
            && Path::new(line)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
    };
    for line in text.lines().filter(inside) {
        let path = appdir.join(line);
        let _ = fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            dirs.push(parent.to_path_buf());
        }
    }
    // Deepest first, so a directory is tried after its children.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for dir in dirs {
        let mut dir = dir.as_path();
        while dir != appdir && fs::remove_dir(dir).is_ok() {
            let Some(parent) = dir.parent() else { break };
            dir = parent;
        }
    }
    fs::remove_file(&record)
}

/// Add `paths`, relative to the AppDir, to the record of generated files.
pub fn record_generated(appdir: &Path, paths: Vec<String>) -> io::Result<()> {
    let record = appdir.join(GENERATED_FILE);
    let mut all: Vec<String> = fs::read_to_string(&record)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    all.extend(paths);
    all.sort();
    all.dedup();
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = all.join("\n");
    text.push('\n');
    fs::write(&record, text)
}

/// Where the package records the GL build it pins, relative to the
/// AppDir. Read by the runtime, unlike the provenance record.
pub const PLATFORM_FILE: &str = ".onelf/platform";

/// The pin the package carries: the sysroot's, with the recipe's URL and
/// hash overriding field by field. The label is the sysroot's, so every
/// package built on it shares one store directory; only a pin the recipe
/// supplies whole takes the package's platform label.
fn resolve_pin(opts: &SysrootOptions) -> io::Result<Option<Pin>> {
    let from_sysroot = platform::read(&opts.root)?;
    let url = opts
        .platform_url
        .clone()
        .or_else(|| from_sysroot.as_ref().map(|p| p.url.clone()));
    let blake3 = opts
        .platform_hash
        .as_ref()
        .map(|h| h.to_ascii_lowercase())
        .or_else(|| from_sysroot.as_ref().map(|p| p.blake3.clone()));
    let (Some(url), Some(blake3)) = (url, blake3) else {
        if opts.platform_url.is_some() || opts.platform_hash.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a GL build pin needs both platform-url and platform-hash",
            ));
        }
        return Ok(None);
    };
    let label = from_sysroot
        .map(|p| p.label)
        .unwrap_or_else(|| opts.platform.clone());
    platform::check_label(&label)
        .and_then(|()| platform::check_url(&url))
        .and_then(|()| platform::check_hash(&blake3))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(Some(Pin { label, url, blake3 }))
}

/// The `.onelf/platform` record: three `key = "value"` lines the runtime
/// reads without a TOML parser.
pub fn render_pin(pin: &Pin) -> String {
    format!(
        "label = {}\nurl = {}\nblake3 = {}\n",
        toml_string(&pin.label),
        toml_string(&pin.url),
        toml_string(&pin.blake3)
    )
}

/// Where the bundle records what it was built from, relative to the
/// AppDir. Read by `onelf info`, never by the runtime.
pub const PROVENANCE_FILE: &str = ".onelf/provenance.toml";

/// The provenance record: the platform label and every contributing
/// package with its version, in name order.
pub fn render_provenance(platform: &str, packages: &[(String, String)]) -> String {
    let mut out = format!("platform = {}\n", toml_string(platform));
    for (name, version) in packages {
        out.push_str(&format!(
            "\n[[package]]\nname = {}\nversion = {}\n",
            toml_string(name),
            toml_string(version)
        ));
    }
    out
}

/// A basic TOML string: quotes, backslashes and control characters
/// escaped, so a package name cannot open a new table.
fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The label a sysroot gets when the recipe names none: the archive's
/// file name, or the directory's.
pub fn default_label(root: &Path, archive: Option<&Path>) -> String {
    archive
        .and_then(|a| a.file_name())
        .or_else(|| root.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sysroot".to_string())
}

/// The AppDir path a sysroot path lands at: `usr/` is the prefix the
/// conventional layout leaves out.
fn appdir_path(rel: &str) -> &str {
    rel.strip_prefix("usr/").unwrap_or(rel)
}

/// Copy one sysroot entry into the AppDir, returning its AppDir-relative
/// path, or `None` for an entry the sysroot lacks or a link that folds
/// onto itself.
fn copy_entry(root: &Path, rel: &str, appdir: &Path) -> io::Result<Option<PathBuf>> {
    // The path comes from the sysroot's own package database, which a
    // fetched archive supplies. The archive extractor refuses entries that
    // leave the sysroot; the database inside it gets the same check here,
    // since a `..` in it would read outside the sysroot and write outside
    // the AppDir.
    if !Path::new(rel).components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{rel}: package database path leaves the sysroot"),
        ));
    }
    // Every directory on the way is resolved inside the sysroot, so a
    // link among them that names an absolute path names the sysroot's.
    let src = entry_in_root(root, Path::new(rel))?;
    let Ok(md) = fs::symlink_metadata(&src) else {
        return Ok(None);
    };
    let dest_rel = PathBuf::from(appdir_path(rel));
    let dest = appdir.join(&dest_rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if md.file_type().is_symlink() {
        let target = fs::read_link(&src)?;
        let Some(target) = relink(rel, &target) else {
            // A link that flattening folds onto itself, such as the
            // `bin -> usr/bin` compatibility links a rootfs carries.
            return Ok(None);
        };
        let target_str = target.to_string_lossy();
        if !onelf_format::symlink_target_within_root(&dest_rel, &target_str) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{rel}: symlink target {} leaves the bundle",
                    target.display()
                ),
            ));
        }
        match fs::symlink_metadata(&dest) {
            Ok(existing) if existing.is_dir() => return Ok(None),
            Ok(_) => fs::remove_file(&dest)?,
            Err(_) => {}
        }
        std::os::unix::fs::symlink(&target, &dest)?;
        return Ok(Some(dest_rel));
    }
    if !md.file_type().is_file() {
        return Ok(None);
    }
    if fs::symlink_metadata(&dest).is_ok() {
        fs::remove_file(&dest)?;
    }
    fs::copy(&src, &dest)
        .map_err(|e| io::Error::new(e.kind(), format!("copying {rel} from the sysroot: {e}")))?;
    super::normalize_mtime(&dest);
    Ok(Some(dest_rel))
}

/// A symlink target as it must read at the link's new location, or
/// `None` when flattening folds the link onto itself.
///
/// The target is resolved in sysroot space first, then mapped through the
/// same `usr/` flattening as the link, then made relative to the link's
/// directory. A relative target is taken relative to the link's directory
/// in the sysroot, which is what the loader would do.
fn relink(link_rel: &str, target: &Path) -> Option<PathBuf> {
    let link = Path::new(link_rel);
    let resolved = if target.is_absolute() {
        normalize(target.strip_prefix("/").unwrap_or(target))
    } else {
        normalize(&link.parent().unwrap_or(Path::new("")).join(target))
    };
    let mapped = PathBuf::from(appdir_path(&resolved.to_string_lossy()));
    let dest = PathBuf::from(appdir_path(link_rel));
    if mapped == dest {
        return None;
    }
    let from = dest.parent().unwrap_or(Path::new(""));
    Some(relative_path(from, &mapped))
}

/// `path` with `.` and `..` components folded, without touching the
/// filesystem. A `..` at the root is dropped rather than escaping.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// `to` expressed relative to the directory `from`, both relative to the
/// same root.
fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn is_elf(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    matches!(f.read(&mut magic), Ok(4)) && magic == *b"\x7fELF"
}

pub fn print_report(opts: &SysrootOptions, report: &SysrootReport) {
    eprintln!("{} {}", color::bold("Sysroot:"), opts.root.display());
    eprintln!(
        "  {} ({}): {}",
        color::bold("Packages"),
        report.packages.len(),
        report
            .packages
            .iter()
            .map(|(n, v)| format!("{n} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "  {} {} files; left out: {} on the platform line, {} by policy, {} by trace{}",
        color::bold("Copied"),
        report.copied,
        report.removed_platform,
        report.removed_policy,
        report.removed_trace,
        if report.absent > 0 {
            format!("; {} listed but absent from the sysroot", report.absent)
        } else {
            String::new()
        }
    );
    if !report.host_packages.is_empty() {
        eprintln!(
            "  {} {}",
            color::bold("Host packages:"),
            report.host_packages.join(", ")
        );
    } else if !report.packages.is_empty() {
        // Silence here is a decision, not an accident: say that nothing
        // on the platform line matched the closure, so a stale line is
        // visible instead of looking like it never ran.
        eprintln!(
            "  {} none; the bundle ships every library in the closure",
            color::bold("Host packages:")
        );
    }
    if !report.host_provided.is_empty() {
        eprintln!(
            "  {} {}",
            color::bold("Host-provided:"),
            report.host_provided.join(", ")
        );
    }
    if let Some(pin) = &report.pin {
        eprintln!(
            "  {} {} from {} ({}...)",
            color::bold("GL build:"),
            pin.label,
            pin.url,
            &pin.blake3[..16]
        );
    }
    for (name, packages) in &report.sets {
        eprintln!(
            "  {} {} leaves out {} package(s): {}",
            color::bold("Shared set:"),
            name,
            packages.len(),
            packages.join(", ")
        );
    }
    for (dep, by) in &report.unsatisfied {
        eprintln!(
            "  {} {dep} (needed by {by}) is not installed in the sysroot",
            color::bold_red("warning:")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module cache names the packaging distribution's own filesystem.
    /// Left that way it sends the bundle to the host's modules, which on
    /// a musl host cannot be loaded into a glibc process at all, and so
    /// does the entry of a module the bundle left out.
    #[test]
    fn a_module_cache_is_rewritten_to_reach_the_bundles_own_copies() {
        let root =
            std::env::temp_dir().join(format!("onelf-cache-{}-{}", std::process::id(), line!()));
        let dir = root.join("lib/gdk-pixbuf-2.0/2.10.0");
        std::fs::create_dir_all(dir.join("loaders")).unwrap();
        std::fs::write(dir.join("loaders/libpixbufloader_svg.so"), b"elf").unwrap();
        let cache = dir.join("loaders.cache");
        std::fs::write(
            &cache,
            "# LoaderDir = /usr/lib/gdk-pixbuf-2.0/2.10.0/loaders\n\
             \"/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/absent.so\"\n\
             \"absent\" 6 \"gdk-pixbuf\" \"/usr/share/locale\"\n\
             \n\
             \"/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/libpixbufloader_svg.so\" \n\
             \"svg\" 6 \"gdk-pixbuf\" \"/usr/share/locale\"\n\
             \n",
        )
        .unwrap();

        relocate_cache(&cache, &root).unwrap();

        let got = std::fs::read_to_string(&cache).unwrap();
        assert_eq!(
            got,
            "# LoaderDir = /usr/lib/gdk-pixbuf-2.0/2.10.0/loaders\n\
             \"loaders/libpixbufloader_svg.so\" \n\
             \"svg\" 6 \"gdk-pixbuf\" \"/usr/share/locale\"\n\
             \n",
            "the bundle's own copy is named, and a module it lacks loses its entry"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn symlink_targets_are_remapped_relative_to_the_link() {
        let some = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            relink("usr/lib/libfoo.so", Path::new("libfoo.so.1")),
            some("libfoo.so.1")
        );
        assert_eq!(
            relink("usr/lib/libfoo.so", Path::new("/usr/lib/libfoo.so.1")),
            some("libfoo.so.1")
        );
        assert_eq!(
            relink("usr/bin/tool", Path::new("/usr/lib/tool/bin/tool")),
            some("../lib/tool/bin/tool")
        );
        assert_eq!(
            relink("usr/bin/tool", Path::new("../lib/tool/bin/tool")),
            some("../lib/tool/bin/tool")
        );
        assert_eq!(relink("lib64", Path::new("usr/lib")), some("lib"));
        assert_eq!(relink("usr/lib64", Path::new("lib")), some("lib"));
        assert_eq!(relink("usr/sbin", Path::new("bin")), some("bin"));
        // A link to its own directory, as `..//lib` from `lib/gcc4.8`.
        assert_eq!(
            relink("opt/tbb/lib/gcc4.8", Path::new("..//lib")),
            some(".")
        );
        // The compatibility links a rootfs carries fold onto themselves.
        assert_eq!(relink("bin", Path::new("usr/bin")), None);
        assert_eq!(relink("lib", Path::new("usr/lib")), None);
    }

    #[test]
    fn a_database_path_leaving_the_sysroot_is_refused() {
        let root = std::env::temp_dir().join(format!("onelf-dbpath-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sysroot")).unwrap();
        fs::create_dir_all(root.join("appdir")).unwrap();
        fs::write(root.join("outside"), b"secret").unwrap();
        for rel in ["../outside", "usr/../../outside", "/outside"] {
            let err = copy_entry(&root.join("sysroot"), rel, &root.join("appdir")).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{rel}");
        }
        assert!(
            copy_entry(
                &root.join("sysroot"),
                "usr/lib/missing",
                &root.join("appdir")
            )
            .unwrap()
            .is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A directory link in the sysroot that names an absolute path names
    /// the sysroot's directory, so a database path through it never reads
    /// the packer's files.
    #[test]
    fn a_database_path_through_an_absolute_link_stays_in_the_sysroot() {
        let root = std::env::temp_dir().join(format!("onelf-dblink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sysroot = root.join("sysroot");
        fs::create_dir_all(sysroot.join("usr/share")).unwrap();
        fs::create_dir_all(root.join("home/me")).unwrap();
        fs::create_dir_all(root.join("appdir")).unwrap();
        fs::write(root.join("home/me/secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(root.join("home"), sysroot.join("usr/share/x")).unwrap();

        let copied = copy_entry(&sysroot, "usr/share/x/me/secret", &root.join("appdir")).unwrap();
        assert!(copied.is_none(), "{copied:?}");
        assert!(!root.join("appdir/share/x/me/secret").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sysroot_inside_the_appdir_is_refused_before_it_exists() {
        let root = std::env::temp_dir().join(format!("onelf-inside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(&appdir).unwrap();
        for sysroot in [
            "appdir/sysroot",
            "appdir/a/b/../sysroot",
            "other/../appdir/x",
        ] {
            assert!(
                refuse_inside(&root.join(sysroot), &appdir).is_err(),
                "{sysroot}"
            );
        }
        for sysroot in ["sysroot", "appdir/../sysroot", "appdir-2/sysroot"] {
            assert!(
                refuse_inside(&root.join(sysroot), &appdir).is_ok(),
                "{sysroot}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_generated_record_never_reaches_outside_the_appdir() {
        let root = std::env::temp_dir().join(format!("onelf-generated-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib/sub")).unwrap();
        fs::create_dir_all(root.join("outside/empty")).unwrap();
        fs::write(appdir.join("lib/sub/libfoo.so"), b"elf").unwrap();
        fs::write(root.join("outside/keep"), b"keep").unwrap();
        fs::create_dir_all(appdir.join(".onelf")).unwrap();
        fs::write(
            appdir.join(GENERATED_FILE),
            format!(
                "lib/sub/libfoo.so\n../outside/keep\nlib/../../outside/empty/x\n{}\n",
                root.join("outside/keep").display()
            ),
        )
        .unwrap();

        remove_generated(&appdir).unwrap();
        assert!(!appdir.join("lib").exists(), "the emptied directories go");
        assert!(appdir.is_dir(), "the AppDir stays");
        assert!(root.join("outside/keep").is_file());
        assert!(root.join("outside/empty").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_link_target_is_the_sysroot_path_it_names() {
        let root = std::env::temp_dir().join(format!("onelf-linktarget-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("usr/share/icons/a")).unwrap();
        std::os::unix::fs::symlink("usr/lib", root.join("lib")).unwrap();
        std::os::unix::fs::symlink("libtbb.so.12.14", root.join("usr/lib/libtbb.so.12")).unwrap();
        std::os::unix::fs::symlink("/lib/libz.so.1.3", root.join("usr/lib/libz.so.1")).unwrap();
        std::os::unix::fs::symlink(
            "../b/index.theme",
            root.join("usr/share/icons/a/index.theme"),
        )
        .unwrap();
        let target = |rel| link_target(&root, rel);
        assert_eq!(
            target("usr/lib/libtbb.so.12").as_deref(),
            Some("usr/lib/libtbb.so.12.14")
        );
        assert_eq!(
            target("usr/lib/libz.so.1").as_deref(),
            Some("usr/lib/libz.so.1.3")
        );
        assert_eq!(
            target("usr/share/icons/a/index.theme").as_deref(),
            Some("usr/share/icons/b/index.theme")
        );
        assert_eq!(target("usr/lib"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_record_renders_to_fixed_bytes_with_escaping() {
        let packages = vec![
            ("app".to_string(), "1.0-1".to_string()),
            ("we\"ird".to_string(), "2\\3".to_string()),
        ];
        assert_eq!(
            render_provenance("platform-1", &packages),
            "platform = \"platform-1\"\n\n[[package]]\nname = \"app\"\nversion = \"1.0-1\"\n\n[[package]]\nname = \"we\\\"ird\"\nversion = \"2\\\\3\"\n"
        );
        assert_eq!(
            default_label(
                Path::new("/x/sysroot"),
                Some(Path::new("/y/platform-1.tar.zst"))
            ),
            "platform-1.tar.zst"
        );
        assert_eq!(default_label(Path::new("/x/sysroot"), None), "sysroot");
    }

    #[test]
    fn usr_is_flattened_and_nothing_else_is() {
        assert_eq!(appdir_path("usr/bin/app"), "bin/app");
        assert_eq!(appdir_path("usr/share/x"), "share/x");
        assert_eq!(appdir_path("etc/app.conf"), "etc/app.conf");
        assert_eq!(appdir_path("opt/app/run"), "opt/app/run");
    }

    /// A name goes to the set whose build ships it, and a name no build
    /// ships stays a finding: nothing is attributed on a recipe's word.
    #[test]
    fn set_provides_are_taken_from_the_findings() {
        let known = vec![
            (
                "qt".to_string(),
                HashSet::from(["libQt6Core.so.6".to_string()]),
            ),
            (
                "gtk".to_string(),
                HashSet::from(["libgtk-3.so.0".to_string()]),
            ),
        ];
        let mut findings = vec![
            (
                PathBuf::from("bin/app"),
                vec![
                    "libQt6Core.so.6".to_string(),
                    "libgdk-3.so.0".to_string(),
                    "libgtk-3.so.0".to_string(),
                ],
            ),
            (
                PathBuf::from("bin/helper"),
                vec!["libtruly.so.1".to_string()],
            ),
        ];

        let provides = partition_set_provides(&known, &mut findings);
        assert_eq!(provides.len(), 2, "{provides:?}");
        assert_eq!(provides[0].set, "qt");
        assert_eq!(
            provides[0].sonames.iter().cloned().collect::<Vec<_>>(),
            ["libQt6Core.so.6"]
        );
        assert_eq!(provides[1].set, "gtk");
        assert_eq!(
            provides[1].sonames.iter().cloned().collect::<Vec<_>>(),
            ["libgtk-3.so.0"]
        );
        assert_eq!(
            findings,
            vec![
                (PathBuf::from("bin/app"), vec!["libgdk-3.so.0".to_string()]),
                (
                    PathBuf::from("bin/helper"),
                    vec!["libtruly.so.1".to_string()]
                ),
            ],
            "what no build ships stays a finding"
        );
    }

    /// A build is checked against its pin before anything is read from
    /// it, and a set with no build to be had says why.
    #[test]
    fn a_set_build_must_match_its_pin() {
        let dir = std::env::temp_dir().join(format!("onelf-set-build-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("set.onelf");
        fs::write(&file, b"not the pinned build").unwrap();
        let set = |url: String| SharedSet {
            name: "gtk".to_string(),
            packages: vec!["gtk3".to_string()],
            url,
            blake3: "0".repeat(64),
        };

        let err = set_build(&set(format!("file://{}", file.display()))).unwrap_err();
        assert!(err.contains("not the pinned build"), "{err}");
        let err = set_build(&set("http://example.com/set.onelf".to_string())).unwrap_err();
        assert!(err.contains("neither https:// nor file://"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}

//! Populating an AppDir from a pinned sysroot.
//!
//! The application's package, everything it depends on, and the optional
//! dependencies the recipe names are copied out of the sysroot with
//! `usr/` flattened into the conventional AppDir layout. The platform
//! line, the policy and an optional trace decide what stays out. Nothing
//! from the packer's own machine is consulted.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

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
    // A sysroot inside the AppDir would be packed along with the bundle,
    // and its library directories would be found by every walk that
    // looks for `.so` files.
    if let (Ok(root_abs), Ok(appdir_abs)) = (root.canonicalize(), appdir.canonicalize())
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
    let is_top_level_object = |rel: &str| {
        let Some((dir, name)) = rel.rsplit_once('/') else {
            return false;
        };
        matches!(dir, "usr/lib" | "usr/lib64" | "lib" | "lib64") && name.contains(".so")
    };
    let host_packages: std::collections::BTreeSet<String> = full
        .packages
        .iter()
        .filter(|n| *n != &owner.name)
        .filter_map(|n| db.package(n))
        .filter(|p| {
            p.files
                .iter()
                .any(|f| is_top_level_object(f) && platform.matches(f))
        })
        .map(|p| p.name.clone())
        .collect();
    // A shared set's closure is left out the same way: its packages and
    // what only they depended on come from the set's build at launch.
    let mut set_packages: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut set_report: Vec<(String, Vec<String>)> = Vec::new();
    for set in &opts.sets {
        if set.name.is_empty()
            || !set
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "set {}: a set name is letters, digits, - and _ only",
                    set.name
                ),
            ));
        }
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
            Some(shipped) => set_sonames.extend(shipped),
            None => {
                eprintln!(
                    "  {} set {}: its build is not at hand, so what it holds is \
                     inferred from the sysroot; put the build beside the recipe \
                     or in ONELF_PLATFORM_STORE to read it instead",
                    color::bold("note:"),
                    set.name
                );
                set_sonames.extend(
                    db.closure_excluding(&set.packages[0], &set.packages[1..], &host_packages)
                        .packages
                        .iter()
                        .filter_map(|n| db.package(n))
                        .flat_map(|p| p.files.iter())
                        .filter(|f| is_top_level_object(f))
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
            let path = root.join(rel);
            // A kept link keeps what it points to: `libtbb.so.12` is what
            // the loader asks for, `libtbb.so.12.14` is what it gets.
            if let Ok(target) = fs::read_link(&path)
                && let Some(name) = target.file_name()
            {
                keep_names.insert(name.to_string_lossy().into_owned());
            }
            if is_elf(&path)
                && let Ok(needed) = parse_needed(&path)
            {
                keep_names.extend(needed);
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

/// Where the package records the shared sets it needs at launch,
/// relative to the AppDir: one table per set with the build's URL and
/// hash, and the packages left to it.
pub const SETS_FILE: &str = ".onelf/sets";

/// The name every shared object in a dependency build answers to, one
/// per line, written by `pack-set` and read by the packages that leave
/// the build's contents out of their own bundle.
pub const SHIPPED_FILE: &str = ".onelf/sonames";

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
                p.files.iter().any(|f| {
                    let Some((dir, name)) = f.rsplit_once('/') else {
                        return false;
                    };
                    matches!(dir, "usr/lib" | "usr/lib64" | "lib" | "lib64")
                        && name.contains(".so")
                        && platform.matches(f)
                })
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

/// The sonames a set's build records, when the build can be found.
///
/// Looked for where a build plausibly sits without going to the network:
/// a `file://` URL names one outright, and a store keyed by hash holds
/// the ones already fetched. Anything else leaves the caller to infer.
fn shipped_sonames(set: &SharedSet) -> Option<Vec<String>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = set.url.strip_prefix("file://") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(store) = std::env::var_os("ONELF_PLATFORM_STORE") {
        candidates.push(
            Path::new(&store)
                .join(&set.name)
                .join(format!("{}.onelf", set.blake3)),
        );
    }
    let text = candidates.iter().filter(|p| p.is_file()).find_map(|path| {
        crate::metadata::read_packed_file(path, SHIPPED_FILE)
            .ok()
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    })?;
    Some(
        text.lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
    )
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
                .map(|prefix| root.join(prefix).join(rel).join(name))
                .find(|p| p.is_file());
            let Some(src) = src else { continue };
            fs::copy(&src, &dest)?;
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
    for line in text.lines().filter(|l| !l.is_empty()) {
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
/// hash overriding field by field, under the package's own label.
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
    platform::check_url(&url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    platform::check_hash(&blake3).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(Some(Pin {
        label: opts.platform.clone(),
        url,
        blake3,
    }))
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

/// Copy one file or symlink. Returns false when the sysroot lacks it,
/// which a debloated rootfs does for files its database still lists.
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
    let src = root.join(rel);
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
}

//! Populating an AppDir from a pinned sysroot.
//!
//! The application's package, everything it depends on, and the optional
//! dependencies the recipe names are copied out of the sysroot with
//! `usr/` flattened into the conventional AppDir layout. The platform
//! line, the policy and an optional trace decide what stays out. Nothing
//! from the packer's own machine is consulted.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
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
    /// What the build admitted, which descriptor discovery draws from.
    pub(crate) universe: SysrootUniverse,
}

/// Why a sysroot build refuses a path descriptor discovery named. A
/// component the source never had is distinct from one the build
/// deliberately left out, and the report owes it that distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No package in the sysroot owns it and the sysroot does not hold
    /// it.
    Absent,
    /// A package the build left to the host or to a shared set whole
    /// owns it.
    HostPackage {
        /// The owning package's name.
        owner: String,
    },
    /// A package the recipe never selected owns it. Discovery never
    /// selects packages.
    UnselectedPackage {
        /// The owning package's name.
        owner: String,
    },
    /// A selected package owns it, but the policy or a trace removed it.
    Pruned {
        /// The owning package's name.
        owner: String,
    },
}

/// What a populated sysroot build admits, and why it refuses the rest.
///
/// Descriptor discovery draws its candidates from the file universe the
/// build put into the AppDir: the closure of the application's package
/// and the optional packages the recipe named, after the platform line,
/// the policy and a trace pruned it. Everything else the sysroot tree
/// may still hold is refused with the reason. The universe holds the
/// sysroot root and nothing beyond it, so a sysroot build's discovery
/// never reads the packer's own machine, never selects an unselected
/// optional package, and never restores what the policy or a trace
/// removed.
#[derive(Debug, Default)]
pub struct SysrootUniverse {
    /// The sysroot the universe answers for. Empty when the build had
    /// no sysroot.
    root: PathBuf,
    /// The sysroot-relative files the build copied.
    kept: HashSet<String>,
    /// The kept files' ancestor directories, so a directory is admitted
    /// exactly when it still holds admitted content.
    kept_dirs: HashSet<String>,
    /// The sysroot-relative files of packages the build left to the
    /// host or to a shared set whole.
    left_out: HashSet<String>,
    /// The admitted package names.
    closure: HashSet<String>,
    /// The sysroot's package database, which attributes a refusal to
    /// the package owning the refused path.
    db: Database,
}

impl SysrootUniverse {
    /// The universe a populated build admits: `kept` are the
    /// sysroot-relative files the closure and the pruning tiers left
    /// in, `left_out` the files of packages handed to the host or a
    /// shared set whole, and `closure` the admitted package names. The
    /// decisions are populate's; this only records them.
    pub(crate) fn admit(
        root: &Path,
        db: Database,
        kept: &[String],
        left_out: &HashSet<String>,
        closure: &[String],
    ) -> Self {
        let mut universe = Self {
            root: root.to_path_buf(),
            kept: kept.iter().cloned().collect(),
            left_out: left_out.clone(),
            closure: closure.iter().cloned().collect(),
            db,
            ..Default::default()
        };
        for file in &universe.kept {
            for dir in Path::new(file).ancestors().skip(1) {
                if dir.as_os_str().is_empty() {
                    continue;
                }
                universe
                    .kept_dirs
                    .insert(dir.to_string_lossy().into_owned());
            }
        }
        universe
    }

    /// The sysroot root the universe answers for. Discovery resolves
    /// every candidate against it and nothing outside it.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Whether a sysroot-relative path is admitted: a file the build
    /// copied, or a directory that still holds copied files.
    pub(crate) fn admits(&self, rel: &str) -> bool {
        let rel = rel.trim_start_matches('/');
        self.kept.contains(rel) || self.kept_dirs.contains(rel)
    }

    /// The admitted files under a sysroot-relative directory, sorted, so
    /// a directory candidate is walked no further than the build's own
    /// content and never restores what pruning removed.
    pub(crate) fn admitted_under(&self, dir: &str) -> Vec<String> {
        let prefix = format!("{}/", dir.trim_start_matches('/'));
        let mut files: Vec<String> = self
            .kept
            .iter()
            .filter(|f| f.starts_with(&prefix))
            .cloned()
            .collect();
        files.sort();
        files
    }

    /// Why a sysroot-relative path is not admitted. A path
    /// [`Self::admits`] answers for has no refusal. A directory is
    /// owned by no package, so its refusal is the content under it: the
    /// first owning package in the database's order speaks for it.
    pub(crate) fn refusal(&self, rel: &str) -> Refusal {
        let rel = rel.trim_start_matches('/');
        if let Some(owner) = self.db.owner(rel) {
            return self.classify(rel, &owner.name);
        }
        let prefix = format!("{rel}/");
        for package in self.db.packages() {
            for file in &package.files {
                if file.starts_with(&prefix) {
                    return self.classify(file, &package.name);
                }
            }
        }
        Refusal::Absent
    }

    /// The refusal for `file`, which `owner` owns, against the build's
    /// decisions.
    fn classify(&self, file: &str, owner: &str) -> Refusal {
        if self.left_out.contains(file) {
            Refusal::HostPackage {
                owner: owner.to_string(),
            }
        } else if self.closure.contains(owner) {
            Refusal::Pruned {
                owner: owner.to_string(),
            }
        } else {
            Refusal::UnselectedPackage {
                owner: owner.to_string(),
            }
        }
    }
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

    // Every destination the population writes is judged before the
    // disposal or the first copy touches the tree.
    let collisions = population_collisions(appdir, root, &pruned.kept)?;
    if !collisions.is_empty() {
        return Err(population_collision_error(&collisions));
    }
    let conflicts = dispose_generated(appdir, root, &pruned.kept)?;
    if !conflicts.is_empty() {
        return Err(generated_conflict_error(&conflicts));
    }
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

    let packages: Vec<(String, String)> = closure
        .packages
        .iter()
        .filter_map(|n| db.package(n))
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    // What this build admitted is what descriptor discovery may draw
    // from: the closure and the pruning tiers above decided it, and the
    // universe records the decision.
    let universe = SysrootUniverse::admit(root, db, &pruned.kept, &host_files, &closure.packages);
    let mut report = SysrootReport {
        packages,
        unsatisfied: closure.unsatisfied.clone(),
        host_provided,
        removed_platform,
        removed_policy: pruned.removed_policy,
        removed_trace: pruned.removed_trace,
        host_packages: host_packages.into_iter().collect(),
        host_files,
        sets: set_report,
        set_sonames,
        universe,
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
/// in `[sysroot.sets.<name>]` and in the paths built from it, so the name
/// follows the one set-name rule every metadata table applies.
pub fn check_set_name(name: &str) -> io::Result<()> {
    onelf_format::rules::RuleId::set_name(name)
        .map(|_| ())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("set {name}: {e}")))
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
/// the build's contents out of their own bundle. A hint the build's
/// provider index supersedes when it is present: the listing never adds
/// to or contradicts what the index records.
pub const SHIPPED_FILE: &str = ".onelf/sonames";

/// The packed path a set artifact records its verified provider index
/// at. Absent from builds published before indexes existed; those keep
/// answering from [`SHIPPED_FILE`] as before.
pub const PROVIDER_INDEX_FILE: &str = ".onelf/providers";

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
    let uid = rustix::process::getuid().as_raw();
    store_root_with(
        uid,
        std::env::var_os("ONELF_PLATFORM_STORE").map(PathBuf::from),
        || onelf_format::cache_layout::resolve_root(uid, None),
    )
}

/// [`store_root`] over an injected uid, store hint and cache root.
///
/// The hint is the store's location only when `uid` owns it, the rule
/// the runtime applies. An ordinary uid owns a store root that belongs
/// to it or does not exist yet. Root owns a hinted store only when it
/// exists, belongs to uid 0 and is neither group nor other writable,
/// since the name arrived through the invoking user's environment.
/// Otherwise the uid's own cache root holds the store, which for root
/// is the persistent `/var/cache/onelf`.
fn store_root_with(
    uid: u32,
    hint: Option<PathBuf>,
    cache_root: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let owned = |dir: &Path| match fs::metadata(dir) {
        Ok(md) => md.is_dir() && md.uid() == uid && (uid != 0 || md.mode() & 0o022 == 0),
        Err(e) => uid != 0 && e.kind() == io::ErrorKind::NotFound,
    };
    match hint.filter(|dir| !dir.as_os_str().is_empty() && owned(dir)) {
        Some(dir) => Some(dir),
        None => cache_root().map(|root| onelf_format::cache_layout::platform_dir(&root)),
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

/// The loader-namespace names a set's pinned build verifiably exports,
/// or why they cannot be read.
///
/// A build carrying a provider index answers from it: the index is
/// packed inside the pinned build and the build itself has already
/// matched the recipe's pin, so its exports belong to exactly that build
/// whatever name the recipe declares it under. A hand-edited `.onelf/sonames` cannot
/// add to or change them. A build without an index, one published
/// before indexes were written, answers from its recorded sonames as
/// before and needs no republishing. An index that is present but
/// malformed is an error naming the build, never a reason to fall back
/// to the listing.
fn shipped_sonames(set: &SharedSet) -> Result<Vec<String>, String> {
    let build = set_build(set)?;
    if let Some(bytes) = crate::metadata::read_packed_file(&build, PROVIDER_INDEX_FILE)
        .map_err(|e| format!("{}: {e}", build.display()))?
    {
        let index = onelf_format::provider_index::ProviderIndex::decode(&bytes)
            .map_err(|e| format!("{}: {e}", build.display()))?;
        return Ok(index.exports);
    }
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

/// The loader-namespace names the ELF objects in `dir` verifiably
/// export: the `DT_SONAME` of every object, parsed from the bytes that
/// get packed. An object that exports no soname, a private plugin say,
/// contributes nothing, however its basename reads.
pub(crate) fn verified_exports(dir: &Path) -> io::Result<Vec<String>> {
    let mut names: Vec<String> = jwalk::WalkDir::new(dir)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| super::elf::parse_soname(&e.path()))
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Encode the provider index of the set `name` from the verified
/// objects `dir` carries. The packer injects the bytes as
/// `.onelf/providers`, so the source tree is never written to and the
/// index describes exactly the build that is packed: the audit has
/// already dropped what it cannot load by the time this runs.
pub(crate) fn encode_provider_index(dir: &Path, name: &str) -> io::Result<Vec<u8>> {
    let index = onelf_format::provider_index::ProviderIndex::new(name, &verified_exports(dir)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    index
        .encode()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Check a just-published index against the set and verified exports
/// the packed artifact must have: the index has to name the set it was
/// published as, and its exports have to be exactly what the artifact's
/// objects export. Anything else is a bounded failure, never a hint.
fn check_index(
    index: &onelf_format::provider_index::ProviderIndex,
    name: &str,
    exports: &[String],
) -> io::Result<()> {
    if index.set != name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("provider index names set {}, not {name}", index.set),
        ));
    }
    if index.exports != exports {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider index does not match the exports the set's objects verify",
        ));
    }
    Ok(())
}

/// Read the packed set artifact at `output` back and verify it carries
/// the provider index of `name` with exactly the `exports` its objects
/// verifiably export, then return its hash to pin. Called after
/// packing, so the pin is only ever printed for a build whose recorded
/// providers were checked rather than taken on faith.
pub(crate) fn verify_set_artifact(
    output: &Path,
    name: &str,
    exports: &[String],
) -> io::Result<String> {
    let hash = crate::sysroot_cmd::hash_file(output)?;
    let bytes =
        crate::metadata::read_packed_file(output, PROVIDER_INDEX_FILE)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: the packed set records no provider index",
                    output.display()
                ),
            )
        })?;
    let index = onelf_format::provider_index::ProviderIndex::decode(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", output.display()),
        )
    })?;
    check_index(&index, name, exports)?;
    Ok(hash)
}

/// The sonames a set-only build's pinned sets supply, split out of the
/// verifier's findings and reported as theirs.
///
/// Each set's pinned build is read for its provider index, falling back
/// to its `.onelf/sonames` record when it predates indexes, fetched
/// first when it is not at hand, so a name is attributed to a set only
/// when that set's build ships it. A set whose build cannot be had is
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

/// One declared set as a provider of loader-namespace names: the set's
/// canonical declaration name, the full pin of the build the exports
/// were read from, and the sonames that build verifiably exports, each
/// with the content hash of the object exporting it. The pin and the
/// per-export hashes are what the collision rule compares: two builds
/// whose shared export hashes the same are one provider of it, and two
/// whose hashes differ cannot be selected together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetProvider {
    /// The set declaration name.
    pub set: String,
    /// The BLAKE3 pin of the build the exports belong to.
    pub pin: String,
    /// The loader-namespace names the pinned build verifiably ships,
    /// read from its provider index, or from its recorded listing when
    /// it publishes no index. This is what answers whether a selection
    /// supplies a soname; it needs no content hashes, so every set
    /// whose build could be read carries it.
    pub provides: Vec<String>,
    /// The sonames the pinned build exports with their content hashes,
    /// sorted by soname and unique. Filled only for the sets an export
    /// overlap makes comparable, since a hash is what the collision
    /// rule weighs and nothing else.
    pub exports: Vec<SetExport>,
}

/// One exported soname and the BLAKE3 content hash of the object that
/// exports it, as verified out of a set's pinned build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetExport {
    /// The soname the object exports.
    pub soname: String,
    /// BLAKE3 of the object's bytes.
    pub blake3: [u8; 32],
}

/// The sonames a set's pinned build exports with their content hashes,
/// read out of the pinned artifact itself.
///
/// The build must carry a provider index: it is the authority for what
/// the build exports, it is bound to the pin the recipe pins, and a
/// listing alone does not establish what an export's bytes are,
/// so a build without one is refused here rather than taken on its
/// recorded word. Every payload object the artifact carries is then
/// decompressed and verified against its recorded content hash, and the
/// sonames its objects export must be exactly the ones the index
/// records, so the hashes always describe the exports the index names.
pub(crate) fn verified_export_hashes(set: &SharedSet) -> Result<Vec<SetExport>, String> {
    let build = set_build(set)?;
    let bytes = crate::metadata::read_packed_file(&build, PROVIDER_INDEX_FILE)
        .map_err(|e| format!("{}: {e}", build.display()))?
        .ok_or_else(|| {
            format!(
                "{} records no provider index, so its exports cannot be checked",
                build.display()
            )
        })?;
    let index = onelf_format::provider_index::ProviderIndex::decode(&bytes)
        .map_err(|e| format!("{}: {e}", build.display()))?;

    let (footer, manifest) = crate::info::read_footer_and_manifest(&build)
        .map_err(|e| format!("{}: {e}", build.display()))?;
    let mut file = fs::File::open(&build).map_err(|e| format!("{}: {e}", build.display()))?;
    let codec = onelf_format::payload::Codec::open(&file, &footer)
        .map_err(|e| format!("{}: {e}", build.display()))?;
    let mut found: Vec<SetExport> = Vec::new();
    for (at, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != onelf_format::EntryKind::File {
            continue;
        }
        if !manifest.entry_path(at).contains(".so") {
            continue;
        }
        let data = crate::extract::decompress_verified(&mut file, &footer, entry, &codec)
            .map_err(|e| format!("{}: {}: {e}", build.display(), manifest.entry_path(at)))?;
        let Ok(elf) = goblin::elf::Elf::parse(&data) else {
            continue;
        };
        let Some(soname) = elf.soname else {
            continue;
        };
        if soname.is_empty() || soname.contains('/') {
            continue;
        }
        found.push(SetExport {
            soname: soname.to_string(),
            blake3: entry.content_hash,
        });
    }
    found.sort_by(|a, b| (&a.soname, &a.blake3).cmp(&(&b.soname, &b.blake3)));
    found.dedup();
    for pair in found.windows(2) {
        if pair[0].soname == pair[1].soname {
            return Err(format!(
                "{} exports {} from objects with different bytes",
                build.display(),
                pair[0].soname
            ));
        }
    }
    let scanned: Vec<String> = found.iter().map(|e| e.soname.clone()).collect();
    if scanned != index.exports {
        return Err(format!(
            "{}: the provider index records {:?}, and its objects export {:?}",
            build.display(),
            index.exports,
            scanned
        ));
    }
    Ok(found)
}

/// Verified reads over one declared set's pinned build.
///
/// The build is resolved through the same pinned path a launch uses,
/// matched against its pin before anything is read, and every entry is
/// decompressed through the container's verified path, so what a caller
/// prepares from this is exactly the bytes the pin names and nothing
/// else. A build that cannot be had on those terms is an error naming
/// the build, never a reason to read a path on its recorded word.
pub(crate) struct VerifiedSet {
    set: String,
    build: PathBuf,
    file: fs::File,
    footer: onelf_format::Footer,
    manifest: onelf_format::Manifest,
    codec: onelf_format::payload::Codec,
    files: HashMap<String, usize>,
}

impl VerifiedSet {
    /// Open one set's pinned build for verified reads.
    pub(crate) fn open(set: &SharedSet) -> Result<Self, String> {
        let build = set_build(set)?;
        let (footer, manifest) = crate::info::read_footer_and_manifest(&build)
            .map_err(|e| format!("{}: {e}", build.display()))?;
        let file = fs::File::open(&build).map_err(|e| format!("{}: {e}", build.display()))?;
        let codec = onelf_format::payload::Codec::open(&file, &footer)
            .map_err(|e| format!("{}: {e}", build.display()))?;
        let files = manifest
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == onelf_format::EntryKind::File)
            .map(|(at, _)| (manifest.entry_path(at), at))
            .collect();
        Ok(Self {
            set: set.name.clone(),
            build,
            file,
            footer,
            manifest,
            codec,
            files,
        })
    }

    /// The set declaration name the build belongs to.
    pub(crate) fn set(&self) -> &str {
        &self.set
    }

    /// The build the reads come from, for diagnostics.
    pub(crate) fn build(&self) -> &Path {
        &self.build
    }

    /// The paths of the regular files the build carries, in manifest
    /// order. Directories and links are not listed, so a caller walking
    /// a declared prefix sees only what it can read back verified.
    pub(crate) fn file_paths(&self) -> Vec<String> {
        self.manifest
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == onelf_format::EntryKind::File)
            .map(|(at, _)| self.manifest.entry_path(at))
            .collect()
    }

    /// The verified bytes of one regular file the build carries, or
    /// `None` when it carries no such path. The bytes are decompressed
    /// and matched against the manifest's recorded content hash before
    /// they are returned, so a corrupted payload is an error naming the
    /// build and the path, never a partial answer.
    pub(crate) fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(&at) = self.files.get(path) else {
            return Ok(None);
        };
        let entry = &self.manifest.entries[at];
        crate::extract::decompress_verified(&mut self.file, &self.footer, entry, &self.codec)
            .map(Some)
            .map_err(|e| format!("{}: {path}: {e}", self.build.display()))
    }
}

/// The providers a provider audit over `sets` starts from, with the
/// sets whose exports could not be verified and the reason.
///
/// Names always come from the builds themselves. Content hashes are
/// resolved only for the sets whose exports overlap another set's,
/// because those are the only comparisons a collision rule makes, so a
/// selection whose sets export disjoint sonames never decompresses a
/// byte of their payloads. A set involved in an overlap whose exports
/// cannot be verified claims nothing, the way a build that cannot be
/// had does, and keeps its existing reporting.
pub(crate) fn audit_providers(sets: &[SharedSet]) -> (Vec<SetProvider>, Vec<(String, String)>) {
    let mut named: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut unchecked: Vec<(String, String)> = Vec::new();
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for set in sets {
        match shipped_sonames(set) {
            Ok(mut exports) => {
                exports.sort();
                exports.dedup();
                for name in &exports {
                    *counts.entry(name.clone()).or_default() += 1;
                }
                named.push((set.name.clone(), set.blake3.clone(), exports));
            }
            Err(why) => unchecked.push((set.name.clone(), why)),
        }
    }
    let mut providers: Vec<SetProvider> = Vec::new();
    for (name, pin, exports) in named {
        let shared = exports.iter().any(|soname| counts[soname] > 1);
        if !shared {
            providers.push(SetProvider {
                set: name,
                pin,
                provides: exports,
                exports: Vec::new(),
            });
            continue;
        }
        let set = sets.iter().find(|s| s.name == name);
        match set.map(verified_export_hashes) {
            Some(Ok(hashes)) => providers.push(SetProvider {
                set: name,
                pin,
                provides: hashes.iter().map(|e| e.soname.clone()).collect(),
                exports: hashes,
            }),
            Some(Err(why)) => unchecked.push((name, why)),
            None => unreachable!("the names came from this list"),
        }
    }
    providers.sort_by(|a, b| a.set.cmp(&b.set));
    (providers, unchecked)
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
/// the AppDir, with the hash of the bytes it wrote to each. The next
/// build judges every listed path by content before it copies anything:
/// its own unchanged output goes, so a changed policy or closure takes
/// effect, while a file holding something else was edited by the
/// publisher and stops the build instead of being deleted or packed.
/// Never packed.
pub const GENERATED_FILE: &str = ".onelf/generated";

/// First line of the versioned ledger format. A record without it is a
/// legacy path-only list from a build that recorded no hashes, and its
/// entries are adopted only with provenance, never deleted on faith.
const GENERATED_LEDGER_HEADER: &str = "generated-v2";

/// The largest ledger read whole, so a hand-mangled record errors out
/// instead of costing unbounded memory.
const GENERATED_LEDGER_LIMIT: u64 = 8 * 1024 * 1024;

/// One entry of the generated-file ledger: a path the build wrote,
/// relative to the AppDir, and the BLAKE3 of the bytes it wrote there
/// when known. An entry without a hash is legacy: what the build last
/// wrote to the path is unknown, so the file can only be judged against
/// an output the current plan reproduces. A pending entry carries the
/// hash of what the build copied before its own ELF rewriting ran, so
/// the bytes on disk may differ from it through no publisher's hand;
/// the entry is judged softly until the staging flow attests the final
/// bytes through [`attest_generated`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeneratedEntry {
    /// The path, relative to the AppDir.
    pub path: String,
    /// The hash of the bytes the build last wrote, when the entry
    /// carries one.
    pub hash: Option<blake3::Hash>,
    /// Whether the hash predates the build's own ELF rewriting, which
    /// makes a difference on disk the build's own doing rather than
    /// proof of an edit.
    pub pending: bool,
}

/// Why a formerly generated path stops publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GeneratedConflict {
    /// The file's bytes differ from the hash the ledger recorded.
    Edited {
        /// The hash of what the build last wrote.
        recorded: blake3::Hash,
        /// The hash of what the path holds now.
        actual: blake3::Hash,
    },
    /// The ledger entry carries no hash and no current output proves
    /// the bytes, so they may hold publisher edits.
    Unproven {
        /// The hash of what the path holds now.
        actual: blake3::Hash,
    },
}

/// A formerly generated path publication stops on: the path, whether
/// the plan in hand would write there again, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeneratedFinding {
    /// The path, relative to the AppDir.
    pub path: String,
    /// Whether the plan in hand would write to the path again.
    pub refreshable: bool,
    /// Why the file is not trusted as generated output.
    pub why: GeneratedConflict,
}

/// What a build may do with a formerly generated path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Nothing is at the path; there is nothing to dispose of.
    Absent,
    /// The bytes match the record. The build may remove the file and
    /// write its own output there when the plan still names the path.
    Dispose,
    /// The record cannot attest these bytes: the entry is pending the
    /// build's own ELF rewriting, or carries no hash and the current
    /// plan reproduces the path. The build re-derives the path, which
    /// is no grounds to stop on, and no license to skip the rewrite.
    Excuse,
    /// The bytes differ from an attested record, or a path-only record
    /// proves nothing and nothing reproduces the path. The file stays
    /// and publication stops until the author resolves it.
    Conflict(GeneratedConflict),
}

/// Judge a formerly generated path by content: the ledger's recorded
/// hash against the hash of what the path holds now, `None` when
/// nothing is there. Shared with host assembly, which can adopt a
/// legacy entry first by setting its hash to the hash of its own
/// staged output when the file provably equals that output, and which
/// attests entries after its own ELF rewriting through
/// [`attest_generated`].
pub(crate) fn decide_generated_disposal(
    record: &GeneratedEntry,
    actual: Option<blake3::Hash>,
) -> Disposition {
    match (&record.hash, actual) {
        (_, None) => Disposition::Absent,
        (None, Some(actual)) => Disposition::Conflict(GeneratedConflict::Unproven { actual }),
        (Some(recorded), Some(actual)) if recorded == &actual => Disposition::Dispose,
        (Some(_), Some(_)) if record.pending => Disposition::Excuse,
        (Some(recorded), Some(actual)) => Disposition::Conflict(GeneratedConflict::Edited {
            recorded: *recorded,
            actual,
        }),
    }
}

/// Read the ledger. A record without the version header is the legacy
/// path-only form; either way a line that names no plain path inside
/// the AppDir was never ours and stays ignored.
pub(crate) fn read_generated(appdir: &Path) -> io::Result<Vec<GeneratedEntry>> {
    let record = appdir.join(GENERATED_FILE);
    let file = match fs::File::open(&record) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.take(GENERATED_LEDGER_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", record.display())))?;
    if text.len() as u64 > GENERATED_LEDGER_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: larger than the {} MiB ledger limit",
                record.display(),
                GENERATED_LEDGER_LIMIT / (1024 * 1024)
            ),
        ));
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    match lines.first() {
        Some(&GENERATED_LEDGER_HEADER) => parse_hashed_ledger(&record, &lines[1..]),
        Some(_) => Ok(lines
            .into_iter()
            .filter(|line| inside_appdir(line))
            .map(|path| GeneratedEntry {
                path: path.to_string(),
                hash: None,
                pending: false,
            })
            .collect()),
        None => Ok(Vec::new()),
    }
}

/// Parse the versioned record: a path per entry, with the hash of what
/// the build wrote there on a `#` line under it. A `#~` line marks a
/// pending entry, one whose hash predates the build's own ELF
/// rewriting. A hash line that has no path to belong to, or a path
/// with two, is a malformed record and a bounded error, never a guess.
fn parse_hashed_ledger(record: &Path, lines: &[&str]) -> io::Result<Vec<GeneratedEntry>> {
    let mut entries: Vec<GeneratedEntry> = Vec::new();
    let mut awaiting: Option<usize> = None;
    for line in lines {
        let (hex, pending) = match line.strip_prefix("#~") {
            Some(hex) => (hex, true),
            None => match line.strip_prefix('#') {
                Some(hex) => (hex, false),
                None => {
                    if inside_appdir(line) {
                        entries.push(GeneratedEntry {
                            path: (*line).to_string(),
                            hash: None,
                            pending: false,
                        });
                        awaiting = Some(entries.len() - 1);
                    } else {
                        awaiting = None;
                    }
                    continue;
                }
            },
        };
        let Some(index) = awaiting else {
            return Err(malformed_ledger(
                record,
                line,
                "a hash line with no path above it",
            ));
        };
        if entries[index].hash.is_some() {
            return Err(malformed_ledger(record, line, "a second hash for one path"));
        }
        let hex = hex.to_ascii_lowercase();
        let owner = entries[index].path.clone();
        entries[index].hash = Some(hex.parse::<blake3::Hash>().map_err(|_| {
            malformed_ledger(record, line, &format!("not a blake3 digest for {owner}"))
        })?);
        entries[index].pending = pending;
        awaiting = None;
    }
    Ok(entries)
}

/// A bounded parse error naming the record and the offending line.
fn malformed_ledger(record: &Path, line: &str, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {}: {why}", record.display(), line),
    )
}

/// Whether `line` names a plain path inside the AppDir: the ledger sits
/// in the AppDir and anyone can edit it, so a line that reaches outside
/// is no file of ours.
fn inside_appdir(line: &str) -> bool {
    !line.is_empty()
        && Path::new(line)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// The BLAKE3 of what `path` holds, under a tag for its kind so a file
/// replaced by a link of the same text still hashes differently.
/// `None` when nothing is there.
pub(crate) fn hash_entry(path: &Path) -> io::Result<Option<blake3::Hash>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let mut hasher = blake3::Hasher::new();
    if meta.file_type().is_symlink() {
        hasher.update(b"link ");
        hasher.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
    } else if meta.is_dir() {
        hasher.update(b"dir");
    } else if meta.is_file() {
        hasher.update(b"file ");
        hasher.update_reader(fs::File::open(path)?)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: not a regular file, link or directory", path.display()),
        ));
    }
    Ok(Some(hasher.finalize()))
}

/// The hash of what this build would write for the sysroot path `rel`,
/// or `None` when it would write nothing there: the sysroot lacks the
/// entry, or flattening folds its link onto itself. This is the
/// provenance a legacy ledger entry is adopted against.
fn expected_output_hash(root: &Path, rel: &str) -> io::Result<Option<blake3::Hash>> {
    let src = match entry_in_root(root, Path::new(rel)) {
        Ok(src) => src,
        Err(_) => return Ok(None),
    };
    let meta = match fs::symlink_metadata(&src) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut hasher = blake3::Hasher::new();
    if meta.file_type().is_symlink() {
        let Some(target) = relink(rel, &fs::read_link(&src)?) else {
            return Ok(None);
        };
        hasher.update(b"link ");
        hasher.update(target.as_os_str().as_encoded_bytes());
    } else if meta.is_file() {
        hasher.update(b"file ");
        hasher.update_reader(fs::File::open(&src)?)?;
    } else {
        return Ok(None);
    }
    Ok(Some(hasher.finalize()))
}

/// The destinations the population of `kept` would replace without
/// the authority to: a path whose existing content differs from what
/// the sysroot supplies and that the ledger does not record as the
/// build's own output, and a path beneath a symlinked tree directory,
/// where the copy would land wherever the link points. A recorded path
/// is left to [`dispose_generated`], which judges it by content. Reads
/// only, so a finding stops the build before anything is written.
fn population_collisions(appdir: &Path, root: &Path, kept: &[String]) -> io::Result<Vec<String>> {
    let recorded: HashSet<String> = read_generated(appdir)?
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    let mut findings = Vec::new();
    for rel in kept {
        let dest_rel = appdir_path(rel);
        if let Some(link) = super::plan::symlinked_ancestor(appdir, Path::new(dest_rel)) {
            findings.push(format!(
                "{dest_rel}: passes through the symlink {}, so the copy would land wherever \
                 it points; replace the link with a directory",
                link.display()
            ));
            continue;
        }
        let dest = appdir.join(dest_rel);
        let Some(actual) = hash_entry(&dest)? else {
            continue;
        };
        if recorded.contains(dest_rel) {
            continue;
        }
        let Some(expected) = expected_output_hash(root, rel)? else {
            continue;
        };
        // A link planned where the tree holds a directory is left
        // alone by the copy, so it replaces nothing.
        let src_is_link = entry_in_root(root, Path::new(rel))
            .ok()
            .and_then(|src| fs::symlink_metadata(src).ok())
            .is_some_and(|m| m.file_type().is_symlink());
        if src_is_link && dest.is_dir() {
            continue;
        }
        if actual != expected {
            findings.push(format!(
                "{dest_rel}: holds content hashing to {actual}, but the sysroot's {rel} \
                 hashes to {expected}; the build would replace it, so remove the existing \
                 path or move it to one the build does not write"
            ));
        }
    }
    findings.sort();
    Ok(findings)
}

/// The bounded error a population collision stops the build with,
/// before anything was written.
fn population_collision_error(findings: &[String]) -> io::Error {
    const SHOWN: usize = 8;
    let mut text = format!(
        "populating from the sysroot stopped before anything was written; {} conflict(s):\n",
        findings.len()
    );
    for finding in findings.iter().take(SHOWN) {
        text.push_str("  ");
        text.push_str(finding);
        text.push('\n');
    }
    if findings.len() > SHOWN {
        text.push_str(&format!("  and {} more\n", findings.len() - SHOWN));
    }
    io::Error::new(io::ErrorKind::InvalidData, text)
}

/// Remove what a previous build generated, judging each recorded path
/// by content before anything is touched: a file whose bytes still
/// match its recorded hash goes, and the directories it emptied go
/// with it, while a file whose bytes differ from an attested hash was
/// edited by the publisher. The edited file stays, and the findings
/// returned stop publication. A pending entry, one whose hash predates
/// the build's own ELF rewriting, cannot attest the bytes on disk: the
/// plan re-derives its path, so the file is removed when the plan no
/// longer names it and left for the copy to replace when it does. A
/// path-only legacy entry is adopted, and its file removed, when the
/// file provably equals the output this build would write for it,
/// which `kept`, the sysroot paths the plan copies, supplies. Any other
/// legacy path proves nothing about its bytes, whether the plan still
/// writes it or not, so it is preserved and stops publication as an
/// ownership conflict rather than being guessed safe to delete or
/// replace. Nothing is removed while any path conflicts.
pub(crate) fn dispose_generated(
    appdir: &Path,
    root: &Path,
    kept: &[String],
) -> io::Result<Vec<GeneratedFinding>> {
    let entries = read_generated(appdir)?;
    if entries.is_empty() {
        let _ = fs::remove_file(appdir.join(GENERATED_FILE));
        return Ok(Vec::new());
    }
    // Where the plan writes: the AppDir path to the sysroot path that
    // supplies it.
    let planned: std::collections::HashMap<&str, &str> = kept
        .iter()
        .map(|rel| (appdir_path(rel), rel.as_str()))
        .collect();
    let mut removals: Vec<String> = Vec::new();
    let mut findings: Vec<GeneratedFinding> = Vec::new();
    for entry in &entries {
        let actual = hash_entry(&appdir.join(&entry.path))?;
        let mut judged = entry.clone();
        // A path-only record proves nothing on its own. When the plan
        // still writes the path and the file equals that output, the
        // entry is adopted as the build's own.
        if judged.hash.is_none()
            && let Some(actual) = actual
            && let Some(rel) = planned.get(entry.path.as_str())
            && let Some(expected) = expected_output_hash(root, rel)?
            && expected == actual
        {
            judged.hash = Some(expected);
        }
        match decide_generated_disposal(&judged, actual) {
            Disposition::Absent => {}
            Disposition::Dispose => removals.push(entry.path.clone()),
            Disposition::Excuse => {
                // The plan re-derives the path. Left in place when the
                // copy overwrites it, removed when nothing will.
                if !planned.contains_key(entry.path.as_str()) {
                    removals.push(entry.path.clone());
                }
            }
            Disposition::Conflict(why) => {
                findings.push(GeneratedFinding {
                    path: entry.path.clone(),
                    refreshable: planned.contains_key(entry.path.as_str()),
                    why,
                });
            }
        }
    }
    if !findings.is_empty() {
        findings.sort_by(|a, b| a.path.cmp(&b.path));
        return Ok(findings);
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for rel in &removals {
        let _ = fs::remove_file(appdir.join(rel));
        if let Some(parent) = Path::new(rel).parent() {
            dirs.push(appdir.join(parent));
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
    fs::remove_file(appdir.join(GENERATED_FILE))?;
    Ok(Vec::new())
}

/// Re-hash every recorded entry from the bytes on disk and clear the
/// pending mark, so each entry attests the bytes the tree now holds.
/// The staging flow calls this after its own ELF rewriting is done,
/// which is the point from which a difference on disk is a publisher's
/// edit rather than the build's own doing, and every later rebuild
/// stops on one instead of excusing it.
pub(crate) fn attest_generated(appdir: &Path) -> io::Result<()> {
    let mut entries = read_generated(appdir)?;
    for entry in &mut entries {
        entry.hash = hash_entry(&appdir.join(&entry.path))?;
        entry.pending = false;
    }
    write_generated(appdir, &entries)
}

/// The error a build stops with when a formerly generated file no
/// longer holds what the ledger recorded: every such path with the
/// recorded state against the actual one, and how the author resolves
/// it. Publication happens only after a build that reports no
/// conflicts, so nothing here is ever packed.
pub(crate) fn generated_conflict_error(findings: &[GeneratedFinding]) -> io::Error {
    const SHOWN: usize = 8;
    let mut text = String::from(
        "generated files were changed since the build that wrote them; publication stops until each is resolved:\n",
    );
    for finding in findings.iter().take(SHOWN) {
        text.push_str("  ");
        text.push_str(&finding_line(finding));
        text.push('\n');
    }
    if findings.len() > SHOWN {
        text.push_str(&format!("  and {} more\n", findings.len() - SHOWN));
    }
    text.push_str(
        "  for each one, remove the file, or move it to a path the build does not generate to keep it as publisher-owned input\n",
    );
    io::Error::new(io::ErrorKind::InvalidData, text)
}

/// One finding as the error spells it: the path, the recorded state
/// against the actual one, and whether the plan would have replaced
/// the file.
fn finding_line(finding: &GeneratedFinding) -> String {
    let replaced = if finding.refreshable {
        ", and this build would have replaced it"
    } else {
        ", and no current output writes it anymore"
    };
    match &finding.why {
        GeneratedConflict::Edited { recorded, actual } => format!(
            "{}: the ledger records blake3 {recorded} but the file hashes to {actual}{replaced}",
            finding.path
        ),
        GeneratedConflict::Unproven { actual } => format!(
            "{}: the ledger entry carries no hash, so the file's blake3 {actual} cannot be proven to be generated output{replaced}",
            finding.path
        ),
    }
}

/// Add `paths`, relative to the AppDir, to the record of generated
/// files, hashing what each path holds so the next build can tell the
/// build's own output from a publisher's edits. Paths recorded before
/// and not named again keep their recorded hash; a path named again is
/// hashed anew, since the build just wrote it. An ELF object is
/// recorded pending: the build's own rewriting may still change its
/// bytes, so the entry attests them only once [`attest_generated`]
/// runs after that rewriting. A path that names no plain location
/// inside the AppDir is not recorded.
pub fn record_generated(appdir: &Path, paths: Vec<String>) -> io::Result<()> {
    let mut merged: std::collections::BTreeMap<String, GeneratedEntry> = read_generated(appdir)?
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    for path in paths {
        if !inside_appdir(&path) {
            continue;
        }
        // An ELF object the build copied is not final until the
        // rewriting has run over it, so its hash attests nothing yet.
        let pending = is_elf(&appdir.join(&path));
        let hash = hash_entry(&appdir.join(&path))?;
        merged.insert(
            path.clone(),
            GeneratedEntry {
                path,
                hash,
                pending,
            },
        );
    }
    let entries: Vec<GeneratedEntry> = merged.into_values().collect();
    write_generated(appdir, &entries)
}

/// Write the ledger: the version header, then a path per entry with the
/// hash of the bytes the build wrote on a `#` line under it, `#~` for a
/// pending entry. Paths sort, so the same tree writes the same record.
fn write_generated(appdir: &Path, entries: &[GeneratedEntry]) -> io::Result<()> {
    let record = appdir.join(GENERATED_FILE);
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::from(GENERATED_LEDGER_HEADER);
    text.push('\n');
    for entry in entries {
        text.push_str(&entry.path);
        text.push('\n');
        if let Some(hash) = entry.hash {
            text.push_str(if entry.pending { "#~" } else { "#" });
            text.push_str(hash.to_hex().as_ref());
            text.push('\n');
        }
    }
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

/// Fixtures the sysroot and discovery tests share: packages in memory
/// and the universe a populate over them would admit, decided by the
/// same closure and pruning calls populate makes.
#[cfg(test)]
pub(crate) mod fixture {
    use std::collections::HashSet;
    use std::path::Path;

    use onelf_sysroot::prune::prune;
    use onelf_sysroot::{Database, Package, Policy, Trace};

    use super::SysrootUniverse;

    /// A package owning `files`, relative to the root.
    pub(crate) fn package(name: &str, files: &[&str]) -> Package {
        Package {
            name: name.to_string(),
            version: "1.0-1".to_string(),
            files: files.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// The universe a populate of `owner` and `optional` admits from the
    /// packages, with the platform line out of the way and the policy
    /// and trace the caller supplies.
    pub(crate) fn populate_universe(
        root: &Path,
        packages: &[Package],
        owner: &str,
        optional: &[String],
        policy: Option<&Policy>,
        trace: Option<&Trace>,
    ) -> SysrootUniverse {
        let mut db = Database::default();
        for package in packages {
            db.insert(package.clone());
        }
        let closure = db.closure(owner, optional);
        let files = db.files_of(&closure);
        let pruned = prune(&files, None, policy, trace, &HashSet::new(), None);
        SysrootUniverse::admit(root, db, &pruned.kept, &HashSet::new(), &closure.packages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onelf_sysroot::Package;

    /// The hinted store is the packer's store only when the uid owns it:
    /// an ordinary uid takes its own or a missing one, and root takes
    /// only a root-owned store no one else can write. Anything else falls
    /// back to the uid's own cache root.
    #[test]
    fn a_hinted_store_is_used_only_when_the_uid_owns_it() {
        use std::os::unix::fs::MetadataExt;
        let mine = std::env::temp_dir().join(format!("onelf-store-hint-{}", std::process::id()));
        fs::create_dir_all(&mine).unwrap();
        let me = fs::metadata(&mine).unwrap().uid();
        let cache = || Some(PathBuf::from("/cache-root/onelf"));
        let fallback = Some(PathBuf::from("/cache-root/onelf/platform"));

        assert_eq!(
            store_root_with(me, Some(mine.clone()), cache),
            Some(mine.clone())
        );
        let missing = mine.join("not-yet");
        assert_eq!(
            store_root_with(me, Some(missing.clone()), cache),
            Some(missing.clone())
        );
        assert_eq!(store_root_with(me + 1, Some(mine.clone()), cache), fallback);
        assert_eq!(store_root_with(me, None, cache), fallback);
        assert_eq!(store_root_with(me, Some(PathBuf::new()), cache), fallback);

        if me != 0 {
            assert_eq!(store_root_with(0, Some(mine.clone()), cache), fallback);
            assert_eq!(store_root_with(0, Some(mine.clone()), || None), None);
        }
        assert_eq!(store_root_with(0, Some(missing), cache), fallback);
        let root_owned = Path::new("/");
        if fs::metadata(root_owned).unwrap().uid() == 0 {
            assert_eq!(
                store_root_with(0, Some(root_owned.to_path_buf()), cache),
                Some(root_owned.to_path_buf())
            );
        }
        let shared = Path::new("/tmp");
        if fs::metadata(shared).is_ok_and(|md| md.uid() == 0 && md.mode() & 0o002 != 0) {
            assert_eq!(
                store_root_with(0, Some(shared.to_path_buf()), cache),
                fallback
            );
        }
        let _ = fs::remove_dir_all(&mine);
    }

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
        let hash = hash_entry(&appdir.join("lib/sub/libfoo.so"))
            .unwrap()
            .unwrap();
        fs::write(
            appdir.join(GENERATED_FILE),
            format!(
                "{GENERATED_LEDGER_HEADER}\n\
                 ../outside/keep\n\
                 lib/../../outside/empty/x\n\
                 {}\n\
                 lib/sub/libfoo.so\n\
                 #{}\n",
                root.join("outside/keep").display(),
                hash.to_hex()
            ),
        )
        .unwrap();

        let conflicts = dispose_generated(&appdir, &root, &[]).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(!appdir.join("lib").exists(), "the emptied directories go");
        assert!(appdir.is_dir(), "the AppDir stays");
        assert!(!appdir.join(GENERATED_FILE).exists(), "the record goes too");
        assert!(root.join("outside/keep").is_file());
        assert!(root.join("outside/empty").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    /// Cleanup judged by content: a generated file still holding the
    /// bytes the record wrote goes, and the directories it emptied go
    /// with it.
    #[test]
    fn an_unchanged_generated_file_is_removed_by_its_recorded_hash() {
        let root = std::env::temp_dir().join(format!("onelf-dispose-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib/sub")).unwrap();
        fs::write(appdir.join("lib/sub/cache.bin"), b"generated").unwrap();
        record_generated(&appdir, vec!["lib/sub/cache.bin".to_string()]).unwrap();

        let conflicts = dispose_generated(&appdir, &root, &[]).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(!appdir.join("lib/sub/cache.bin").exists());
        assert!(!appdir.join("lib").exists(), "the emptied directories go");
        assert!(!appdir.join(GENERATED_FILE).exists(), "the record goes too");
        let _ = fs::remove_dir_all(&root);
    }

    /// A file the publisher placed is not in the record, so the cleanup
    /// never considers it, however much it looks like generated output.
    #[test]
    fn a_publisher_file_survives_the_cleanup_of_generated_ones() {
        let root = std::env::temp_dir().join(format!("onelf-publisher-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib")).unwrap();
        fs::write(appdir.join("lib/generated.so"), b"generated").unwrap();
        fs::write(appdir.join("lib/mine.so"), b"mine").unwrap();
        record_generated(&appdir, vec!["lib/generated.so".to_string()]).unwrap();

        let conflicts = dispose_generated(&appdir, &root, &[]).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(!appdir.join("lib/generated.so").exists());
        assert_eq!(fs::read(appdir.join("lib/mine.so")).unwrap(), b"mine");
        let _ = fs::remove_dir_all(&root);
    }

    /// An obsolete generated file whose bytes differ from the record was
    /// edited by the publisher: it stays, the record stays, and the
    /// finding stops publication naming the recorded and actual state
    /// and how the author resolves it.
    #[test]
    fn an_edited_generated_file_is_kept_and_stops_publication() {
        let root = std::env::temp_dir().join(format!("onelf-edited-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib")).unwrap();
        let path = appdir.join("lib/plugin.so");
        fs::write(&path, b"generated").unwrap();
        record_generated(&appdir, vec!["lib/plugin.so".to_string()]).unwrap();
        let recorded = hash_entry(&path).unwrap().unwrap();
        fs::write(&path, b"publisher edit").unwrap();
        let edited = hash_entry(&path).unwrap().unwrap();

        let conflicts = dispose_generated(&appdir, &root, &[]).unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(!conflicts[0].refreshable);
        assert_eq!(
            conflicts[0].why,
            GeneratedConflict::Edited {
                recorded,
                actual: edited,
            }
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            b"publisher edit",
            "the edit stays"
        );
        assert!(appdir.join(GENERATED_FILE).is_file(), "nothing was mutated");

        let text = generated_conflict_error(&conflicts).to_string();
        assert!(text.contains("lib/plugin.so"), "{text}");
        assert!(text.contains(recorded.to_hex().as_ref()), "{text}");
        assert!(text.contains("remove"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    /// The same edit on a path the plan would write again names the
    /// replacement, so the author knows the build would have overwritten
    /// their work.
    #[test]
    fn an_edited_generated_file_the_plan_would_refresh_names_the_replacement() {
        let root = std::env::temp_dir().join(format!("onelf-refresh-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib")).unwrap();
        let path = appdir.join("lib/libfoo.so");
        fs::write(&path, b"generated").unwrap();
        record_generated(&appdir, vec!["lib/libfoo.so".to_string()]).unwrap();
        fs::write(&path, b"publisher edit").unwrap();

        let kept = vec!["usr/lib/libfoo.so".to_string()];
        let conflicts = dispose_generated(&appdir, &root, &kept).unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0].refreshable);
        assert!(
            generated_conflict_error(&conflicts)
                .to_string()
                .contains("replaced"),
            "the replacement is named"
        );
        assert_eq!(fs::read(&path).unwrap(), b"publisher edit");
        let _ = fs::remove_dir_all(&root);
    }

    /// A path-only legacy record is adopted when the file provably
    /// equals the output the plan reproduces: the entry is judged like a
    /// hashed one and the file goes, so the rebuild starts clean.
    #[test]
    fn a_legacy_record_adopts_a_file_the_plan_reproduces() {
        let root = std::env::temp_dir().join(format!("onelf-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sysroot = root.join("sysroot");
        let appdir = root.join("appdir");
        fs::create_dir_all(sysroot.join("usr/lib")).unwrap();
        fs::create_dir_all(appdir.join("lib/.onelf")).unwrap();
        fs::write(sysroot.join("usr/lib/libfoo.so"), b"same bytes").unwrap();
        fs::write(appdir.join("lib/libfoo.so"), b"same bytes").unwrap();
        fs::create_dir_all(appdir.join(".onelf")).unwrap();
        fs::write(appdir.join(GENERATED_FILE), "lib/libfoo.so\n").unwrap();

        let kept = vec!["usr/lib/libfoo.so".to_string()];
        let conflicts = dispose_generated(&appdir, &sysroot, &kept).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(
            !appdir.join("lib/libfoo.so").exists(),
            "adopted, so it goes"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Without a hash and without an output the plan reproduces, a
    /// legacy entry proves nothing: the file stays and the finding names
    /// it, rather than guessing an unproven file safe to delete.
    #[test]
    fn a_legacy_record_without_provenance_is_kept_and_reported() {
        let root = std::env::temp_dir().join(format!("onelf-unproven-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib/.onelf")).unwrap();
        fs::write(appdir.join("lib/old.so"), b"something").unwrap();
        fs::create_dir_all(appdir.join(".onelf")).unwrap();
        fs::write(appdir.join(GENERATED_FILE), "lib/old.so\n").unwrap();

        let held = hash_entry(&appdir.join("lib/old.so")).unwrap().unwrap();
        let conflicts = dispose_generated(&appdir, &root, &[]).unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(
            conflicts[0].why,
            GeneratedConflict::Unproven { actual: held }
        );
        assert!(appdir.join("lib/old.so").is_file(), "the file stays");
        assert!(appdir.join(GENERATED_FILE).is_file(), "nothing was mutated");
        let text = generated_conflict_error(&conflicts).to_string();
        assert!(text.contains("no hash"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A populated destination already holding different content that
    /// no ledger entry records is the publisher's: the population stops
    /// before the disposal or any copy runs, naming the path and both
    /// hashes, and the file is untouched. Identical content and a path
    /// the ledger records pass the check.
    #[test]
    fn a_publisher_file_at_a_populated_destination_stops_before_writing() {
        let root = std::env::temp_dir().join(format!("onelf-popcollide-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sysroot = root.join("sysroot");
        let appdir = root.join("appdir");
        fs::create_dir_all(sysroot.join("usr/lib")).unwrap();
        fs::create_dir_all(appdir.join("lib")).unwrap();
        fs::write(sysroot.join("usr/lib/libfoo.so.1"), b"sysroot bytes").unwrap();
        fs::write(sysroot.join("usr/lib/libsame.so.1"), b"same bytes").unwrap();
        fs::write(appdir.join("lib/libfoo.so.1"), b"publisher bytes").unwrap();
        fs::write(appdir.join("lib/libsame.so.1"), b"same bytes").unwrap();
        let kept = vec![
            "usr/lib/libfoo.so.1".to_string(),
            "usr/lib/libsame.so.1".to_string(),
        ];
        let findings = population_collisions(&appdir, &sysroot, &kept).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].starts_with("lib/libfoo.so.1:"), "{findings:?}");
        assert!(findings[0].contains("would replace it"), "{findings:?}");
        assert_eq!(
            fs::read(appdir.join("lib/libfoo.so.1")).unwrap(),
            b"publisher bytes"
        );

        record_generated(&appdir, vec!["lib/libfoo.so.1".to_string()]).unwrap();
        assert!(
            population_collisions(&appdir, &sysroot, &kept)
                .unwrap()
                .is_empty(),
            "a recorded path is left to the ledger's disposal"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A legacy path-only entry the plan still writes, whose bytes are
    /// not the output this build reproduces, proves nothing: the file is
    /// preserved and the build stops with an ownership conflict naming
    /// the path, rather than replacing bytes that may be a publisher's.
    #[test]
    fn a_mismatched_legacy_file_the_plan_writes_is_an_ownership_conflict() {
        let root = std::env::temp_dir().join(format!("onelf-legacyedit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sysroot = root.join("sysroot");
        let appdir = root.join("appdir");
        fs::create_dir_all(sysroot.join("usr/lib")).unwrap();
        fs::create_dir_all(appdir.join("lib")).unwrap();
        fs::write(sysroot.join("usr/lib/libfoo.so"), b"current output").unwrap();
        fs::write(appdir.join("lib/libfoo.so"), b"something else").unwrap();
        fs::create_dir_all(appdir.join(".onelf")).unwrap();
        fs::write(appdir.join(GENERATED_FILE), "lib/libfoo.so\n").unwrap();

        let kept = vec!["usr/lib/libfoo.so".to_string()];
        let stale = appdir.join("lib/libfoo.so");
        let conflicts = dispose_generated(&appdir, &sysroot, &kept).unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].path, "lib/libfoo.so");
        assert!(conflicts[0].refreshable, "{conflicts:?}");
        assert!(
            matches!(conflicts[0].why, GeneratedConflict::Unproven { .. }),
            "{conflicts:?}"
        );
        assert_eq!(
            fs::read(&stale).unwrap(),
            b"something else",
            "the unproven file is preserved"
        );
        assert!(appdir.join(GENERATED_FILE).is_file(), "nothing was mutated");
        let _ = fs::remove_dir_all(&root);
    }

    /// The ledger round trips with hashes, parses the legacy path-only
    /// form, and answers a malformed record with a bounded error instead
    /// of a guess.
    #[test]
    fn the_ledger_round_trips_and_rejects_malformed_records() {
        let root = std::env::temp_dir().join(format!("onelf-ledger-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib")).unwrap();
        fs::write(appdir.join("lib/a.so"), b"\x7fELFobject").unwrap();
        std::os::unix::fs::symlink("a.so", appdir.join("lib/b.so")).unwrap();
        record_generated(
            &appdir,
            vec!["lib/a.so".to_string(), "lib/b.so".to_string()],
        )
        .unwrap();
        let link_hash = hash_entry(&appdir.join("lib/b.so")).unwrap().unwrap();
        let a_hash = hash_entry(&appdir.join("lib/a.so")).unwrap().unwrap();
        assert_eq!(
            read_generated(&appdir).unwrap(),
            vec![
                GeneratedEntry {
                    path: "lib/a.so".to_string(),
                    hash: Some(a_hash),
                    pending: true,
                },
                GeneratedEntry {
                    path: "lib/b.so".to_string(),
                    hash: Some(link_hash),
                    pending: true,
                },
            ],
            "an ELF object and the link onto it are pending the rewriting"
        );
        assert!(
            appdir.join(GENERATED_FILE).is_file(),
            "the record stays for the next build"
        );

        fs::write(appdir.join(GENERATED_FILE), "lib/a.so\nlib/b.so\n").unwrap();
        assert_eq!(
            read_generated(&appdir).unwrap(),
            vec![
                GeneratedEntry {
                    path: "lib/a.so".to_string(),
                    hash: None,
                    pending: false,
                },
                GeneratedEntry {
                    path: "lib/b.so".to_string(),
                    hash: None,
                    pending: false,
                },
            ],
            "the legacy path-only form parses"
        );

        fs::write(
            appdir.join(GENERATED_FILE),
            format!("{GENERATED_LEDGER_HEADER}\nlib/a.so\n#nothex\n"),
        )
        .unwrap();
        let err = read_generated(&appdir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("lib/a.so"), "{err}");

        fs::write(
            appdir.join(GENERATED_FILE),
            format!("{GENERATED_LEDGER_HEADER}\n#0123456789abcdef\n"),
        )
        .unwrap();
        assert_eq!(
            read_generated(&appdir).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "a hash line with no path above it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A pending entry, one whose hash the build recorded before its
    /// own ELF rewriting ran, attests nothing: a difference on disk is
    /// excused, so an obsolete object the rewriting touched still goes
    /// and one the plan still writes is left for the copy to replace,
    /// never a false conflict over the build's own doing.
    #[test]
    fn a_pending_entry_is_excused_whatever_the_plan_decides() {
        let appdir = |root: &Path| root.join("appdir");
        let seed = |root: &Path| {
            let appdir = appdir(root);
            fs::create_dir_all(appdir.join("lib")).unwrap();
            let object = appdir.join("lib/rewritten.so");
            fs::write(&object, b"\x7fELFsource").unwrap();
            record_generated(&appdir, vec!["lib/rewritten.so".to_string()]).unwrap();
            fs::write(&object, b"\x7fELFrewritten").unwrap();
            appdir
        };

        // Obsolete: nothing plans the path, so the excused object goes
        // like any other generated output.
        let root = std::env::temp_dir().join(format!("onelf-pending-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = seed(&root);
        let object = appdir.join("lib/rewritten.so");
        let conflicts = dispose_generated(&appdir, &root, &[]).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(!object.exists(), "the excused object still goes");
        let _ = fs::remove_dir_all(&root);

        // Refreshable: the plan writes the path, so the file is left
        // for the copy to replace and nothing conflicts.
        let root = std::env::temp_dir().join(format!("onelf-pending-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = seed(&root);
        let object = appdir.join("lib/rewritten.so");
        let conflicts =
            dispose_generated(&appdir, &root, &["lib/rewritten.so".to_string()]).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(object.is_file(), "the plan replaces it, nothing else may");
        let _ = fs::remove_dir_all(&root);
    }

    /// Attesting after the rewriting pins the final bytes: from then on
    /// a difference on disk is a publisher's edit and stops
    /// publication, and the record carries the final hash for the next
    /// build.
    #[test]
    fn attesting_pins_the_bytes_the_tree_now_holds() {
        let root = std::env::temp_dir().join(format!("onelf-attest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib")).unwrap();
        let object = appdir.join("lib/app.so");
        fs::write(&object, b"\x7fELFsource").unwrap();
        record_generated(&appdir, vec!["lib/app.so".to_string()]).unwrap();
        assert!(
            read_generated(&appdir).unwrap()[0].pending,
            "pending at first"
        );

        fs::write(&object, b"\x7fELFrewritten").unwrap();
        attest_generated(&appdir).unwrap();
        let final_hash = hash_entry(&object).unwrap().unwrap();
        assert_eq!(
            read_generated(&appdir).unwrap(),
            vec![GeneratedEntry {
                path: "lib/app.so".to_string(),
                hash: Some(final_hash),
                pending: false,
            }],
            "the record now attests the rewritten bytes"
        );

        fs::write(&object, b"\x7fELFpublisher edit").unwrap();
        let conflicts = dispose_generated(&appdir, &root, &[]).unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(fs::read(&object).unwrap(), b"\x7fELFpublisher edit");
        let _ = fs::remove_dir_all(&root);
    }

    /// The disposition is a pure function of the recorded and actual
    /// hashes, shared with host assembly.
    #[test]
    fn the_disposition_is_decided_by_content() {
        let recorded = blake3::hash(b"recorded");
        let other = blake3::hash(b"other");
        let entry = |hash: Option<blake3::Hash>, pending: bool| GeneratedEntry {
            path: "lib/x.so".to_string(),
            hash,
            pending,
        };
        assert_eq!(
            decide_generated_disposal(&entry(None, false), None),
            Disposition::Absent
        );
        assert_eq!(
            decide_generated_disposal(&entry(Some(recorded), false), None),
            Disposition::Absent
        );
        assert_eq!(
            decide_generated_disposal(&entry(Some(recorded), false), Some(recorded)),
            Disposition::Dispose
        );
        assert_eq!(
            decide_generated_disposal(&entry(Some(recorded), false), Some(other)),
            Disposition::Conflict(GeneratedConflict::Edited {
                recorded,
                actual: other,
            })
        );
        assert_eq!(
            decide_generated_disposal(&entry(None, false), Some(other)),
            Disposition::Conflict(GeneratedConflict::Unproven { actual: other })
        );
        // A pending entry attests nothing: its bytes may differ through
        // the build's own rewriting, so the difference is excused.
        assert_eq!(
            decide_generated_disposal(&entry(Some(recorded), true), Some(recorded)),
            Disposition::Dispose
        );
        assert_eq!(
            decide_generated_disposal(&entry(Some(recorded), true), Some(other)),
            Disposition::Excuse
        );
    }

    /// The hash tags what a path holds, so a file swapped for a link of
    /// the same text still differs, and a missing path hashes to
    /// nothing.
    #[test]
    fn the_hash_tags_what_a_path_holds() {
        let root = std::env::temp_dir().join(format!("onelf-hash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("dir")).unwrap();
        let file = root.join("file");
        fs::write(&file, b"same text").unwrap();
        std::os::unix::fs::symlink("file", root.join("link")).unwrap();

        assert_eq!(hash_entry(&root.join("absent")).unwrap(), None);
        let file_hash = hash_entry(&file).unwrap().unwrap();
        let link_hash = hash_entry(&root.join("link")).unwrap().unwrap();
        assert_ne!(link_hash, file_hash, "a link is not the file it names");
        let dir_hash = hash_entry(&root.join("dir")).unwrap().unwrap();
        assert_ne!(dir_hash, file_hash);
        fs::write(root.join("other"), b"same text").unwrap();
        assert_eq!(
            hash_entry(&root.join("other")).unwrap().unwrap(),
            file_hash,
            "the same bytes hash the same"
        );
        std::os::unix::fs::symlink("file", root.join("link2")).unwrap();
        assert_eq!(
            hash_entry(&root.join("link2")).unwrap().unwrap(),
            link_hash,
            "the same target hashes the same"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A path recorded again is hashed anew, since the build just wrote
    /// it; a path left from the previous record keeps its recorded hash,
    /// so an edit made after that record still shows up next build.
    #[test]
    fn recording_again_refreshes_only_the_paths_named() {
        let root = std::env::temp_dir().join(format!("onelf-rerecord-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let appdir = root.join("appdir");
        fs::create_dir_all(appdir.join("lib")).unwrap();
        fs::write(appdir.join("lib/a.so"), b"a1").unwrap();
        fs::write(appdir.join("lib/b.so"), b"b1").unwrap();
        record_generated(
            &appdir,
            vec!["lib/a.so".to_string(), "lib/b.so".to_string()],
        )
        .unwrap();
        fs::write(appdir.join("lib/a.so"), b"a2").unwrap();
        fs::write(appdir.join("lib/b.so"), b"b2").unwrap();
        record_generated(&appdir, vec!["lib/a.so".to_string()]).unwrap();

        let tagged = |bytes: &[u8]| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"file ");
            hasher.update(bytes);
            hasher.finalize()
        };
        let entries = read_generated(&appdir).unwrap();
        assert_eq!(
            entries[0].hash,
            Some(tagged(b"a2")),
            "the path named again is hashed anew"
        );
        assert_eq!(
            entries[1].hash,
            Some(tagged(b"b1")),
            "the path left alone keeps its record"
        );
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

    /// A STORED-mode package carrying `files` at their literal paths,
    /// laid out the way a pinned build is, so verified reads run against
    /// a real container a test can tamper with byte by byte.
    fn stored_package(files: &[(&str, &[u8])]) -> Vec<u8> {
        use onelf_format::{Block, Entry, EntryKind, Flags, Footer, Manifest, ManifestHeader};
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

    /// Write one pinned build and the set declaration pinning it by its
    /// whole-file digest.
    fn pinned_build(tag: &str, files: &[(&str, &[u8])]) -> (PathBuf, SharedSet) {
        let dir = std::env::temp_dir().join(format!("onelf-vset-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let bytes = stored_package(files);
        let path = dir.join("set.onelf");
        fs::write(&path, &bytes).unwrap();
        let set = SharedSet {
            name: "audio".to_string(),
            packages: vec!["alsa-lib".to_string()],
            url: format!("file://{}", path.display()),
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        };
        (dir, set)
    }

    /// A pinned build reads back exactly the bytes it carries, lists
    /// them in manifest order, and answers None for a path it does not
    /// carry.
    #[test]
    fn verified_reads_answer_a_pinned_build() {
        let (dir, set) = pinned_build(
            "read",
            &[
                ("share/alsa/alsa.conf", b"conf\n"),
                ("lib/gstreamer-1.0/gst-plugin-scanner", b"#!/bin/sh\n"),
            ],
        );
        let mut read = VerifiedSet::open(&set).unwrap();
        assert_eq!(read.set(), "audio");
        assert_eq!(read.build(), set.url.strip_prefix("file://").unwrap());
        assert_eq!(
            read.file_paths(),
            [
                "share/alsa/alsa.conf",
                "lib/gstreamer-1.0/gst-plugin-scanner"
            ]
        );
        assert_eq!(
            read.read("share/alsa/alsa.conf").unwrap(),
            Some(b"conf\n".to_vec())
        );
        assert_eq!(read.read("share/alsa/absent.conf").unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file on disk that is not the pin is refused before any entry
    /// is read.
    #[test]
    fn a_verified_reader_refuses_a_build_that_is_not_the_pin() {
        let (dir, set) = pinned_build("unpinned", &[("share/alsa/alsa.conf", b"conf\n")]);
        let path = PathBuf::from(set.url.strip_prefix("file://").unwrap());
        let original = fs::read(&path).unwrap();
        let mut flipped = original.clone();
        let at = flipped.len() - onelf_format::FOOTER_SIZE - 1;
        flipped[at] ^= 0xff;
        fs::write(&path, &flipped).unwrap();

        let err = VerifiedSet::open(&set).err().unwrap();
        assert!(err.contains("not the pinned build"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A build re-pinned to tampered bytes passes the pin check and
    /// then fails the verified read: the error names the build and the
    /// path, and never returns partial bytes.
    #[test]
    fn a_verified_read_refuses_tampered_content() {
        let (dir, set) = pinned_build("tampered", &[("share/alsa/alsa.conf", b"conf\n")]);
        let path = PathBuf::from(set.url.strip_prefix("file://").unwrap());
        let original = fs::read(&path).unwrap();
        let mut flipped = original.clone();
        let at = flipped.len() - onelf_format::FOOTER_SIZE - 1;
        flipped[at] ^= 0xff;
        fs::write(&path, &flipped).unwrap();
        let corrupt = SharedSet {
            blake3: blake3::hash(&flipped).to_hex().to_string(),
            ..set
        };

        let mut read = VerifiedSet::open(&corrupt).unwrap();
        let err = read.read("share/alsa/alsa.conf").err().unwrap();
        assert!(err.contains("tampered or corrupt package"), "{err}");
        assert!(err.contains("alsa.conf"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Build a minimal ELF64 shared object whose `DT_SONAME` is
    /// `soname`, so the index derivation runs against real objects
    /// without a toolchain.
    fn soname_object(soname: &str) -> Vec<u8> {
        let strtab = {
            let mut s = vec![0u8];
            s.extend_from_slice(soname.as_bytes());
            s.push(0);
            s
        };
        let dyn_off = 64 + 2 * 56;
        let dyn_size = 4 * 16;
        let str_off = dyn_off + dyn_size;
        let total = str_off + strtab.len();
        let strtab_value = str_off as u64;

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
        elf.extend_from_slice(&2u16.to_le_bytes());
        elf.extend_from_slice(&[0u8; 6]);
        // PT_LOAD covering the whole file, then PT_DYNAMIC.
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
        // DT_STRTAB, DT_STRSZ, DT_SONAME, DT_NULL.
        elf.extend_from_slice(&5u64.to_le_bytes());
        elf.extend_from_slice(&strtab_value.to_le_bytes());
        elf.extend_from_slice(&10u64.to_le_bytes());
        elf.extend_from_slice(&(strtab.len() as u64).to_le_bytes());
        elf.extend_from_slice(&14u64.to_le_bytes());
        elf.extend_from_slice(&1u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&strtab);
        assert_eq!(elf.len(), total);
        elf
    }

    /// The index encoded for a set tree records exactly the sonames the
    /// tree's objects export. A file whose basename merely shares a
    /// string with an exported name, a private plugin say, stays out,
    /// and two objects exporting the same soname record it once.
    #[test]
    fn the_provider_index_records_verified_sonames() {
        let dir = std::env::temp_dir().join(format!("onelf-pindex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("lib/plugins")).unwrap();
        fs::write(
            dir.join("lib/libcraft.so.9.1"),
            soname_object("libcraft.so.9"),
        )
        .unwrap();
        std::os::unix::fs::symlink("libcraft.so.9.1", dir.join("lib/libcraft.so.9")).unwrap();
        fs::write(dir.join("lib/plugins/libcraft.so.9"), b"not an elf object").unwrap();
        fs::write(dir.join("lib/readme"), b"neither").unwrap();
        let bytes = encode_provider_index(&dir, "gtk").unwrap();

        let index = onelf_format::provider_index::ProviderIndex::decode(&bytes).unwrap();
        assert_eq!(index.set, "gtk");
        assert_eq!(index.exports, ["libcraft.so.9"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A published index that disagrees about the set or the exports is
    /// a failure naming the disagreement, never a hint.
    #[test]
    fn a_published_index_must_match_the_verified_exports() {
        let exports = vec!["liba.so.1".to_owned()];
        let index = onelf_format::provider_index::ProviderIndex::new("gtk", &exports).unwrap();

        check_index(&index, "gtk", &exports).unwrap();
        assert!(check_index(&index, "qt", &exports).is_err());
        let other = vec!["libb.so.2".to_owned()];
        assert!(check_index(&index, "gtk", &other).is_err());
        let mut extra = exports.clone();
        extra.push("libb.so.2".to_owned());
        assert!(check_index(&index, "gtk", &extra).is_err());
        assert!(check_index(&index, "gtk", &[]).is_err());
    }

    /// The database and sysroot tree the eligibility tests share: an
    /// application, its library, and a plugin package nothing depends on
    /// and the recipe does not name.
    fn selectable_sysroot(root: &Path) -> Vec<Package> {
        let mut app = fixture::package("app", &["usr/bin/app"]);
        app.depends = vec!["glib".into()];
        let glib = fixture::package(
            "glib",
            &[
                "usr/lib/libglib-2.0.so.0",
                "usr/share/glib-2.0/schemas/gschemas.compiled",
            ],
        );
        let extra = fixture::package("extra", &["usr/lib/gstreamer-1.0/libgstextra.so"]);
        for package in [&app, &glib, &extra] {
            for file in &package.files {
                let path = root.join(file);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&path, b"x").unwrap();
            }
        }
        vec![app, glib, extra]
    }

    /// A plugin package the recipe never selected stays out of the
    /// universe, and naming it explicitly is what brings it in.
    #[test]
    fn an_unselected_optional_package_is_never_expanded() {
        let dir =
            std::env::temp_dir().join(format!("onelf-universe-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        let packages = selectable_sysroot(&root);

        let universe = fixture::populate_universe(&root, &packages, "app", &[], None, None);
        assert_eq!(universe.root(), root);
        assert!(
            !universe.admits("usr/lib/gstreamer-1.0"),
            "the plugin directory of an unselected package is not admitted"
        );
        assert_eq!(
            universe.admitted_under("usr/lib/gstreamer-1.0"),
            Vec::<String>::new(),
            "discovery never walks into an unselected package"
        );
        assert_eq!(
            universe.refusal("usr/lib/gstreamer-1.0/libgstextra.so"),
            Refusal::UnselectedPackage {
                owner: "extra".to_string()
            }
        );
        assert!(
            universe.admits("usr/lib/libglib-2.0.so.0"),
            "the selected closure is admitted"
        );

        let named = vec!["extra".to_string()];
        let universe = fixture::populate_universe(&root, &packages, "app", &named, None, None);
        assert!(
            universe.admits("usr/lib/gstreamer-1.0"),
            "an explicitly selected package is admitted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Content the policy or a trace removed stays out of the universe,
    /// with the refusal naming the selected package that owns it rather
    /// than claiming the source lacks it.
    #[test]
    fn policy_and_trace_pruned_content_is_never_resurrected() {
        let dir =
            std::env::temp_dir().join(format!("onelf-universe-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        let mut packages = selectable_sysroot(&root);
        let mut gst = fixture::package(
            "gst",
            &[
                "usr/lib/gstreamer-1.0/libgstcore.so",
                "usr/lib/gstreamer-1.0/libgstbad.so",
                "usr/share/gstreamer-1.0/taglists.def",
            ],
        );
        gst.depends = vec!["glib".into()];
        for file in &gst.files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"x").unwrap();
        }
        packages.push(gst);
        packages[0].depends.push("gst".into());

        let policy_path = dir.join("policy.txt");
        fs::write(&policy_path, "usr/lib/gstreamer-1.0/libgstbad.so\n").unwrap();
        let policy = Policy::load(&policy_path).unwrap();
        let universe =
            fixture::populate_universe(&root, &packages, "app", &[], Some(&policy), None);
        assert_eq!(
            universe.refusal("usr/lib/gstreamer-1.0/libgstbad.so"),
            Refusal::Pruned {
                owner: "gst".to_string()
            },
            "a policy removal is named as one, not as absence"
        );
        assert_eq!(
            universe.admitted_under("usr/lib/gstreamer-1.0"),
            ["usr/lib/gstreamer-1.0/libgstcore.so"],
            "the surviving members of a pruned directory stay admitted"
        );

        let trace = Trace::from_paths(vec![
            "/usr/bin/app".into(),
            "/usr/lib/gstreamer-1.0/libgstcore.so".into(),
        ]);
        let universe = fixture::populate_universe(&root, &packages, "app", &[], None, Some(&trace));
        assert_eq!(
            universe.refusal("usr/share/gstreamer-1.0/taglists.def"),
            Refusal::Pruned {
                owner: "gst".to_string()
            },
            "a trace removal is named as one too"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file of a package left to the host or a shared set whole is
    /// refused with its owner named, and a path nothing owns is absent
    /// rather than pruned.
    #[test]
    fn host_package_files_and_unowned_paths_are_refused_by_reason() {
        let root = Path::new("/fixture").join("sys");
        let gst = fixture::package(
            "gst",
            &[
                "usr/lib/libgstreamer-1.0.so.0",
                "usr/lib/libnvidia-glcore.so.1",
            ],
        );
        let mut left_out = HashSet::new();
        left_out.insert("usr/lib/libnvidia-glcore.so.1".to_string());
        let kept = vec!["usr/lib/libgstreamer-1.0.so.0".to_string()];
        let universe = SysrootUniverse::admit(
            &root,
            {
                let mut db = Database::default();
                db.insert(gst);
                db
            },
            &kept,
            &left_out,
            &["gst".to_string()],
        );
        assert_eq!(
            universe.refusal("usr/lib/libnvidia-glcore.so.1"),
            Refusal::HostPackage {
                owner: "gst".to_string()
            },
            "the package left to the host is the one named"
        );
        assert!(!universe.admits("usr/lib/libnvidia-glcore.so.1"));
        assert_eq!(universe.refusal("usr/share/nothing"), Refusal::Absent);
    }

    /// A directory is admitted exactly while it holds admitted content:
    /// once everything under it is pruned, the directory is no longer a
    /// discovery root either.
    #[test]
    fn a_directory_is_admitted_only_while_it_holds_admitted_content() {
        let dir =
            std::env::temp_dir().join(format!("onelf-universe-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        let mut packages = selectable_sysroot(&root);
        let mut gst = fixture::package(
            "gst",
            &[
                "usr/lib/gstreamer-1.0/libgstcore.so",
                "usr/lib/gstreamer-1.0/libgstbad.so",
            ],
        );
        gst.depends = vec!["glib".into()];
        for file in &gst.files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"x").unwrap();
        }
        packages.push(gst);

        let policy_path = dir.join("policy.txt");
        fs::write(&policy_path, "usr/lib/gstreamer-1.0/**\n").unwrap();
        let policy = Policy::load(&policy_path).unwrap();
        let universe =
            fixture::populate_universe(&root, &packages, "app", &[], Some(&policy), None);
        assert!(
            !universe.admits("usr/lib/gstreamer-1.0"),
            "a directory whose every file was pruned holds nothing admitted"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
